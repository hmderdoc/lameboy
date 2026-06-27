//! Decode a raw telnet/ANSI input byte stream into key events.
//!
//! Replaces crossterm's console event reader: over an inherited door socket
//! there is no console to read events from, so we parse the bytes ourselves.
//! Handles telnet IAC negotiation (stripped), arrow keys as `ESC [` / `ESC O`
//! sequences, a lone `ESC` as the Esc key (resolved on the next idle poll), and
//! CR / CRLF / CR-NUL as Enter.
//!
//! There are no key-up events over a socket; callers treat every key as a press
//! and rely on the existing button-release timeout.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
}

// Telnet
const IAC: u8 = 255;
const SE: u8 = 240;
const SB: u8 = 250;
const WILL: u8 = 251;
const WONT: u8 = 252;
const DO: u8 = 253;
const DONT: u8 = 254;

#[derive(Clone, Copy, PartialEq)]
enum State {
    Ground,
    Esc,    // saw ESC, awaiting continuation (or idle -> Key::Esc)
    Csi,    // saw ESC [ , accumulating to a final byte
    Ss3,    // saw ESC O , awaiting one byte
    Iac,    // saw 0xFF
    IacOpt, // saw IAC + WILL/WONT/DO/DONT, awaiting option byte
    IacSub, // inside SB ... , awaiting IAC
    IacSubIac, // inside SB, saw IAC, awaiting SE
}

pub struct KeyDecoder {
    state: State,
    cr_seen: bool,             // swallow the LF/NUL that may follow a CR
    csi: String,               // accumulated CSI parameter bytes
    cursor: Option<(u16, u16)>, // last cursor-position report (row, col)
}

impl Default for KeyDecoder {
    fn default() -> Self {
        Self { state: State::Ground, cr_seen: false, csi: String::new(), cursor: None }
    }
}

impl KeyDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode a chunk of input bytes into zero or more keys.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut out = Vec::new();
        for &b in bytes {
            self.step(b, &mut out);
        }
        out
    }

    /// Call when a poll returned no bytes: resolves a lone trailing ESC into the
    /// Esc key (it wasn't the start of an escape sequence after all).
    pub fn idle(&mut self) -> Option<Key> {
        if self.state == State::Esc {
            self.state = State::Ground;
            Some(Key::Esc)
        } else {
            None
        }
    }

    /// Take the most recent cursor-position report (`ESC [ row ; col R`), if the
    /// terminal answered our size probe since the last call.
    pub fn take_cursor(&mut self) -> Option<(u16, u16)> {
        self.cursor.take()
    }

    fn step(&mut self, b: u8, out: &mut Vec<Key>) {
        match self.state {
            State::Iac => {
                self.state = match b {
                    WILL | WONT | DO | DONT => State::IacOpt,
                    SB => State::IacSub,
                    IAC => State::Ground, // escaped 0xFF data byte; ignore as input
                    _ => State::Ground,   // standalone command (NOP, etc.)
                };
            }
            State::IacOpt => self.state = State::Ground,
            State::IacSub => {
                if b == IAC {
                    self.state = State::IacSubIac;
                }
            }
            State::IacSubIac => {
                self.state = if b == SE { State::Ground } else { State::IacSub };
            }
            State::Esc => match b {
                b'[' => {
                    self.csi.clear();
                    self.state = State::Csi;
                }
                b'O' => self.state = State::Ss3,
                0x1b => out.push(Key::Esc), // previous ESC was lone; stay in Esc for this one
                _ => {
                    out.push(Key::Esc); // previous ESC was lone
                    self.state = State::Ground;
                    self.ground(b, out);
                }
            },
            State::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    match b {
                        b'A' => out.push(Key::Up),
                        b'B' => out.push(Key::Down),
                        b'C' => out.push(Key::Right),
                        b'D' => out.push(Key::Left),
                        b'Z' => out.push(Key::BackTab),
                        b'R' => self.cursor = parse_cursor(&self.csi), // size probe reply
                        _ => {} // Home/End/PgUp/~-sequences etc.: ignored
                    }
                    self.state = State::Ground;
                } else {
                    // parameter / intermediate byte (digits, ';') — accumulate
                    self.csi.push(b as char);
                }
            }
            State::Ss3 => {
                match b {
                    b'A' => out.push(Key::Up),
                    b'B' => out.push(Key::Down),
                    b'C' => out.push(Key::Right),
                    b'D' => out.push(Key::Left),
                    _ => {}
                }
                self.state = State::Ground;
            }
            State::Ground => self.ground(b, out),
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<Key>) {
        match b {
            IAC => self.state = State::Iac,
            0x1b => self.state = State::Esc,
            b'\r' => {
                out.push(Key::Enter);
                self.cr_seen = true;
                return;
            }
            b'\n' | 0x00 => {
                if self.cr_seen {
                    // swallow the LF / NUL that completes a CR-terminated line
                } else if b == b'\n' {
                    out.push(Key::Enter);
                }
            }
            b'\t' => out.push(Key::Tab),
            0x08 | 0x7f => out.push(Key::Backspace),
            0x20..=0x7e => out.push(Key::Char(b as char)),
            _ => {} // other control bytes: ignored
        }
        self.cr_seen = false;
    }
}

