//! Stream the emulator's PCM to a SyncTERM-APC-capable terminal as a sequence of
//! short clips, kept in sync with wall-clock by a self-correcting timeline.
//!
//! Wire format (per the released cterm SyncTERM:A audio doc):
//!   ESC _ SyncTERM:C;S;<name>;<base64-wav> ESC \         (cache the clip)
//!   ESC _ SyncTERM:A;Load;S=<slot>;<name>  ESC \         (decode into a slot)
//!   ESC _ SyncTERM:A;Queue;C=<chan>;S=<slot> ESC \       (play on a channel)
//!
//! Sync model. The emulator runs locked to wall-clock, so each produced sample
//! already carries a timestamp: its position on the audio timeline *is* the
//! real time it was produced. The terminal drains the channel FIFO at a fixed
//! realtime rate. With a wall-clock producer and a realtime consumer, latency is
//! just the initial buffering delay -- it only *grows* if we keep sending audio
//! the saturated link can't clear. So every tick we send only as much as the
//! schedule asks for and reconcile the rest:
//!
//!   * the producer drifting a little faster than realtime -> declare a slightly
//!     higher WAV sample-rate so the terminal plays the clip back compressed,
//!     absorbing the drift with no gap (the terminal's own resampler does the
//!     work). Bounded by MAX_CORRECTION_PCT, a pitch-perceptibility limit.
//!   * a big surplus from a stall/catch-up burst -> compress at the cap and drop
//!     the part that won't fit, so audio skips once to "now" instead of lagging.
//!
//! The only cushion we hold is the measured jitter of our own emit cadence, not
//! a tuned constant. The terminal's one feedback signal -- the `Update;C=`
//! one-shot `CSI = 7 ; <ch> ; 0 n` fired when the channel drains -- re-anchors
//! the timeline after an underrun.

use std::io::{self, Write};

/// Source rate from the emulator's APU (see audio::AUDIO_SAMPLE_RATE).
const SRC_RATE: u32 = 44100;
/// Channel to play on (0-1 are reserved by SyncTERM for internal music/SFX).
const CHANNEL: u8 = 2;
/// Number of rotating slot/filename pairs to cycle through.
const SLOTS: u8 = 8;
/// Largest playback-rate nudge used to absorb drift, as a percent. This is a
/// psychoacoustic bound (a ~2% pitch shift is hard to notice on lo-fi GB audio),
/// not a tuning knob -- surplus beyond what this can compress is dropped instead.
const MAX_CORRECTION_PCT: f32 = 2.0;

// --- Dual-loop drift controller tuning ---------------------------------------
// The declared playback rate is out_rate*(1 + cor), where cor = clamp(drift_ema
// + cushion_trim). drift_ema is a slow (4s) low-pass of the true producer-vs-
// wall skew and carries the whole steady ~1% correction as a near-DC value;
// cushion_trim is a small deadbanded proportional nudge that walks queued lead
// toward CUSHION_MS. cor is slew-limited so it can never bang rail-to-rail (the
// failure mode of the earlier instantaneous-ratio controller). Drops happen only
// when projected lead breaches a ceiling, refractory-limited -- so steady-state
// drops are ~zero. See the design notes in the repo history.
const MAX_COR: f64 = MAX_CORRECTION_PCT as f64 / 100.0; // hard +/-2% declared-rate cap
const CUSHION_MS: u64 = 100; // queued-lead setpoint
const CUSHION_PRIME_MS: u64 = 90; // silence pre-queued at prime / re-anchor
const DEADBAND_MS: f64 = 25.0; // no cushion trim within +/-25ms of the setpoint
const DRIFT_TAU_MS: f64 = 4000.0; // drift-EMA time constant
const DRIFT_UPDATE_MS: u64 = 200; // drift measurement window (>= a few PCM lumps)
const TRIM_GAIN: f64 = 0.004; // cushion trim per 100ms of past-deadband error
const TRIM_MAX: f64 = 0.006; // cushion-trim magnitude clamp
const SLEW_PER_TICK: f64 = 0.0008; // max change in cor per emit (0.08%)
const CEILING_MS: u64 = 260; // drop when projected lead exceeds this (steady state)
const WARMUP_MS: u64 = 12000; // startup window with a higher drop ceiling
const WARMUP_CEILING_MS: u64 = 500;
const DROP_REFRACTORY_MS: u64 = 400; // minimum spacing between drops
const DROP_TARGET_MS: u64 = 100; // lead to settle to after a drop

