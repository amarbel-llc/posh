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

/// Detects the Kitty keyboard protocol encoding of Ctrl+\ (92 = backslash,
/// 5 = ctrl modifier, :1 = press event).
fn is_kitty_ctrl_backslash(buf: &[u8]) -> bool {
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
    contains(buf, b"\x1b[92;5u") || contains(buf, b"\x1b[92;5:1u")
}

fn client_loop(stream: UnixStream) -> Result<()> {
    util::install_sigwinch_handler();
    stream.set_nonblocking(true)?;
    let sock_fd = stream.as_raw_fd();
    util::set_nonblocking(STDIN)?;

    let mut sock_write_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut stdout_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut read_buf = FrameBuffer::new();
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

        let mut fds = vec![util::pollfd(STDIN, libc::POLLIN)];
        let mut sock_events = libc::POLLIN;
        if !sock_write_buf.is_empty() {
            sock_events |= libc::POLLOUT;
        }
        fds.push(util::pollfd(sock_fd, sock_events));
        if !stdout_buf.is_empty() {
            fds.push(util::pollfd(STDOUT, libc::POLLOUT));
        }

        match util::poll(&mut fds, -1) {
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
                    if buf[0] == 0x1c || is_kitty_ctrl_backslash(&buf[..n]) {
                        ipc::append_frame(&mut sock_write_buf, Tag::Detach, b"");
                    } else {
                        ipc::append_frame(&mut sock_write_buf, Tag::Input, &buf[..n]);
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
                Ok(_) => {
                    while let Some(frame) = read_buf.next() {
                        if frame.tag == Tag::Output && !frame.payload.is_empty() {
                            stdout_buf.extend_from_slice(&frame.payload);
                        }
                    }
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
    fn kitty_detach_sequences() {
        assert!(is_kitty_ctrl_backslash(b"\x1b[92;5u"));
        assert!(is_kitty_ctrl_backslash(b"\x1b[92;5:1u"));
        assert!(!is_kitty_ctrl_backslash(b"\x1b[92;5:3u"));
        assert!(!is_kitty_ctrl_backslash(b"\x1b[92;1u"));
        assert!(!is_kitty_ctrl_backslash(b"garbage"));
    }
}
