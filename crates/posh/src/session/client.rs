//! Attach client: raw-mode tty bridged to a session daemon over the Unix
//! socket (zmx clientLoop port). Detach key: Ctrl-\.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use crate::pty::{self, RawMode};
use crate::session::ipc::{self, FrameBuffer, Tag};
use crate::session::{daemon, Config};
use crate::util::{self, Error, Result};

const STDIN: i32 = libc::STDIN_FILENO;
const STDOUT: i32 = libc::STDOUT_FILENO;

/// Reset sequence written on detach: disable mouse reporting (1000/1002/
/// 1003/1006), bracketed paste (2004), focus events (1004), alt screen
/// (1049), and re-show the cursor. The screen is intentionally not cleared.
const RESTORE_SEQ: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[?1049l\x1b[?25h";

pub fn cmd_attach(
    cfg: &Config,
    name: &str,
    command: Option<Vec<String>>,
    detach_flag: bool,
) -> Result<()> {
    if !detach_flag && std::env::var_os("POSH_SESSION").is_some() {
        return Err(Error::from(
            "cannot attach to a session from within a session",
        ));
    }

    let created = daemon::ensure_session(cfg, name, command)?;
    if detach_flag {
        if created {
            println!("session \"{name}\" created");
        } else {
            println!("session \"{name}\" already exists");
        }
        return Ok(());
    }

    let path = cfg.socket_path(name)?;
    let stream = UnixStream::connect(&path)
        .map_err(|e| Error(format!("connect {}: {e}", path.display())))?;

    let raw = RawMode::enable(STDIN)?;
    // Clean slate before the daemon replays the session state.
    let _ = util::write_fd(STDOUT, b"\x1b[2J\x1b[H");
    let result = client_loop(stream);
    let _ = util::write_fd(STDOUT, RESTORE_SEQ);
    drop(raw);
    result
}

/// The detach key Ctrl-\ as kitty keyboard CSI-u encodings (92 = backslash,
/// 5 = ctrl modifier; with and without the explicit `:1` press-event suffix).
const KITTY_DETACH_SEQS: [&[u8]; 2] = [b"\x1b[92;5u", b"\x1b[92;5:1u"];

enum KittyMatch {
    /// The slice begins with a full detach sequence.
    Full,
    /// The slice is a proper prefix of a detach sequence (need more bytes).
    Partial,
    /// No detach sequence starts here.
    No,
}

fn match_kitty_detach(s: &[u8]) -> KittyMatch {
    let mut partial = false;
    for seq in KITTY_DETACH_SEQS {
        if s.len() >= seq.len() {
            if &s[..seq.len()] == seq {
                return KittyMatch::Full;
            }
        } else if seq.starts_with(s) {
            partial = true;
        }
    }
    if partial {
        KittyMatch::Partial
    } else {
        KittyMatch::No
    }
}

/// Scans the stdin byte stream for the detach key — raw Ctrl-\ (0x1c) at any
/// offset, or its kitty CSI-u encodings — surviving splits across reads by
/// holding back a trailing partial that could still complete the sequence.
#[derive(Default)]
struct DetachMatcher {
    carry: Vec<u8>,
}

impl DetachMatcher {
    /// Returns the bytes to forward to the daemon as input, and whether the
    /// detach key was seen (in which case bytes after it are discarded).
    fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(input);
        let mut forward = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            if b == 0x1c {
                return (forward, true);
            }
            if b == 0x1b {
                match match_kitty_detach(&data[i..]) {
                    KittyMatch::Full => return (forward, true),
                    KittyMatch::Partial => {
                        // Hold back; the rest may arrive on the next read.
                        self.carry = data[i..].to_vec();
                        return (forward, false);
                    }
                    KittyMatch::No => {}
                }
            }
            forward.push(b);
            i += 1;
        }
        (forward, false)
    }
}

