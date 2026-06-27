//! Stream the emulator's PCM to a SyncTERM-APC-capable terminal as chunked
//! clips (the "option A" pseudo-stream): downmix to mono @22050, accumulate a
//! chunk, encode S16 WAV, base64 it, and emit `Store` + `Load` + `Queue` APCs.
//!
//! Wire format (must match the client shim's parser; reconcile with cterm.c
//! before trusting stock SyncTERM):
//!   ESC _ SyncTERM:C;S;<name>;<base64-wav> ESC \      (cache the clip)
//!   ESC _ SyncTERM:A;Load;S=<slot>;<name>  ESC \      (decode into a slot)
//!   ESC _ SyncTERM:A;Queue;C=<chan>;S=<slot> ESC \    (play on a channel)
//!
//! The mixer plays queued clips back-to-back, so consecutive chunks are gapless.

use std::io::{self, Write};

/// Output sample rate after downmix/decimation. GB melodic content sits well
/// under 11 kHz, so 22050 mono is plenty and a quarter the bytes of stereo/44k.
const OUT_RATE: u32 = 22050;
/// Source rate from the emulator's APU (see audio::AUDIO_SAMPLE_RATE).
const SRC_RATE: u32 = 44100;
/// Channel to play on (0-1 are reserved by SyncTERM for internal music/SFX).
const CHANNEL: u8 = 2;
/// Number of rotating slot/filename pairs to cycle through.
const SLOTS: u8 = 8;

pub struct ApcAudio {
    chunk_samples: usize,   // mono samples per emitted chunk (OUT_RATE based)
    preroll_samples: usize, // buffer this much before the first (burst) emission
    primed: bool,           // false until the pre-roll cushion has been sent
    accum: Vec<i16>,        // mono S16 @ OUT_RATE awaiting emission
    carry: f32,             // leftover mono sample for 2:1 decimation
    have_carry: bool,
    slot: u8,
}

impl ApcAudio {
    /// `chunk_ms` trades latency (≈chunk length) against per-chunk overhead.
    /// `preroll_ms` is the FIFO lead built before playback starts: the client
    /// mixer (shim or SyncTERM) plays queued clips immediately, so the only way
    /// to give it a jitter cushion is to queue this much ahead in one burst,
    /// then stream realtime. Larger = fewer gaps, more audio latency.
    pub fn new(chunk_ms: u32, preroll_ms: u32) -> Self {
        let chunk_samples = ((OUT_RATE as u64 * chunk_ms as u64 / 1000) as usize).max(1);
        let preroll_samples = (OUT_RATE as u64 * preroll_ms as u64 / 1000) as usize;
        Self {
            chunk_samples,
            preroll_samples,
            primed: false,
            accum: Vec::with_capacity(preroll_samples + chunk_samples + 64),
            carry: 0.0,
            have_carry: false,
            slot: 0,
        }
    }

    /// Feed interleaved stereo f32 [-1,1] @ SRC_RATE. Accumulates only (no I/O).
    pub fn push_samples(&mut self, samples: &[f32]) {
        let _ = SRC_RATE; // documents the assumed input rate
        for frame in samples.chunks(2) {
            let l = frame[0];
            let r = frame.get(1).copied().unwrap_or(l);
            let mono = 0.5 * (l + r);
            // 2:1 decimate (average adjacent mono samples) -> OUT_RATE.
            if self.have_carry {
                let avg = 0.5 * (self.carry + mono);
                self.accum.push(f32_to_i16(avg));
                self.have_carry = false;
            } else {
                self.carry = mono;
                self.have_carry = true;
            }
        }
    }

    /// Emit every full chunk currently buffered. Call once per frame; flushes
    /// `out` so audio keeps flowing even when video frames are being skipped.
    pub fn emit_ready<W: Write + ?Sized>(&mut self, out: &mut W) -> io::Result<()> {
        if !self.primed {
            // Hold output until the pre-roll cushion is built, then burst it all
            // out at once so the client FIFO starts with a lead.
            if self.accum.len() < self.preroll_samples {
                return Ok(());
            }
            self.primed = true;
        }
        let mut emitted = false;
        while self.accum.len() >= self.chunk_samples {
            self.emit_chunk(self.chunk_samples, out)?;
            emitted = true;
        }
        if emitted {
            out.flush()?;
        }
        Ok(())
    }

    /// Emit any remaining partial chunk (call on exit).
    pub fn flush<W: Write + ?Sized>(&mut self, out: &mut W) -> io::Result<()> {
        if !self.accum.is_empty() {
            let n = self.accum.len();
            self.emit_chunk(n, out)?;
            out.flush()?;
        }
        Ok(())
    }

    fn emit_chunk<W: Write + ?Sized>(&mut self, n: usize, out: &mut W) -> io::Result<()> {
        let wav = encode_wav_mono(&self.accum[..n], OUT_RATE);
        self.accum.drain(0..n);
        let b64 = base64_encode(&wav);
        let slot = self.slot;
        self.slot = (self.slot + 1) % SLOTS;

        // Store the clip, load it into a slot, queue it on the channel.
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

/// Standard base64 (with `=` padding) — what the shim's base64.b64decode expects.
fn base64_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity((data.len() + 2) / 3 * 4);
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
        assert_eq!(w.len(), 44 + 4 * 2);
    }

    #[test]
    fn chunks_emit_when_full_and_rotate_slots() {
        let mut a = ApcAudio::new(10, 0); // 220-sample chunks, no pre-roll for the test
        // Feed ~30ms of stereo @44100 -> should yield a couple of chunks.
        let stereo = vec![0.25f32; 44100 * 2 * 30 / 1000 * 2];
        a.push_samples(&stereo);
        let mut out = Vec::new();
        a.emit_ready(&mut out).unwrap();
        assert!(out.windows(9).any(|w| w == b"SyncTERM:"), "emitted APCs");
        assert!(out.windows(7).any(|w| w == b"A;Queue"), "queued a clip");
    }
}
