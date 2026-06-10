//! Binary IPC protocol between session daemons and clients, ported from
//! zmx's ipc.zig: each frame is a 5-byte header (1 byte tag + 4 byte
//! little-endian payload length) followed by the payload.

use std::os::fd::RawFd;

use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Tag {
    Input = 0,
    Output = 1,
    Resize = 2,
    Detach = 3,
    DetachAll = 4,
    Kill = 5,
    Info = 6,
    Init = 7,
    History = 8,
    Run = 9,
    Ack = 10,
    /// Daemon -> client at teardown: the shell's exit status, so an
    /// attached client can exit with the session's real code. github #18.
    Exit = 11,
}

impl Tag {
    fn from_u8(b: u8) -> Option<Tag> {
        Some(match b {
            0 => Tag::Input,
            1 => Tag::Output,
            2 => Tag::Resize,
            3 => Tag::Detach,
            4 => Tag::DetachAll,
            5 => Tag::Kill,
            6 => Tag::Info,
            7 => Tag::Init,
            8 => Tag::History,
            9 => Tag::Run,
            10 => Tag::Ack,
            11 => Tag::Exit,
            _ => return None,
        })
    }
}

pub const HEADER_LEN: usize = 5;

/// Upper bound on a single frame's payload. Legitimate frames are PTY chunks
/// (a few KiB) or a `dump_vt` replay (megabytes for a large terminal with
/// deep scrollback); 64 MiB leaves generous headroom while bounding the
/// buffer a hostile/confused peer can force us to allocate. github #10.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

/// History payload format selector (single byte): vt escape stream vs
/// plain text. Encode on the client, decode in the daemon.
pub fn encode_history_format(vt: bool) -> [u8; 1] {
    [u8::from(vt)]
}

pub fn decode_history_format(payload: &[u8]) -> bool {
    payload.first() == Some(&1)
}

pub fn append_frame(buf: &mut Vec<u8>, tag: Tag, payload: &[u8]) {
    buf.push(tag as u8);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
}

pub fn encode_frame(tag: Tag, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    append_frame(&mut buf, tag, payload);
    buf
}

pub fn send(fd: RawFd, tag: Tag, payload: &[u8]) -> std::io::Result<()> {
    util::write_all_retry(fd, &encode_frame(tag, payload), 5000)
}

/// Resize payload: rows then cols, both little-endian u16 (zmx packed struct
/// layout).
pub fn encode_resize(rows: u16, cols: u16) -> [u8; 4] {
    let mut out = [0u8; 4];
    out[..2].copy_from_slice(&rows.to_le_bytes());
    out[2..].copy_from_slice(&cols.to_le_bytes());
    out
}

pub fn decode_resize(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() != 4 {
        return None;
    }
    let rows = u16::from_le_bytes([payload[0], payload[1]]);
    let cols = u16::from_le_bytes([payload[2], payload[3]]);
    Some((rows, cols))
}

/// Exit-status payload: the session shell's exit code, little-endian i32
/// (shell convention: WEXITSTATUS, or 128+signal when signaled).
pub fn encode_exit(code: i32) -> [u8; 4] {
    code.to_le_bytes()
}

pub fn decode_exit(payload: &[u8]) -> Option<i32> {
    Some(i32::from_le_bytes(payload.try_into().ok()?))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub tag: Tag,
    pub payload: Vec<u8>,
}

/// Reassembles frames from a (typically non-blocking) stream socket.
#[derive(Default)]
pub struct FrameBuffer {
    buf: Vec<u8>,
    head: usize,
}

impl FrameBuffer {
    pub fn new() -> FrameBuffer {
        FrameBuffer::default()
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.compact();
        self.buf.extend_from_slice(data);
    }

    /// Reads once from the fd. Returns Ok(0) on EOF; WouldBlock propagates.
    pub fn read_from(&mut self, fd: RawFd) -> std::io::Result<usize> {
        let mut tmp = [0u8; 4096];
        let n = util::read_fd(fd, &mut tmp)?;
        if n > 0 {
            self.feed(&tmp[..n]);
        }
        Ok(n)
    }