/// A snapshot of the stream's health for the on-screen/log diagnostics.
#[derive(Clone, Copy, Default)]
pub struct ApcStats {
    pub lead_ms: u32,      // audio queued ahead of realtime (the live cushion)
    pub drift_pct: f32,    // produced-vs-wall clock skew, signed (the "ratio")
    pub correction_pct: f32, // playback-rate nudge applied on the last clip
    pub drops: u32,        // cumulative clips with dropped (skipped) audio
    pub rate: u32,         // output sample rate in Hz (bandwidth)
}

pub struct ApcAudio {
    out_rate: u32, // output sample rate after decimation (bandwidth lever)
    decim: u32,    // integer SRC_RATE/out_rate decimation factor
    min_send: u64, // smallest clip we bother emitting, ms (avoids tiny WAVs)

    // Decimation accumulator (SRC_RATE mono -> out_rate mono).
    decim_sum: f32,
    decim_cnt: u32,
    accum: Vec<i16>, // mono S16 @ out_rate awaiting emission

    slot: u8,
    primed: bool,
    play_start_ms: u64, // wall-clock anchor for the realtime playback timeline
    emitted_ms: f64,    // realtime audio handed to the terminal since the anchor
                        // (fractional ms; integer truncation here decays lead)

    // Measured cushion: the rolling max gap between emit calls (link jitter).
    last_now_ms: u64,
    jitter_ms: u64,

    // Dual-loop drift controller state.
    cor: f64,                // current applied declared-rate correction c
    drift_ema: f64,          // slow low-pass of the true producer/consumer skew
    drift_win_start_ms: u64, // wall time the current drift window opened
    drift_win_prod0: u64,    // produced_samples snapshot at that window start
    last_drop_ms: u64,       // wall time of the last drop (refractory gate)

    // Diagnostics.
    start_ms: u64,
    produced_samples: u64,
    drops: u32,
    correction_pct: f32,
    lead_ms: u32,
}

impl ApcAudio {
    /// `min_chunk_ms` is the smallest clip we emit (drop granularity / overhead
    /// floor). `out_rate_hint` is the desired output sample rate; it snaps to an
    /// integer divisor of 44100 (lower = less bandwidth, the lever that keeps the
    /// link from saturating in the first place).
    pub fn new(min_chunk_ms: u32, out_rate_hint: u32) -> Self {
        let decim = (SRC_RATE / out_rate_hint.clamp(5512, SRC_RATE)).clamp(1, 8);
        let out_rate = SRC_RATE / decim;
        Self {
            out_rate,
            decim,
            min_send: min_chunk_ms.max(5) as u64,
            decim_sum: 0.0,
            decim_cnt: 0,
            accum: Vec::with_capacity((out_rate as usize / 1000) * 256 + 64),
            slot: 0,
            primed: false,
            play_start_ms: 0,
            emitted_ms: 0.0,
            last_now_ms: 0,
            jitter_ms: 0,
            cor: 0.0,
            drift_ema: 0.0,
            drift_win_start_ms: 0,
            drift_win_prod0: 0,
            last_drop_ms: 0,
            start_ms: 0,
            produced_samples: 0,
            drops: 0,
            correction_pct: 0.0,
            lead_ms: 0,
        }
    }

    /// Feed interleaved stereo f32 [-1,1] @ SRC_RATE. Accumulates only (no I/O).
    pub fn push_samples(&mut self, samples: &[f32]) {
        for frame in samples.chunks(2) {
            let l = frame[0];
            let r = frame.get(1).copied().unwrap_or(l);
            self.decim_sum += 0.5 * (l + r);
            self.decim_cnt += 1;
            if self.decim_cnt >= self.decim {
                let avg = self.decim_sum / self.decim as f32;
                self.accum.push(f32_to_i16(avg));
                self.produced_samples += 1;
                self.decim_sum = 0.0;
                self.decim_cnt = 0;
            }
        }
    }

