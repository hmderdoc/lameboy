//! Translate the Game Boy's lead pulse channel into ANSI music.
//!
//! Real audio is impossible on a headless BBS host, so instead of playing PCM we
//! emit ANSI-music (MML) sequences that SyncTERM renders with its beeper. It is a
//! monophonic, lossy approximation -- one channel, no percussion -- but it carries
//! the melody.
//!
//! Wire format (consumed by cterm.c): `ESC [ |` introduces SyncTERM ANSI music,
//! then an MML body, terminated by a raw `0x0E`. We always emit `MB` (background)
//! because cterm defaults to foreground music, which would block the terminal until
//! each note finished. Notes are addressed by number with `N<n>`, indexing cterm's
//! `note_frequency[]` table (index 3 = C2, 38 = A4/440Hz, octave blocks of 13).
//!
//! cterm queues notes in a FIFO drained at each note's duration. We emit at a fixed
//! ~150ms cadence with notes sized to ~150ms (T200 L8, legato) so the queue drains
//! as fast as we fill it -- no backlog drift -- while a held tone is re-struck each
//! tick to sustain it. (SyncTERM has also been observed to preempt the queue on a
//! new sequence, which this scheme is equally safe under.)

use std::io::Write;
use std::time::{Duration, Instant};

use gameboy_core::sound::Sound;

/// How often we sample the APU and (re)emit. Notes are sized to match this so the
/// cterm note FIFO neither starves nor backs up.
const TICK: Duration = Duration::from_millis(150);
/// Tempo + default length chosen so 240000/TEMPO/NOTELEN == ~150ms (one TICK).
const TEMPO: u32 = 200;
const NOTELEN: u32 = 8;

pub struct AnsiMusic {
    pub enabled: bool,
    /// Last note we emitted: Some(0) == rest, Some(n) == tone index, None == nothing yet.
    last: Option<u8>,
    last_tick: Instant,
}

impl AnsiMusic {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            last: None,
            last_tick: Instant::now() - TICK,
        }
    }

    /// Sample the APU and emit music. Cheap to call every frame; internally rate
    /// limited to one tick. No-op when disabled.
    pub fn update<W: Write + ?Sized>(&mut self, sound: &Sound, out: &mut W) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last_tick) < TICK {
            return;
        }
        self.last_tick = now;

        let note = lead_notenum(sound);
        if note == 0 {
            // Silence: emit a single rest on the transition into silence; nothing
            // to sustain while quiet.
            if self.last != Some(0) {
                let _ = write!(out, "\x1b[|MBN0\x0e");
                let _ = out.flush();
                self.last = Some(0);
            }
        } else {
            // Tone: (re)strike every tick so a held note sustains.
            let _ = write!(out, "\x1b[|MBMLT{}L{}N{}\x0e", TEMPO, NOTELEN, note);
            let _ = out.flush();
            self.last = Some(note);
        }
    }
}

/// Choose the lead voice (the louder of the two voiced pulse channels) and return
/// its cterm note number, or 0 (rest) when neither is sounding.
fn lead_notenum(sound: &Sound) -> u8 {
    let p1 = sound.pulse1();
    let p2 = sound.pulse2();
    let lead = match (p1.is_voiced(), p2.is_voiced()) {
        (true, true) => {
            if p2.get_output_vol() > p1.get_output_vol() {
                Some(p2)
            } else {
                Some(p1)
            }
        }
        (true, false) => Some(p1),
        (false, true) => Some(p2),
        (false, false) => None,
    };
    match lead {
        Some(ch) => freq_to_notenum(ch.frequency_hz()),
        None => 0,
    }
}

/// Map a tone frequency (Hz) to a cterm `note_frequency[]` index in 3..=71.
/// cterm lays the table out as octave blocks of 13 (12 notes + a 0 separator),
/// starting at index 3 = C2, with index 38 == A4 (440Hz). Returns 0 (rest) for a
/// non-positive frequency.
pub fn freq_to_notenum(hz: f64) -> u8 {
    if hz <= 0.0 {
        return 0;
    }
    // Nearest MIDI semitone, clamped to the table's playable range C2..D#7.
    let midi = (69.0 + 12.0 * (hz / 440.0).log2()).round() as i32;
    let midi = midi.clamp(36, 99);
    let octave = midi / 12 - 1; // scientific octave: C2 -> 2
    let semitone = midi % 12; // 0=C .. 11=B
    let index = 3 + (octave - 2) * 13 + semitone;
    index.clamp(3, 71) as u8
}

#[cfg(test)]
mod tests {
    use super::freq_to_notenum;

    #[test]
    fn maps_reference_pitches_to_cterm_indices() {
        assert_eq!(freq_to_notenum(440.0), 38); // A4
        assert_eq!(freq_to_notenum(65.41), 3); // C2 (bottom of table)
        assert_eq!(freq_to_notenum(261.63), 29); // C4 (middle C)
        assert_eq!(freq_to_notenum(880.0), 51); // A5
    }

    #[test]
    fn never_lands_on_a_zero_separator() {
        // Sweep the audible range; every result must be a real-note index, not one
        // of cterm's 0-Hz separator slots (15, 28, 41, 54, 67) or out of range.
        let seps = [0u8, 1, 2, 15, 28, 41, 54, 67];
        let mut hz = 60.0;
        while hz < 2600.0 {
            let n = freq_to_notenum(hz);
            assert!((3..=71).contains(&n), "out of range at {hz}Hz -> {n}");
            assert!(!seps.contains(&n), "hit separator at {hz}Hz -> {n}");
            hz *= 1.01;
        }
    }

    #[test]
    fn silence_is_rest() {
        assert_eq!(freq_to_notenum(0.0), 0);
        assert_eq!(freq_to_notenum(-5.0), 0);
    }
}