    /// Returns the next complete frame, skipping frames with unknown tags.
    /// Errors if a header announces a payload over [`MAX_FRAME_LEN`], so a
    /// peer cannot drive unbounded buffering by claiming a huge length.
    pub fn next(&mut self) -> util::Result<Option<Frame>> {
        loop {
            let avail = &self.buf[self.head..];
            if avail.len() < HEADER_LEN {
                return Ok(None);
            }
            let len = u32::from_le_bytes([avail[1], avail[2], avail[3], avail[4]]) as usize;
            if len > MAX_FRAME_LEN {
                return Err(util::Error(format!(
                    "frame length {len} exceeds maximum {MAX_FRAME_LEN}"
                )));
            }
            if avail.len() < HEADER_LEN + len {
                return Ok(None);
            }
            let tag_byte = avail[0];
            let payload = avail[HEADER_LEN..HEADER_LEN + len].to_vec();
            self.head += HEADER_LEN + len;
            if let Some(tag) = Tag::from_u8(tag_byte) {
                return Ok(Some(Frame { tag, payload }));
            }
        }
    }

    fn compact(&mut self) {
        if self.head > 0 {
            self.buf.drain(..self.head);
            self.head = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Info payload (zmx ipc.Info): fixed 528-byte record.

pub const MAX_CMD_LEN: usize = 256;
pub const MAX_CWD_LEN: usize = 256;
pub const INFO_LEN: usize = 8 + 4 + 2 + 2 + MAX_CMD_LEN + MAX_CWD_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub clients: u64,
    pub pid: i32,
    pub cmd: String,
    pub cwd: String,
}

impl SessionInfo {
    /// The session command as an argv vector. The wire `cmd` joins argv with
    /// NUL so arguments containing spaces survive the round-trip (a plain
    /// space-join would corrupt them on fork). github #18.
    pub fn cmd_argv(&self) -> Vec<String> {
        self.cmd
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Human-readable command for display (argv NUL separators rendered as
    /// spaces).
    pub fn cmd_display(&self) -> String {
        self.cmd.replace('\0', " ")
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(INFO_LEN);
        let cmd = self.cmd.as_bytes();
        let cwd = self.cwd.as_bytes();
        let cmd_len = cmd.len().min(MAX_CMD_LEN);
        let cwd_len = cwd.len().min(MAX_CWD_LEN);
        out.extend_from_slice(&self.clients.to_le_bytes());
        out.extend_from_slice(&self.pid.to_le_bytes());
        out.extend_from_slice(&(cmd_len as u16).to_le_bytes());
        out.extend_from_slice(&(cwd_len as u16).to_le_bytes());
        out.extend_from_slice(&cmd[..cmd_len]);
        out.resize(16 + MAX_CMD_LEN, 0);
        out.extend_from_slice(&cwd[..cwd_len]);
        out.resize(INFO_LEN, 0);
        out
    }

    pub fn decode(payload: &[u8]) -> Option<SessionInfo> {
        if payload.len() != INFO_LEN {
            return None;
        }
        let clients = u64::from_le_bytes(payload[0..8].try_into().ok()?);
        let pid = i32::from_le_bytes(payload[8..12].try_into().ok()?);
        let cmd_len =
            (u16::from_le_bytes(payload[12..14].try_into().ok()?) as usize).min(MAX_CMD_LEN);
        let cwd_len =
            (u16::from_le_bytes(payload[14..16].try_into().ok()?) as usize).min(MAX_CWD_LEN);
        let cmd = String::from_utf8_lossy(&payload[16..16 + cmd_len]).into_owned();
        let cwd_start = 16 + MAX_CMD_LEN;
        let cwd = String::from_utf8_lossy(&payload[cwd_start..cwd_start + cwd_len]).into_owned();
        Some(SessionInfo {
            clients,
            pid,
            cmd,
            cwd,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_roundtrip() {
        assert_eq!(decode_exit(&encode_exit(0)), Some(0));
        assert_eq!(decode_exit(&encode_exit(7)), Some(7));
        assert_eq!(decode_exit(&encode_exit(128 + 9)), Some(137));
        assert_eq!(decode_exit(b""), None);
        assert_eq!(decode_exit(b"abc"), None);
        // And through the frame layer.
        let mut buf = FrameBuffer::new();
        buf.feed(&encode_frame(Tag::Exit, &encode_exit(7)));
        let frame = buf.next().unwrap().unwrap();
        assert_eq!(frame.tag, Tag::Exit);
        assert_eq!(decode_exit(&frame.payload), Some(7));
    }

    #[test]
    fn frame_roundtrip() {
        let mut buf = FrameBuffer::new();
        let mut wire = encode_frame(Tag::Input, b"hello");
        wire.extend_from_slice(&encode_frame(Tag::Detach, b""));
        buf.feed(&wire);
        assert_eq!(
            buf.next().unwrap(),
            Some(Frame {
                tag: Tag::Input,
                payload: b"hello".to_vec()
            })
        );
        assert_eq!(
            buf.next().unwrap(),
            Some(Frame {
                tag: Tag::Detach,
                payload: vec![]
            })
        );
        assert_eq!(buf.next().unwrap(), None);
    }

    #[test]
    fn frame_split_delivery() {
        let wire = encode_frame(Tag::Output, b"abcdef");
        let mut buf = FrameBuffer::new();
        // Deliver one byte at a time; the frame appears only when complete.
        for (i, b) in wire.iter().enumerate() {
            buf.feed(&[*b]);
            if i < wire.len() - 1 {
                assert_eq!(buf.next().unwrap(), None);
            }
        }
        assert_eq!(
            buf.next().unwrap(),
            Some(Frame {
                tag: Tag::Output,
                payload: b"abcdef".to_vec()
            })
        );
    }

    #[test]
    fn oversize_frame_length_is_rejected() {
        // A header claiming a payload past MAX_FRAME_LEN must error rather
        // than buffer toward it. github #10.
        let mut wire = vec![Tag::Output as u8];
        wire.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut buf = FrameBuffer::new();
        buf.feed(&wire);
        assert!(buf.next().is_err());
    }

    #[test]
    fn unknown_tag_skipped() {
        let mut wire = vec![0xEEu8];
        wire.extend_from_slice(&3u32.to_le_bytes());
        wire.extend_from_slice(&b"junk"[..3]);
        wire.extend_from_slice(&encode_frame(Tag::Ack, b"ok"));
        let mut buf = FrameBuffer::new();
        buf.feed(&wire);
        assert_eq!(
            buf.next().unwrap(),
            Some(Frame {
                tag: Tag::Ack,
                payload: b"ok".to_vec()
            })
        );
    }

    #[test]
    fn resize_roundtrip() {
        let bytes = encode_resize(48, 120);
        assert_eq!(decode_resize(&bytes), Some((48, 120)));
        assert_eq!(decode_resize(b"xy"), None);
    }

    #[test]
    fn info_roundtrip() {
        let info = SessionInfo {
            clients: 3,
            pid: 4242,
            cmd: "htop -d 10".to_string(),
            cwd: "/home/user/project".to_string(),
        };
        let bytes = info.encode();
        assert_eq!(bytes.len(), INFO_LEN);
        assert_eq!(SessionInfo::decode(&bytes), Some(info));
    }

    #[test]
    fn cmd_argv_preserves_spaced_arguments() {
        // The wire form NUL-joins argv so an argument with spaces survives a
        // fork instead of being re-split on whitespace. github #18.
        let argv = vec![
            "vim".to_string(),
            "a b.txt".to_string(),
            "--cmd=set ai".to_string(),
        ];
        let info = SessionInfo {
            clients: 0,
            pid: 1,
            cmd: argv.join("\0"),
            cwd: String::new(),
        };
        let decoded = SessionInfo::decode(&info.encode()).unwrap();
        assert_eq!(decoded.cmd_argv(), argv);
        assert_eq!(decoded.cmd_display(), "vim a b.txt --cmd=set ai");
    }

    #[test]
    fn info_truncates_long_fields() {
        let info = SessionInfo {
            clients: 0,
            pid: 1,
            cmd: "x".repeat(400),
            cwd: "y".repeat(300),
        };
        let decoded = SessionInfo::decode(&info.encode()).unwrap();
        assert_eq!(decoded.cmd.len(), MAX_CMD_LEN);
        assert_eq!(decoded.cwd.len(), MAX_CWD_LEN);
    }
}
