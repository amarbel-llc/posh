//! Roaming remote client (mosh-client/stmclient port, without the local
//! prediction/overlay engine): raw-mode tty, reliable input stream upload,
//! and full-state frame application (clear screen + write the dump_vt
//! stream).

use std::net::{SocketAddr, ToSocketAddrs};

use crate::pty::{self, RawMode};
use crate::remote::crypto::Key;
use crate::remote::datagram::Connection;
use crate::remote::sync::{
    self, ClientMessage, FragmentAssembly, Fragmenter, FrameBody, InputOutbox, ServerFrame,
};
use crate::util::{self, now_ms, Error, Result};

const STDIN: i32 = 0;
const STDOUT: i32 = 1;
const HEARTBEAT_INTERVAL: u64 = 3000; // ms
const SILENCE_WARN_AFTER: u64 = 5000; // ms

pub fn run(host: &str, port: u16) -> Result<()> {
    // mosh convention: the key travels in the environment, never on argv
    // (argv is world-readable via ps).
    let key_str = std::env::var("POSH_KEY")
        .map_err(|_| Error::from("POSH_KEY environment variable not set"))?;
    std::env::remove_var("POSH_KEY");
    let key = Key::from_base64(key_str.trim())?;

    let addr = resolve(host, port)?;
    let conn = Connection::client(addr, &key)?;

    let raw = RawMode::enable(STDIN)?;
    let result = client_loop(conn);
    drop(raw);
    eprintln!("\nposh: [client exited]");
    result
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| Error(format!("could not resolve {host}: {e}")))?
        .collect();
    // Prefer IPv4 (the common path for roaming UDP), fall back to anything.
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or_else(|| Error(format!("no addresses for {host}")))
}

fn client_loop(mut conn: Connection) -> Result<()> {
    util::install_sigwinch_handler();
    util::set_nonblocking(STDIN)?;

    let mut fragmenter = Fragmenter::new();
    let mut assembly = FragmentAssembly::new();
    let mut outbox = InputOutbox::new();
    let (mut rows, mut cols) = pty::term_size(STDOUT);
    // Frame 0 is the implicit empty initial state.
    let mut applied_num: u64 = 0;
    let mut applied_data: Vec<u8> = Vec::new();
    let mut last_send: u64 = 0;
    let mut last_contact = now_ms();
    let mut warned = false;
    let mut shutdown_seen = false;

    // Hello: teaches the server our address and terminal size.
    send_message(&mut conn, &mut fragmenter, applied_num, rows, cols, &outbox);

    loop {
        let now = now_ms();
        let mut deadline = last_send + HEARTBEAT_INTERVAL;
        if !outbox.is_empty() {
            deadline = deadline.min(last_send + conn.rto());
        }
        if !warned {
            deadline = deadline.min(last_contact + SILENCE_WARN_AFTER);
        }
        let timeout = deadline.saturating_sub(now).min(1000) as i32;

        let mut fds = [
            util::pollfd(STDIN, libc::POLLIN),
            util::pollfd(conn.raw_fd(), libc::POLLIN),
        ];
        let mut send_now = false;
        match util::poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }

        if util::take_flag(&util::SIGWINCH_RECEIVED) {
            let size = pty::term_size(STDOUT);
            rows = size.0;
            cols = size.1;
            send_now = true;
        }

        // Keystrokes -> reliable input stream.
        if fds[0].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 4096];
            match util::read_fd(STDIN, &mut buf) {
                Ok(0) => return Ok(()), // EOF on the local tty
                Ok(n) => {
                    outbox.push(&buf[..n]);
                    send_now = true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }

        // Server frames.
        if fds[1].revents & libc::POLLIN != 0 {
            loop {
                match conn.recv() {
                    Ok(Some(payload)) => {
                        let Ok(frag) = sync::Fragment::from_bytes(&payload) else {
                            continue;
                        };
                        let Some(assembled) = assembly.add(frag) else {
                            continue;
                        };
                        let Ok(frame) = ServerFrame::decode(&assembled) else {
                            continue;
                        };
                        last_contact = now_ms();
                        if warned {
                            eprintln!("\r\nposh: connection re-established.");
                            warned = false;
                        }
                        outbox.ack(frame.input_ack);
                        if frame.flags & sync::FLAG_SHUTDOWN != 0 {
                            shutdown_seen = true;
                        }
                        if apply_frame(&frame, &mut applied_num, &mut applied_data) {
                            send_now = true; // ack the new state promptly
                        }
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }

        let now = now_ms();
        if !warned && now.saturating_sub(last_contact) >= SILENCE_WARN_AFTER {
            eprintln!(
                "\r\nposh: connection lost (no contact in {}s); retrying...",
                now.saturating_sub(last_contact) / 1000
            );
            warned = true;
        }

        if send_now
            || (!outbox.is_empty() && now.saturating_sub(last_send) >= conn.rto())
            || now.saturating_sub(last_send) >= HEARTBEAT_INTERVAL
        {
            send_message(&mut conn, &mut fragmenter, applied_num, rows, cols, &outbox);
            last_send = now;
        }

        if shutdown_seen {
            // The shell exited; the final-state ack went out just above.
            return Ok(());
        }
    }
}

/// Applies a frame to the local terminal. Frames reconstruct complete screen
/// state, so application is: clear screen, then write the dump_vt stream.
/// Returns true when the frame advanced (or repeated) server state and an
/// ack should go out.
fn apply_frame(frame: &ServerFrame, applied_num: &mut u64, applied_data: &mut Vec<u8>) -> bool {
    if frame.frame_num < *applied_num {
        return true; // stale retransmission: re-ack our newer state
    }
    let bytes: Vec<u8> = match &frame.body {
        FrameBody::Empty => return false,
        FrameBody::Full(bytes) => bytes.clone(),
        FrameBody::Diff { base, diff } => {
            if *base != *applied_num {
                // Diff against a state we do not hold; the server will fall
                // back to a full dump once it sees our (stale) ack.
                return true;
            }
            match sync::apply_diff(applied_data, diff) {
                Some(bytes) => bytes,
                None => return true,
            }
        }
    };
    if frame.frame_num == *applied_num {
        return true; // duplicate retransmission: re-ack, don't redraw
    }
    let mut out = Vec::with_capacity(8 + bytes.len());
    out.extend_from_slice(b"\x1b[2J\x1b[H");
    out.extend_from_slice(&bytes);
    let _ = util::write_all_retry(STDOUT, &out, 1000);
    *applied_num = frame.frame_num;
    *applied_data = bytes;
    true
}

fn send_message(
    conn: &mut Connection,
    fragmenter: &mut Fragmenter,
    acked_frame: u64,
    rows: u16,
    cols: u16,
    outbox: &InputOutbox,
) {
    let msg = ClientMessage {
        acked_frame,
        rows,
        cols,
        input_base: outbox.base(),
        input: outbox.pending().to_vec(),
    };
    for frag in fragmenter.make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX) {
        let _ = conn.send(&frag.to_bytes());
    }
}