    /// Ship one reconciled clip for this tick, holding queued lead near a fixed
    /// cushion via a slow drift feed-forward plus a small deadbanded trim, with the
    /// declared rate slew-limited so it can never bang between the +/-2% rails.
    /// `now_ms` is a monotonic wall clock. Call once per game-loop tick.
    pub fn emit_ready<W: Write + ?Sized>(&mut self, out: &mut W, now_ms: u64) -> io::Result<()> {
        // STEP 0 -- prime: anchor the timeline and pre-queue a silence cushion so
        // lead starts at the setpoint instead of riding the underrun edge.
        if !self.primed {
            self.play_start_ms = now_ms;
            self.start_ms = now_ms;
            self.last_now_ms = now_ms;
            self.drift_win_start_ms = now_ms;
            self.drift_win_prod0 = self.produced_samples;
            self.emitted_ms = 0.0;
            self.cor = 0.0;
            self.drift_ema = 0.0;
            self.last_drop_ms = 0;
            self.primed = true;
            self.arm_update(out)?;
            self.emit_silence(CUSHION_PRIME_MS, out)?;
            self.emitted_ms += CUSHION_PRIME_MS as f64;
            out.flush()?;
            return Ok(());
        }

        // STEP 1 -- cadence/jitter now only sizes the smallest clip we bother with.
        let gap = now_ms.saturating_sub(self.last_now_ms);
        self.last_now_ms = now_ms;
        self.jitter_ms = (self.jitter_ms * 15 / 16).max(gap);
        let min_clip_ms = self.min_send.max(self.jitter_ms);

        // STEP 2 -- observe queued lead (the controlled variable).
        let elapsed = now_ms.saturating_sub(self.play_start_ms);
        self.lead_ms = (self.emitted_ms - elapsed as f64).max(0.0) as u32;

        // STEP 3 -- slow loop 1: update the drift EMA over a >=200ms window, so it
        // integrates several PCM lumps and is immune to per-tick quantization.
        let win = now_ms.saturating_sub(self.drift_win_start_ms);
        if win >= DRIFT_UPDATE_MS {
            let prod = self.produced_samples.saturating_sub(self.drift_win_prod0);
            let produced_ms = prod as f64 * 1000.0 / self.out_rate as f64;
            let inst_drift = produced_ms / win as f64 - 1.0;
            let alpha = win as f64 / (DRIFT_TAU_MS + win as f64);
            self.drift_ema += alpha * (inst_drift - self.drift_ema);
            self.drift_win_start_ms = now_ms;
            self.drift_win_prod0 = self.produced_samples;
        }

        // STEP 4 -- slow loop 2: deadbanded proportional cushion trim. Too much
        // buffer (err>0) -> trim>0 -> play faster -> spend it; too little -> stretch.
        let err_ms = self.lead_ms as f64 - CUSHION_MS as f64;
        let trim = if err_ms.abs() <= DEADBAND_MS {
            0.0
        } else {
            let eff = err_ms - err_ms.signum() * DEADBAND_MS;
            (TRIM_GAIN * (eff / 100.0)).clamp(-TRIM_MAX, TRIM_MAX)
        };

        // STEP 5 -- combine, clamp, slew-limit into the applied correction c.
        let c_cmd = (self.drift_ema + trim).clamp(-MAX_COR, MAX_COR);
        let dc = (c_cmd - self.cor).clamp(-SLEW_PER_TICK, SLEW_PER_TICK);
        self.cor = (self.cor + dc).clamp(-MAX_COR, MAX_COR);
        let declared_rate = (self.out_rate as f64 * (1.0 + self.cor)).round() as u32;
        self.correction_pct = (self.cor * 100.0) as f32;

        // STEP 6 -- decide how much to send (structural, independent of cor). Keep
        // emitted tracking elapsed + CUSHION_MS (a FIXED setpoint, not jitter), so
        // lead is a real pre-queued cushion rather than the underrun edge.
        let avail = self.accum.len();
        if avail == 0 {
            return Ok(());
        }
        let avail_ms = avail as u64 * 1000 / self.out_rate as u64;
        let need_ms = (elapsed as f64 + CUSHION_MS as f64 - self.emitted_ms).max(0.0) as u64;
        // Ahead of schedule with only a sliver buffered: wait for a fuller clip.
        if need_ms == 0 && avail_ms < min_clip_ms {
            return Ok(());
        }
        let want_ms = need_ms.max(min_clip_ms);
        let mut send_samples = avail.min((want_ms * self.out_rate as u64 / 1000) as usize);

        // STEP 7 -- drop gate: shed the oldest backlog only when projected lead
        // overshoots the ceiling, at most once per refractory window. A rare
        // stall/burst safety net -- steady-state drift is absorbed by cor, so lead
        // stays near the setpoint and this never fires after warmup.
        let ceiling = if now_ms.saturating_sub(self.start_ms) < WARMUP_MS {
            WARMUP_CEILING_MS
        } else {
            CEILING_MS
        };
        let clip_ms = send_samples as f64 * 1000.0 / declared_rate.max(1) as f64;
        let proj_lead = (self.emitted_ms + clip_ms - elapsed as f64).max(0.0) as u64;
        let can_drop = self.last_drop_ms == 0
            || now_ms.saturating_sub(self.last_drop_ms) >= DROP_REFRACTORY_MS;
        if proj_lead > ceiling && can_drop {
            let excess_ms = proj_lead - DROP_TARGET_MS;
            let drop_samples = avail.min((excess_ms * self.out_rate as u64 / 1000) as usize);
            self.accum.drain(0..drop_samples);
            self.drops += 1;
            self.last_drop_ms = now_ms;
            let avail2 = self.accum.len();
            if avail2 == 0 {
                return Ok(());
            }
            send_samples = avail2.min((want_ms * self.out_rate as u64 / 1000) as usize);
        }

        // STEP 8 -- emit the clip; advance the timeline by the realtime it occupies.
        if send_samples == 0 {
            return Ok(());
        }
        self.emit_chunk(send_samples, declared_rate, out)?;
        let played_ms = send_samples as f64 * 1000.0 / declared_rate.max(1) as f64;
        self.emitted_ms += played_ms;
        out.flush()?;
        Ok(())
    }