/// Parse a CSI cursor-position report body ("row;col") into (row, col).
fn parse_cursor(params: &str) -> Option<(u16, u16)> {
    let mut it = params.split(';');
    let row = it.next()?.parse().ok()?;
    let col = it.next()?.parse().ok()?;
    Some((row, col))
}

/// Reads bytes from a `Term` and decodes them into keys — the input side of the
/// door, replacing crossterm's event reader. `poll` is non-blocking (drain what
/// arrived this frame); `wait` blocks until a key (used by the menu).
pub struct Input {
    decoder: KeyDecoder,
    buf: [u8; 2048],
}

impl Default for Input {
    fn default() -> Self {
        Self { decoder: KeyDecoder::new(), buf: [0u8; 2048] }
    }
}

impl Input {
    pub fn new() -> Self {
        Self::default()
    }

    /// Non-blocking: decode whatever input is available right now. Resolves a
    /// pending lone-ESC into Esc when nothing else arrived. Propagates a closed
    /// connection as an error.
    pub fn poll(&mut self, term: &mut dyn crate::term::Term) -> std::io::Result<Vec<Key>> {
        let n = term.read_available(&mut self.buf)?;
        if n == 0 {
            return Ok(self.decoder.idle().into_iter().collect());
        }
        Ok(self.decoder.feed(&self.buf[..n]))
    }

    /// Block until at least one key is available (or the connection closes,
    /// returning None). Polls with a short sleep; resolves lone-ESC on idle.
    pub fn wait(&mut self, term: &mut dyn crate::term::Term) -> std::io::Result<Option<Key>> {
        let mut empties = 0u32;
        loop {
            let n = term.read_available(&mut self.buf)?;
            if n > 0 {
                let keys = self.decoder.feed(&self.buf[..n]);
                if let Some(k) = keys.into_iter().next() {
                    return Ok(Some(k));
                }
                empties = 0;
            } else {
                // After a couple of idle polls, a held ESC is really the Esc key.
                empties += 1;
                if empties >= 2 {
                    if let Some(k) = self.decoder.idle() {
                        return Ok(Some(k));
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
    }

    pub fn take_cursor(&mut self) -> Option<(u16, u16)> {
        self.decoder.take_cursor()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Key> {
        KeyDecoder::new().feed(bytes)
    }

    #[test]
    fn cursor_position_report_is_captured_not_keyed() {
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b[24;80R"), []); // no key emitted
        assert_eq!(d.take_cursor(), Some((24, 80)));
        assert_eq!(d.take_cursor(), None);
    }

    #[test]
    fn arrows_csi_and_ss3() {
        assert_eq!(keys(b"\x1b[A\x1b[B\x1b[C\x1b[D"), [Key::Up, Key::Down, Key::Right, Key::Left]);
        assert_eq!(keys(b"\x1bOA\x1bOD"), [Key::Up, Key::Left]);
        assert_eq!(keys(b"\x1b[Z"), [Key::BackTab]);
    }

    #[test]
    fn lone_esc_resolves_on_idle() {
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b"), []);
        assert_eq!(d.idle(), Some(Key::Esc));
        assert_eq!(d.idle(), None);
    }

    #[test]
    fn esc_then_char_emits_both() {
        assert_eq!(keys(b"\x1bx"), [Key::Esc, Key::Char('x')]);
    }

    #[test]
    fn enter_handles_cr_crlf_and_crnul() {
        assert_eq!(keys(b"\r"), [Key::Enter]);
        assert_eq!(keys(b"\r\n"), [Key::Enter]);
        assert_eq!(keys(b"\r\x00"), [Key::Enter]);
        assert_eq!(keys(b"\n"), [Key::Enter]);
        assert_eq!(keys(b"a\rb"), [Key::Char('a'), Key::Enter, Key::Char('b')]);
    }

    #[test]
    fn letters_space_tab_backspace() {
        assert_eq!(
            keys(b"hi z\t\x7f\x08"),
            [Key::Char('h'), Key::Char('i'), Key::Char(' '), Key::Char('z'),
             Key::Tab, Key::Backspace, Key::Backspace]
        );
    }

    #[test]
    fn telnet_iac_is_stripped() {
        // IAC WILL ECHO, then 'A'
        assert_eq!(keys(b"\xff\xfb\x01A"), [Key::Char('A')]);
        // IAC SB NAWS ... IAC SE, then 'Z'
        assert_eq!(keys(b"\xff\xfa\x1f\x00\x50\x00\x18\xff\xf0Z"), [Key::Char('Z')]);
    }

    #[test]
    fn sequences_split_across_feeds() {
        let mut d = KeyDecoder::new();
        assert_eq!(d.feed(b"\x1b["), []);
        assert_eq!(d.feed(b"A"), [Key::Up]);
        // ESC split from its '[' across feeds, still becomes an arrow
        assert_eq!(d.feed(b"\x1b"), []);
        assert_eq!(d.feed(b"[D"), [Key::Left]);
    }
}
