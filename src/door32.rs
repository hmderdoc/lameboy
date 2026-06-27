//! DOOR32.SYS dropfile reader.
//!
//! Cross-BBS doors (Synchronet, EleBBS, Mystic, …) don't always hand the door
//! its connection via stdio. The portable convention is the DOOR32.SYS dropfile:
//! the BBS writes it, passes its path on the command line, and line 2 carries an
//! inherited **comm handle** — a socket fd on unix, a Winsock SOCKET on Windows
//! — that the door reads/writes for all terminal I/O. This is how a door talks
//! to a caller on a Windows BBS, where there is no stdio passthrough socket.
//!
//! Format (one value per line):
//!   1  comm type: 0=local, 1=serial, 2=telnet (socket)
//!   2  comm/socket handle
//!   3  baud rate
//!   4  BBS software name/version
//!   5  user record number
//!   6  user real name
//!   7  user handle / alias
//!   8  user security level
//!   9  time left, minutes
//!   10 emulation: 0=ascii, 1=ansi, …
//!   11 current node number

use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Door32 {
    pub comm_type: u8, // 0=local, 1=serial, 2=telnet socket
    pub handle: u64,   // inherited comm handle (fd on unix, SOCKET on Windows)
    pub bbs_id: String,
    pub user_number: u32,
    pub real_name: String,
    pub alias: String,
    pub security: u32,
    pub minutes_left: u32,
    pub emulation: u8,
    pub node: u32,
}

impl Door32 {
    /// The inherited socket handle iff this is a telnet (type 2) connection.
    /// `None` means "no socket — fall back to stdio."
    pub fn socket(&self) -> Option<u64> {
        if self.comm_type == 2 {
            Some(self.handle)
        } else {
            None
        }
    }

    /// A stable per-user key for saves/prefs, mirroring Synchronet's `%4`
    /// (zero-padded user number). Empty if the dropfile had no user number.
    pub fn user_key(&self) -> Option<String> {
        if self.user_number > 0 {
            Some(format!("{:04}", self.user_number))
        } else {
            None
        }
    }
}

/// Parse a DOOR32.SYS file. Returns None if it can't be read or the first two
/// (required) lines don't parse.
pub fn read(path: &Path) -> Option<Door32> {
    parse(&std::fs::read_to_string(path).ok()?)
}

fn parse(text: &str) -> Option<Door32> {
    let mut lines = text.lines();
    let comm_type = lines.next()?.trim().parse().ok()?;
    let handle = lines.next()?.trim().parse().ok()?;
    let mut next_str = || lines.next().unwrap_or("").trim().to_string();
    let _baud = next_str();
    let bbs_id = next_str();
    let user_number = next_str().parse().unwrap_or(0);
    let real_name = next_str();
    let alias = next_str();
    let security = next_str().parse().unwrap_or(0);
    let minutes_left = next_str().parse().unwrap_or(0);
    let emulation = next_str().parse().unwrap_or(0);
    let node = next_str().parse().unwrap_or(0);
    Some(Door32 {
        comm_type,
        handle,
        bbs_id,
        user_number,
        real_name,
        alias,
        security,
        minutes_left,
        emulation,
        node,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_telnet_dropfile() {
        // Typical DOOR32.SYS for a telnet door (CRLF line endings, like a BBS writes).
        let sample = "2\r\n328\r\n0\r\nEleBBS\r\n7\r\nShurato Real\r\nShurato\r\n100\r\n90\r\n1\r\n3\r\n";
        let d = parse(sample).unwrap();
        assert_eq!(d.comm_type, 2);
        assert_eq!(d.handle, 328);
        assert_eq!(d.socket(), Some(328));
        assert_eq!(d.user_number, 7);
        assert_eq!(d.alias, "Shurato");
        assert_eq!(d.node, 3);
        assert_eq!(d.user_key().as_deref(), Some("0007"));
    }

    #[test]
    fn local_mode_has_no_socket() {
        let d = parse("0\n0\n0\nBBS\n1\nSysOp\nSysOp\n255\n1000\n1\n1\n").unwrap();
        assert_eq!(d.socket(), None);
    }
}