    /// Handle the terminal's `CSI = 7 ; <ch> ; 0 n` drain notification (an
    /// underrun): re-anchor the timeline and re-prime the silence cushion so lead
    /// restarts at the setpoint. The drift EMA and correction SURVIVE the
    /// re-anchor, so the first post-anchor clip rides the stable ~1% cor instead
    /// of a fresh ratio. After warmup this path should essentially never fire.
    pub fn notify_drain<W: Write + ?Sized>(&mut self, out: &mut W, now_ms: u64) -> io::Result<()> {
        self.play_start_ms = now_ms;
        self.emitted_ms = 0.0;
        self.lead_ms = 0;
        self.arm_update(out)?;
        self.emit_silence(CUSHION_PRIME_MS, out)?;
        self.emitted_ms += CUSHION_PRIME_MS as f64;
        out.flush()
    }

    /// On exit, flush the channel FIFO so no queued audio plays after the door
    /// returns to the menu (the latency "tail").
    pub fn stop<W: Write + ?Sized>(&mut self, out: &mut W) -> io::Result<()> {
        out.write_all(b"\x1b_SyncTERM:A;Flush;C=")?;
        write_u8_dec(out, CHANNEL)?;
        out.write_all(b"\x1b\\")?;
        out.flush()
    }

    /// Current stream health for diagnostics. `now_ms` matches `emit_ready`.
    pub fn stats(&self, now_ms: u64) -> ApcStats {
        let wall = now_ms.saturating_sub(self.start_ms).max(1);
        let produced_ms = self.produced_samples * 1000 / self.out_rate as u64;
        ApcStats {
            lead_ms: self.lead_ms,
            drift_pct: (produced_ms as f32 / wall as f32 - 1.0) * 100.0,
            correction_pct: self.correction_pct,
            drops: self.drops,
            rate: self.out_rate,
        }
    }