fn client_loop(stream: UnixStream) -> Result<()> {
    util::install_client_signal_handlers();
    stream.set_nonblocking(true)?;
    let sock_fd = stream.as_raw_fd();
    util::set_nonblocking(STDIN)?;

    let mut sock_write_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut stdout_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut read_buf = FrameBuffer::new();
    let mut detach = DetachMatcher::default();
    let mut stream_writer = &stream;

    // Announce our terminal size so the daemon can size the PTY.
    let (rows, cols) = pty::term_size(STDOUT);
    ipc::append_frame(
        &mut sock_write_buf,
        Tag::Init,
        &ipc::encode_resize(rows, cols),
    );

    let err_events = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
    loop {
        if util::take_flag(&util::SIGWINCH_RECEIVED) {
            let (rows, cols) = pty::term_size(STDOUT);
            ipc::append_frame(
                &mut sock_write_buf,
                Tag::Resize,
                &ipc::encode_resize(rows, cols),
            );
        }

        if util::take_flag(&util::SIGTERM_RECEIVED) {
            // SIGTERM/SIGINT/SIGHUP: best-effort detach notice, then leave;
            // cmd_attach restores the tty on the way out either way.
            ipc::append_frame(&mut sock_write_buf, Tag::Detach, b"");
            let _ = util::write_all_retry(sock_fd, &sock_write_buf, 100);
            return Ok(());
        }

        if util::take_flag(&util::SIGCONT_RECEIVED) {
            // Resumed after SIGSTOP/fg: re-Init so the daemon replays the
            // screen (and picks up any size change while stopped).
            let (rows, cols) = pty::term_size(STDOUT);
            ipc::append_frame(
                &mut sock_write_buf,
                Tag::Init,
                &ipc::encode_resize(rows, cols),
            );
        }

        let mut fds = vec![util::pollfd(STDIN, libc::POLLIN)];
        let mut sock_events = libc::POLLIN;
        if !sock_write_buf.is_empty() {
            sock_events |= libc::POLLOUT;
        }
        fds.push(util::pollfd(sock_fd, sock_events));
        if !stdout_buf.is_empty() {
            fds.push(util::pollfd(STDOUT, libc::POLLOUT));
        }

        // Bounded timeout: a signal landing between the flag checks above
        // and this poll sets the flag without an EINTR; an infinite poll
        // would then sit raw-mode until unrelated activity. One wakeup per
        // second bounds that race (the remote loop does the same).
        match util::poll(&mut fds, 1000) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }

        // stdin -> daemon
        if fds[0].revents & (libc::POLLIN | err_events) != 0 {
            let mut buf = [0u8; 4096];
            match util::read_fd(STDIN, &mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    let (forward, detached) = detach.feed(&buf[..n]);
                    if !forward.is_empty() {
                        ipc::append_frame(&mut sock_write_buf, Tag::Input, &forward);
                    }
                    if detached {
                        ipc::append_frame(&mut sock_write_buf, Tag::Detach, b"");
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }

        // daemon -> stdout
        if fds[1].revents & libc::POLLIN != 0 {
            match read_buf.read_from(sock_fd) {
                Ok(0) => return Ok(()),
                Ok(_) => loop {
                    match read_buf.next() {
                        Ok(Some(frame)) => {
                            if frame.tag == Tag::Output && !frame.payload.is_empty() {
                                stdout_buf.extend_from_slice(&frame.payload);
                            }
                        }
                        Ok(None) => break,
                        Err(e) => return Err(e),
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionReset
                        || e.kind() == std::io::ErrorKind::BrokenPipe =>
                {
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Flush buffered writes toward the daemon.
        if fds[1].revents & libc::POLLOUT != 0 && !sock_write_buf.is_empty() {
            match stream_writer.write(&sock_write_buf) {
                Ok(n) => {
                    sock_write_buf.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionReset
                        || e.kind() == std::io::ErrorKind::BrokenPipe =>
                {
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }

        if !stdout_buf.is_empty() {
            match util::write_fd(STDOUT, &stdout_buf) {
                Ok(n) => {
                    stdout_buf.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }

        if fds[1].revents & err_events != 0 {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_ctrl_backslash_detaches() {
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"\x1c"), (vec![], true));
    }

    #[test]
    fn bytes_before_detach_are_forwarded() {
        // Ctrl-\ mid-buffer: the preceding keystrokes must still reach the app
        // (the old matcher dropped the whole buffer). github #17.
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"abc\x1c"), (b"abc".to_vec(), true));
    }

    #[test]
    fn plain_input_passes_through() {
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"hello"), (b"hello".to_vec(), false));
    }

    #[test]
    fn kitty_detach_in_one_read() {
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"\x1b[92;5u"), (vec![], true));
        let mut m2 = DetachMatcher::default();
        assert_eq!(m2.feed(b"\x1b[92;5:1u"), (vec![], true));
    }

    #[test]
    fn kitty_detach_split_across_reads() {
        // The 7-byte CSI-u sequence arriving in two reads must still detach
        // (the old substring scan missed this). github #17.
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"\x1b[92"), (vec![], false)); // held back as partial
        assert_eq!(m.feed(b";5u"), (vec![], true));
    }

    #[test]
    fn non_detach_kitty_key_is_forwarded_after_split() {
        // A different CSI-u key sharing the `\x1b[9` prefix must be delivered,
        // not swallowed, once disambiguated on the next read.
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"\x1b[9"), (vec![], false));
        let (fwd, detached) = m.feed(b"7;5u");
        assert!(!detached);
        assert_eq!(fwd, b"\x1b[97;5u");
    }
}
