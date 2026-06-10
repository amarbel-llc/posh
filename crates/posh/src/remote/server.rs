//! Roaming remote server (mosh-server port, simplified SSP): owns the PTY
//! and a posh_term::Terminal, and syncs screen state to the client as
//! dump_vt frames (full or diffed against the last client-acked frame).

use posh_term::Terminal;

use crate::pty;
use crate::remote::crypto::Key;
use crate::remote::datagram::{Connection, DEFAULT_PORT_RANGE};
use crate::remote::sync::{
    self, ClientMessage, FragmentAssembly, Fragmenter, FrameBody, InputInbox, ServerFrame,
};
use crate::util::{self, now_ms, Result};

const SEND_INTERVAL_MIN: u64 = 20; // ms between fresh frames
const HEARTBEAT_INTERVAL: u64 = 3000; // ms between empty keepalives
const SHUTDOWN_GRACE: u64 = 10_000; // ms to wait for the final-state ack

pub fn run(port_range: Option<(u16, u16)>, command: Option<Vec<String>>) -> Result<()> {
    let key = Key::random();
    let (conn, port) = Connection::server(port_range.unwrap_or(DEFAULT_PORT_RANGE), &key)?;

    // The ssh wrapper parses this line; it must be the only stdout output.
    println!("POSH CONNECT {port} {}", key.to_base64());
    use std::io::Write;
    std::io::stdout().flush()?;

    util::ignore_signal(libc::SIGHUP);
    if util::double_fork()? {
        eprintln!("[posh-server detached]");
        return Ok(());
    }
    util::redirect_stdio_devnull();

    let (rows, cols) = (24u16, 80u16);
    let child = pty::spawn_shell(command.as_deref(), rows, cols, &[])?;
    util::set_nonblocking(child.master)?;

    server_loop(conn, child, rows, cols);
    std::process::exit(0);
}

struct FrameState {
    num: u64,
    data: Vec<u8>,
}