    /// Arm the one-shot drain notification on our channel.
    fn arm_update<W: Write + ?Sized>(&mut self, out: &mut W) -> io::Result<()> {
        out.write_all(b"\x1b_SyncTERM:A;Update;C=")?;
        write_u8_dec(out, CHANNEL)?;
        out.write_all(b"\x1b\\")?;
        Ok(())
    }

    /// Emit one clip of `n` backlog samples at the given declared rate, draining
    /// them from accum. A rate above the true `out_rate` makes the terminal play
    /// the clip back faster (compressed), which is how drift is absorbed.
    fn emit_chunk<W: Write + ?Sized>(&mut self, n: usize, rate: u32, out: &mut W) -> io::Result<()> {
        let wav = encode_wav_mono(&self.accum[..n], rate);
        self.accum.drain(0..n);
        self.send_wav(&wav, out)
    }

    /// Queue `ms` of silence as a real zero-PCM clip at the true rate, pre-buffering
    /// lead at prime / re-anchor without depending on terminal Synth support.
    fn emit_silence<W: Write + ?Sized>(&mut self, ms: u64, out: &mut W) -> io::Result<()> {
        let n = (ms * self.out_rate as u64 / 1000) as usize;
        if n == 0 {
            return Ok(());
        }
        let wav = encode_wav_mono(&vec![0i16; n], self.out_rate);
        self.send_wav(&wav, out)
    }

    /// Store+Load+Queue a ready WAV on the next rotating slot.
    fn send_wav<W: Write + ?Sized>(&mut self, wav: &[u8], out: &mut W) -> io::Result<()> {
        let b64 = base64_encode(wav);
        let slot = self.slot;
        self.slot = (self.slot + 1) % SLOTS;

        out.write_all(b"\x1b_SyncTERM:C;S;g")?;
        write_u8_dec(out, slot)?;
        out.write_all(b";")?;
        out.write_all(&b64)?;
        out.write_all(b"\x1b\\")?;

        out.write_all(b"\x1b_SyncTERM:A;Load;S=")?;
        write_u8_dec(out, slot)?;
        out.write_all(b";g")?;
        write_u8_dec(out, slot)?;
        out.write_all(b"\x1b\\")?;

        out.write_all(b"\x1b_SyncTERM:A;Queue;C=")?;
        write_u8_dec(out, CHANNEL)?;
        out.write_all(b";S=")?;
        write_u8_dec(out, slot)?;
        out.write_all(b"\x1b\\")?;
        Ok(())
    }
}

#[inline]
fn f32_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0) as i16
}

fn write_u8_dec<W: Write + ?Sized>(out: &mut W, n: u8) -> io::Result<()> {
    let mut buf = [0u8; 3];
    let s = {
        let mut i = 3;
        let mut v = n;
        loop {
            i -= 1;
            buf[i] = b'0' + (v % 10);
            v /= 10;
            if v == 0 {
                break;
            }
        }
        &buf[i..]
    };
    out.write_all(s)
}

