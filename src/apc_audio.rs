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
    emitted_ms: u64,    // realtime audio handed to the terminal since the anchor

    // Measured cushion: the rolling max gap between emit calls (link jitter).
    last_now_ms: u64,
    jitter_ms: u64,

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
            emitted_ms: 0,
            last_now_ms: 0,
            jitter_ms: 0,
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

    /// Send one reconciled clip for this tick. `now_ms` is a monotonic wall clock
    /// (e.g. session-start elapsed). Call once per frame.
    pub fn emit_ready<W: Write + ?Sized>(&mut self, out: &mut W, now_ms: u64) -> io::Result<()> {
        if !self.primed {
            self.play_start_ms = now_ms;
            self.start_ms = now_ms;
            self.last_now_ms = now_ms;
            self.emitted_ms = 0;
            self.primed = true;
            self.arm_update(out)?;
            out.flush()?;
        }

        // Measured cushion: decaying max of the interval between emit calls.
        let gap = now_ms.saturating_sub(self.last_now_ms);
        self.last_now_ms = now_ms;
        self.jitter_ms = (self.jitter_ms * 15 / 16).max(gap);
        let target = self.min_send.max(self.jitter_ms);

        let elapsed = now_ms.saturating_sub(self.play_start_ms);
        self.lead_ms = self.emitted_ms.saturating_sub(elapsed) as u32;

        // How much realtime audio the schedule wants queued by now, beyond what
        // we've already sent.
        let need_ms = (elapsed + target).saturating_sub(self.emitted_ms);
        let avail = self.accum.len();
        let avail_ms = avail as u64 * 1000 / self.out_rate as u64;
        if need_ms == 0 || avail == 0 {
            return Ok(());
        }
        // Ahead of schedule with only a sliver buffered: wait for a fuller clip.
        if avail_ms < need_ms && avail_ms < self.min_send {
            return Ok(());
        }

        // Work the ratio in samples, not whole ms: at small tick sizes integer-ms
        // resolution is far coarser than the 2% correction band.
        let need_samples = need_ms as f32 * self.out_rate as f32 / 1000.0;
        let max_ratio = 1.0 + MAX_CORRECTION_PCT / 100.0;
        let (send_samples, rate, played_ms, dropped) = if (avail as f32) <= need_samples {
            // Keeping up or behind: send everything at its true rate.
            (avail, self.out_rate, avail_ms, 0usize)
        } else {
            let ratio = avail as f32 / need_samples;
            if ratio <= max_ratio {
                // Small drift: compress all of it into need_ms by declaring a
                // faster rate; the terminal resamples and the drift vanishes.
                let rate = (self.out_rate as f32 * ratio).round() as u32;
                (avail, rate, need_ms, 0)
            } else {
                // Big surplus (stall): compress at the cap, drop the oldest part
                // that still won't fit so playback skips once to the present.
                let keep = ((need_samples * max_ratio) as usize).min(avail);
                let drop = avail - keep;
                let rate = (self.out_rate as f32 * max_ratio).round() as u32;
                (keep, rate, need_ms, drop)
            }
        };

        if dropped > 0 {
            self.accum.drain(0..dropped);
            self.drops += 1;
        }
        self.correction_pct = (rate as f32 / self.out_rate as f32 - 1.0) * 100.0;
        self.emit_chunk(send_samples, rate, out)?;
        self.emitted_ms += played_ms;
        out.flush()?;
        Ok(())
    }

    /// Handle the terminal's `CSI = 7 ; <ch> ; 0 n` drain notification: the FIFO
    /// emptied, so the playback timeline is stale. Re-anchor to "queued == 0 now"
    /// and re-arm the one-shot notification.
    pub fn notify_drain<W: Write + ?Sized>(&mut self, out: &mut W, now_ms: u64) -> io::Result<()> {
        self.play_start_ms = now_ms;
        self.emitted_ms = 0;
        self.lead_ms = 0;
        self.arm_update(out)?;
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

    /// Emit one clip of `n` samples at the given declared sample rate, as
    /// Store+Load+Queue. The declared rate is the resample knob: higher than the
    /// true `out_rate` makes the terminal play the clip back faster (compressed).
    fn emit_chunk<W: Write + ?Sized>(&mut self, n: usize, rate: u32, out: &mut W) -> io::Result<()> {
        let wav = encode_wav_mono(&self.accum[..n], rate);
        self.accum.drain(0..n);
        let b64 = base64_encode(&wav);
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
    fn small_drift_speeds_up_without_dropping() {
        // Compression only acts once the cushion is full, so set up that steady
        // state directly: emitted sits right at the cushion edge (elapsed+target),
        // a small emit gap keeps target at min_send, and we then produce a clip
        // ~1.4% longer than the schedule's per-tick need.
        let mut a = ApcAudio::new(40, 22050);
        let mut out = Vec::new();
        a.emit_ready(&mut out, 0).unwrap(); // prime
        a.last_now_ms = 980;
        a.jitter_ms = 20; // target = min_send(40).max(20) = 40
        a.emitted_ms = 1020; // = elapsed(1000) + target(40) - need(20)
        // need = 20ms = 441 samples @22050; push 448 (~1.6% over) so it compresses.
        a.push_samples(&vec![0.2f32; 896 * 2]);
        a.emit_ready(&mut out, 1000).unwrap();
        let s = a.stats(1000);
        assert_eq!(s.drops, 0, "no drops for sub-2% drift");
        assert!(s.correction_pct > 0.5 && s.correction_pct <= MAX_CORRECTION_PCT + 0.01,
            "applied a small speed-up, got {}", s.correction_pct);
    }

    #[test]
    fn big_surplus_is_dropped_and_capped() {
        // A stall dumps 1s of audio at once with the clock barely advanced.
        let mut a = ApcAudio::new(40, 22050);
        let mut out = Vec::new();
        a.emit_ready(&mut out, 0).unwrap(); // prime
        push_ms(&mut a, 1000);
        a.emit_ready(&mut out, 60).unwrap();
        let s = a.stats(60);
        assert_eq!(s.drops, 1, "dropped the un-absorbable surplus once");
        assert!(s.correction_pct <= MAX_CORRECTION_PCT + 0.01, "speed-up stayed within the cap");
        // Lead is bounded to the measured cushion, not the 1s we produced.
        assert!(s.lead_ms < 200, "lead {} should be bounded", s.lead_ms);
    }

    #[test]
    fn drain_notification_reanchors_and_rearms() {
        let mut a = ApcAudio::new(40, 22050);
        let mut out = Vec::new();
        a.notify_drain(&mut out, 1234).unwrap();
        assert_eq!(a.play_start_ms, 1234, "re-anchored to drain time");
        assert_eq!(a.emitted_ms, 0, "queued-ahead reset to zero");
        assert_eq!(count(&out, b"A;Update;C=2"), 1, "re-armed the drain notify");
    }

    #[test]
    fn stop_flushes_channel() {
        let mut a = ApcAudio::new(40, 22050);
        let mut out = Vec::new();
        a.stop(&mut out).unwrap();
        assert_eq!(count(&out, b"A;Flush;C=2"), 1, "flushed the channel on exit");
    }
}