fn server_loop(mut conn: Connection, child: pty::PtyChild, rows: u16, cols: u16) {
    let mut term = Terminal::new(rows, cols);
    let mut fragmenter = Fragmenter::new();
    let mut assembly = FragmentAssembly::new();
    let mut inbox = InputInbox::new();

    // Frame 0 is the implicit empty initial state shared with the client, so
    // the very first real frame can already be expressed as a diff.
    let mut current = FrameState {
        num: 0,
        data: Vec::new(),
    };
    // Last frame the client confirmed; None data means we no longer have its
    // bytes and must send a full dump.
    let mut acked_num: u64 = 0;
    let mut acked_data: Option<Vec<u8>> = Some(Vec::new());
    let mut outstanding: Vec<FrameState> = Vec::new();

    let mut last_gen = term.generation();
    let mut last_send: u64 = 0;
    let mut client_size: (u16, u16) = (rows, cols);
    let mut pty_open = true;
    let mut shutdown = false;
    let mut shutdown_at: u64 = 0;
    let mut force_ack = false;
    // Set when the shell exits: forces one final frame (with FLAG_SHUTDOWN)
    // that the client must ack before we go away.
    let mut force_frame = false;

    loop {
        let now = now_ms();
        let timeout = if conn.has_remote() {
            let mut deadline = last_send + HEARTBEAT_INTERVAL;
            if acked_num < current.num {
                deadline = deadline.min(last_send + conn.rto());
            }
            if term.generation() != last_gen || force_ack || force_frame {
                deadline = deadline.min(last_send + SEND_INTERVAL_MIN);
            }
            if shutdown {
                deadline = deadline.min(shutdown_at + SHUTDOWN_GRACE);
            }
            deadline.saturating_sub(now).min(1000) as i32
        } else if shutdown {
            // Shell exited before any client appeared; nothing to wait for.
            break;
        } else {
            -1
        };

        let mut fds = vec![util::pollfd(conn.raw_fd(), libc::POLLIN)];
        if pty_open {
            fds.push(util::pollfd(child.master, libc::POLLIN));
        }
        match util::poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }

        // Shell output -> terminal model.
        if pty_open && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let mut buf = [0u8; 4096];
            match util::read_fd(child.master, &mut buf) {
                Ok(0) => {
                    pty_open = false;
                }
                Ok(n) => {
                    term.process(&buf[..n]);
                    let responses = term.take_responses();
                    if !responses.is_empty() {
                        let _ = util::write_all_retry(child.master, &responses, 100);
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    pty_open = false; // EIO: shell gone
                }
            }
            if !pty_open && !shutdown {
                shutdown = true;
                shutdown_at = now_ms();
                force_frame = true;
                unsafe {
                    let mut status = 0;
                    libc::waitpid(child.pid, &mut status, libc::WNOHANG);
                }
            }
        }

        // Client datagrams.
        if fds[0].revents & libc::POLLIN != 0 {
            loop {
                match conn.recv() {
                    Ok(Some(payload)) => {
                        let Ok(frag) = sync::Fragment::from_bytes(&payload) else {
                            continue;
                        };
                        let Some(assembled) = assembly.add(frag) else {
                            continue;
                        };
                        let Ok(msg) = ClientMessage::decode(&assembled) else {
                            continue;
                        };
                        handle_client_message(
                            &msg,
                            &mut term,
                            &child,
                            pty_open,
                            &mut inbox,
                            &mut client_size,
                            &mut force_ack,
                        );
                        update_acks(
                            &msg,
                            &current,
                            &mut outstanding,
                            &mut acked_num,
                            &mut acked_data,
                        );
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }

        // Frame production and (re)transmission.
        let now = now_ms();
        if conn.has_remote() {
            let dirty = term.generation() != last_gen;
            let mut send_frame = false;
            let mut send_empty = false;

            if (dirty || force_frame) && now.saturating_sub(last_send) >= SEND_INTERVAL_MIN {
                last_gen = term.generation();
                force_frame = false;
                outstanding.push(FrameState {
                    num: current.num,
                    data: std::mem::take(&mut current.data),
                });
                if outstanding.len() > 8 {
                    outstanding.remove(0);
                }
                current = FrameState {
                    num: current.num + 1,
                    data: term.dump_vt(),
                };
                send_frame = true;
            } else if acked_num < current.num && now.saturating_sub(last_send) >= conn.rto() {
                send_frame = true;
            } else if now.saturating_sub(last_send) >= HEARTBEAT_INTERVAL {
                send_empty = true;
            } else if force_ack && acked_num >= current.num {
                // Input arrived but produced no new frame yet: ack promptly so
                // the client can clear its outbox.
                send_empty = true;
            }
            force_ack = false;

            let flags = if shutdown { sync::FLAG_SHUTDOWN } else { 0 };
            if send_frame {
                let body = match &acked_data {
                    Some(base) => {
                        let diff = sync::make_diff(base, &current.data);
                        if diff.len() + 8 < current.data.len() {
                            FrameBody::Diff {
                                base: acked_num,
                                diff,
                            }
                        } else {
                            FrameBody::Full(current.data.clone())
                        }
                    }
                    None => FrameBody::Full(current.data.clone()),
                };
                let frame = ServerFrame {
                    flags,
                    frame_num: current.num,
                    input_ack: inbox.next_offset(),
                    body,
                };
                send_payload(&mut conn, &mut fragmenter, &frame.encode());
                last_send = now;
            } else if send_empty {
                let frame = ServerFrame {
                    flags,
                    frame_num: current.num,
                    input_ack: inbox.next_offset(),
                    body: FrameBody::Empty,
                };
                send_payload(&mut conn, &mut fragmenter, &frame.encode());
                last_send = now;
            }
        }

        if shutdown {
            // The shell has exited: announce it (frames now carry the
            // shutdown flag) and leave once the client confirmed the final
            // state, or after the grace period.
            if !force_frame && term.generation() == last_gen && acked_num >= current.num {
                break;
            }
            if now_ms().saturating_sub(shutdown_at) >= SHUTDOWN_GRACE {
                break;
            }
        }
    }

    if pty_open {
        unsafe {
            libc::kill(-child.pid, libc::SIGHUP);
        }
    }
    unsafe {
        let mut status = 0;
        libc::waitpid(child.pid, &mut status, libc::WNOHANG);
        libc::close(child.master);
    }
}

fn handle_client_message(
    msg: &ClientMessage,
    term: &mut Terminal,
    child: &pty::PtyChild,
    pty_open: bool,
    inbox: &mut InputInbox,
    client_size: &mut (u16, u16),
    force_ack: &mut bool,
) {
    if msg.rows > 0 && msg.cols > 0 && (msg.rows, msg.cols) != *client_size {
        *client_size = (msg.rows, msg.cols);
        pty::set_term_size(child.master, msg.rows, msg.cols);
        term.resize(msg.rows, msg.cols);
    }
    if let Some(new_input) = inbox.accept(msg.input_base, &msg.input) {
        if pty_open {
            let _ = util::write_all_retry(child.master, new_input, 500);
        }
        *force_ack = true;
    }
}

fn update_acks(
    msg: &ClientMessage,
    current: &FrameState,
    outstanding: &mut Vec<FrameState>,
    acked_num: &mut u64,
    acked_data: &mut Option<Vec<u8>>,
) {
    if msg.acked_frame <= *acked_num {
        return;
    }
    *acked_num = msg.acked_frame;
    *acked_data = if msg.acked_frame == current.num {
        Some(current.data.clone())
    } else {
        outstanding
            .iter()
            .find(|f| f.num == msg.acked_frame)
            .map(|f| f.data.clone())
    };
    outstanding.retain(|f| f.num >= msg.acked_frame);
}

fn send_payload(conn: &mut Connection, fragmenter: &mut Fragmenter, payload: &[u8]) {
    for frag in fragmenter.make_fragments(payload, sync::FRAGMENT_CONTENTS_MAX) {
        let _ = conn.send(&frag.to_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::sync::InputOutbox;
    use crate::util;

    /// Drives a real server_loop over loopback UDP: the PTY runs a shell
    /// that reads one line and exits, so a single test covers input
    /// delivery+ack, frame flow, and the shutdown handshake.
    #[test]
    fn server_loop_input_and_shutdown_handshake() {
        let key = Key::random();
        let (server_conn, port) = Connection::server((62100, 62199), &key).unwrap();
        let cmd: Vec<String> = vec!["/bin/sh".into(), "-c".into(), "read x; exit 0".into()];
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[]).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || server_loop(server_conn, child, 24, 80));

        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut conn = Connection::client(addr, &key).unwrap();
        let mut fragmenter = Fragmenter::new();
        let mut assembly = FragmentAssembly::new();
        let mut outbox = InputOutbox::new();
        outbox.push(b"hello\n");

        let mut acked_frame = 0u64;
        let mut input_acked = 0u64;
        let mut saw_shutdown = false;
        let deadline = now_ms() + 15_000;
        while now_ms() < deadline {
            let msg = ClientMessage {
                acked_frame,
                rows: 24,
                cols: 80,
                input_base: outbox.base(),
                input: outbox.pending().to_vec(),
            };
            for frag in fragmenter.make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX) {
                conn.send(&frag.to_bytes()).unwrap();
            }
            if saw_shutdown && acked_frame > 0 {
                break; // shutdown frame acked in the message just sent
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
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
                        acked_frame = acked_frame.max(frame.frame_num);
                        input_acked = input_acked.max(frame.input_ack);
                        if frame.flags & sync::FLAG_SHUTDOWN != 0 {
                            saw_shutdown = true;
                        }
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }

        assert!(saw_shutdown, "never saw the shutdown flag");
        assert_eq!(input_acked, 6, "input stream not fully acked");
        server.join().unwrap();
    }
}