/// Minimal canonical PCM WAV (RIFF/WAVE, S16) for one mono buffer.
fn encode_wav_mono(samples: &[i16], rate: u32) -> Vec<u8> {
    let ch: u16 = 1;
    let bits: u16 = 16;
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = rate * ch as u32 * (bits / 8) as u32;
    let block_align = ch * (bits / 8);
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&ch.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&byte_rate.to_le_bytes());
    v.extend_from_slice(&block_align.to_le_bytes());
    v.extend_from_slice(&bits.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (with `=` padding) — what the terminal's decoder expects.
fn base64_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b0 = c[0];
        let b1 = *c.get(1).unwrap_or(&0);
        let b2 = *c.get(2).unwrap_or(&0);
        out.push(B64[(b0 >> 2) as usize]);
        out.push(B64[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize]);
        out.push(if c.len() > 1 { B64[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] } else { b'=' });
        out.push(if c.len() > 2 { B64[(b2 & 0x3f) as usize] } else { b'=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack.windows(needle.len()).filter(|w| *w == needle).count()
    }

    /// Push `ms` of full-scale stereo @ SRC_RATE.
    fn push_ms(a: &mut ApcAudio, ms: u32) {
        let frames = (SRC_RATE * ms / 1000) as usize;
        a.push_samples(&vec![0.2f32; frames * 2]);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), b"");
        assert_eq!(base64_encode(b"f"), b"Zg==");
        assert_eq!(base64_encode(b"fo"), b"Zm8=");
        assert_eq!(base64_encode(b"foo"), b"Zm9v");
        assert_eq!(base64_encode(b"foobar"), b"Zm9vYmFy");
    }

    #[test]
    fn wav_header_is_canonical() {
        let w = encode_wav_mono(&[0, 1, -1, 100], 22050);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(&w[36..40], b"data");
        // Declared rate lands in the fmt chunk's sample-rate field (offset 24).
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 22050);
        assert_eq!(w.len(), 44 + 4 * 2);
    }

    #[test]
    fn rate_hint_snaps_to_integer_divisor() {
        assert_eq!(ApcAudio::new(40, 22050).out_rate, 22050); // 2:1
        assert_eq!(ApcAudio::new(40, 11025).out_rate, 11025); // 4:1
        assert_eq!(ApcAudio::new(40, 11025).decim, 4);
    }

    #[test]
    fn emits_and_arms_drain_notify_on_first_tick() {
        let mut a = ApcAudio::new(40, 22050);
        push_ms(&mut a, 60);
        let mut out = Vec::new();
        a.emit_ready(&mut out, 0).unwrap();
        assert!(count(&out, b"A;Queue;C=2") >= 1, "queued a clip");
        assert_eq!(count(&out, b"A;Update;C=2"), 1, "armed the drain notify once");
    }

    #[test]
    fn drain_notification_reanchors_reprimes_and_keeps_drift() {
        let mut a = ApcAudio::new(40, 22050);
        a.drift_ema = 0.011; // a converged estimate that must SURVIVE the re-anchor
        a.cor = 0.010;
        let mut out = Vec::new();
        a.notify_drain(&mut out, 1234).unwrap();
        assert_eq!(a.play_start_ms, 1234, "re-anchored to drain time");
        assert_eq!(a.emitted_ms, CUSHION_PRIME_MS as f64, "re-primed the silence cushion");
        assert_eq!(count(&out, b"A;Update;C=2"), 1, "re-armed the drain notify");
        assert!(count(&out, b"A;Queue;C=2") >= 1, "re-queued a silence clip");
        assert_eq!(a.drift_ema, 0.011, "drift estimate survives the re-anchor");
        assert_eq!(a.cor, 0.010, "correction survives the re-anchor");
    }

    // ---- closed-loop stability harness ------------------------------------
    // Drive the real emit_ready() with a producer running (1+drift) faster than
    // wall-clock, an integer-ms wall clock with cadence jitter, and a terminal
    // that underruns (fires notify_drain) when queued lead hits 0. Because the
    // declared rate IS what the terminal plays at, emitted_ms is an exact account
    // of queued realtime, so lead_ms is the true queued lead -- this measures what
    // the ear cares about (smooth cor, held lead, rare drops), not self-consistent
    // internal bookkeeping. Deterministic (seeded LCG), no device needed.
    struct Sim {
        cor: Vec<f32>,
        lead: Vec<u32>,
        drops: u32,
        underruns: u32,
    }

    fn run_sim(drift: impl Fn(f64) -> f64, secs: u64, seed: u64) -> Sim {
        let mut a = ApcAudio::new(40, 22050);
        let mut out = Vec::new();
        let mut rng = seed | 1;
        let mut rand = || {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (rng >> 40) as f64 / (1u64 << 24) as f64 // [0,1)
        };
        let mut wall = 0.0f64;
        let mut prod_frac = 0.0f64;
        let mut prev_lead = 0u32;
        let (mut cor, mut lead) = (Vec::new(), Vec::new());
        let mut underruns = 0u32;
        let mut next_log = 0.0f64;
        let end = (secs * 1000) as f64;
        while wall <= end {
            let dt = 16.0 + rand() * 12.0; // this tick's wall dt (16-28ms jitter)
            wall += dt;
            let d = drift(wall / 1000.0);
            // Produce (1+d) * dt of SRC audio into the door this tick.
            let src = 44100.0 * (1.0 + d) * dt / 1000.0 + prod_frac;
            let nf = src.floor();
            prod_frac = src - nf;
            a.push_samples(&vec![0.15f32; (nf as usize) * 2]);
            let now = wall.floor() as u64;
            a.emit_ready(&mut out, now).unwrap();
            out.clear();
            if now > 0 && a.lead_ms == 0 && prev_lead > 0 {
                underruns += 1;
                a.notify_drain(&mut out, now).unwrap();
                out.clear();
            }
            prev_lead = a.lead_ms;
            if wall >= next_log {
                let s = a.stats(now);
                cor.push(s.correction_pct);
                lead.push(s.lead_ms);
                next_log += 1000.0;
            }
        }
        Sim { cor, lead, drops: a.drops, underruns }
    }

    fn mean(v: &[f32]) -> f64 {
        v.iter().map(|&x| x as f64).sum::<f64>() / v.len().max(1) as f64
    }
    fn stddev(v: &[f32]) -> f64 {
        let m = mean(v);
        (v.iter().map(|&x| (x as f64 - m).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt()
    }

    #[test]
    fn steady_drift_absorbed_smoothly_no_oscillation() {
        // +1% producer for 60s. The whole point: cor must track ~1% SMOOTHLY (low
        // stddev, no rail-to-rail) -- the exact opposite of attempt B (sd~1.7,
        // +2/-2 sign-flips every second) -- while lead holds a cushion and drops
        // and underruns stay near zero.
        let r = run_sim(|_| 0.01, 60, 0xC0FFEE);
        let warm = 15usize.min(r.cor.len().saturating_sub(1));
        let cor = &r.cor[warm..];
        let lead = &r.lead[warm..];
        let cm = mean(cor);
        let sd = stddev(cor);
        assert!((0.5..=2.0).contains(&cm), "cor should track ~1% drift, mean={cm}");
        assert!(sd < 0.35, "cor must be smooth, not oscillating: stddev={sd}");
        let lm = lead.iter().map(|&x| x as f64).sum::<f64>() / lead.len() as f64;
        assert!((40.0..=200.0).contains(&lm), "lead should hold a cushion, mean={lm}");
        assert!(*lead.iter().min().unwrap() > 0, "lead must not starve to 0 in steady state");
        assert!(r.drops <= 5, "steady sub-cap drift should barely drop: {}", r.drops);
        assert!(r.underruns <= 3, "steady state should rarely underrun: {}", r.underruns);
    }

    #[test]
    fn startup_transient_no_drop_storm() {
        // +6.7% decaying to ~+1% (tau 6s) -- the real startup. cor should ride near
        // the +2% cap while drift exceeds it, then ease down smoothly; drops must
        // NOT climb like attempt B (~5/s). A handful during the transient is ok.
        let r = run_sim(|t| 0.01 + 0.057 * (-t / 6.0).exp(), 45, 0x1234);
        assert!(r.drops < 25, "startup should not drop-storm: {}", r.drops);
        // By the end (well past the transient) cor has eased to the steady band.
        let tail = &r.cor[r.cor.len().saturating_sub(8)..];
        assert!(mean(tail) <= 2.0 + 0.01, "cor eased back under the cap by steady state");
        assert!(stddev(tail) < 0.4, "cor smooth in the settled tail: sd={}", stddev(tail));
    }

    #[test]
    fn stop_flushes_channel() {
        let mut a = ApcAudio::new(40, 22050);
        let mut out = Vec::new();
        a.stop(&mut out).unwrap();
        assert_eq!(count(&out, b"A;Flush;C=2"), 1, "flushed the channel on exit");
    }
}
