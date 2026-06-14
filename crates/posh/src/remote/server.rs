//! Roaming remote server (mosh-server port, simplified SSP): owns the PTY
//! and a posh_term::Terminal, and syncs screen state to the client as
//! dump_vt frames (full or diffed against the last client-acked frame).

use posh_term::Terminal;

use crate::pty;
use crate::remote::caps;
use crate::remote::crypto::Key;
use crate::remote::datagram::{Connection, Family, DEFAULT_PORT_RANGE, SEND_INTERVAL_MIN};
use crate::remote::stats::Stats;
use crate::remote::sync::{
    self, ClientMessage, EchoAck, FragmentAssembly, Fragmenter, FrameBody, InputInbox, ServerFrame,
    HEARTBEAT_INTERVAL,
};
use crate::util::{self, now_ms, Result};

const SHUTDOWN_GRACE: u64 = 10_000; // ms to wait for the final-state ack
/// Silence after which the peer is forgotten (sending stops, the session
/// stays alive waiting for the client to come back).
const PEER_TIMEOUT: u64 = 60_000; // ms

/// POSH_SERVER_NETWORK_TMOUT / POSH_SERVER_SIGNAL_TMOUT, in seconds
/// (0 = disabled), as mosh's MOSH_SERVER_*_TMOUT.
fn timeout_env(name: &str) -> u64 {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => match v.parse::<i64>() {
            Ok(n) if n >= 0 => n as u64,
            Ok(_) => {
                eprintln!("{name} is negative, ignoring");
                0
            }
            Err(_) => {
                eprintln!("{name} not a valid integer, ignoring");
                0
            }
        },
        _ => 0,
    }
}

pub fn run(
    port_range: Option<(u16, u16)>,
    family: Family,
    command: Option<Vec<String>>,
) -> Result<()> {
    util::check_utf8_locale("posh-server")?;

    let key = Key::random();
    let (conn, port) = Connection::server(port_range.unwrap_or(DEFAULT_PORT_RANGE), &key, family)?;

    // The ssh wrapper parses these lines; they must be the only stdout
    // output. POSH IP reports the address the client should dial (the
    // server side of the ssh connection), as mosh.pl's "MOSH IP".
    if let Ok(ssh_connection) = std::env::var("SSH_CONNECTION") {
        if let Some(ip) = ssh_connection.split_whitespace().nth(2) {
            println!("POSH IP {ip}");
        }
    }
    println!("POSH CONNECT {port} {}", key.to_base64());
    use std::io::Write;
    std::io::stdout().flush()?;

    util::ignore_signal(libc::SIGHUP);
    if util::double_fork()? {
        eprintln!("[posh-server detached]");
        return Ok(());
    }
    util::redirect_stdio_devnull();
    util::install_sigusr1_handler();

    let (rows, cols) = (24u16, 80u16);
    // posh#51: the ssh bootstrap allocates no remote pty, so sshd set no TERM;
    // terminfo::session_env gives the session shell a resolved TERM (+ the
    // client's COLORTERM) so color-by-$TERM tools (git, Charmbracelet TUIs)
    // aren't left colorless.
    let child = pty::spawn_shell(command.as_deref(), rows, cols, &crate::terminfo::session_env())?;
    util::set_nonblocking(child.master)?;

    server_loop(conn, child, rows, cols);
    std::process::exit(0);
}

struct FrameState {
    num: u64,
    /// The visible-screen `dump_vt` bytes as of this frame — the diff base
    /// for a later `Diff`. A scrollback frame leaves the visible screen
    /// unchanged, so it records the same visible bytes as the frame before
    /// it, keeping the diff-base chain intact across interleaved scrollback
    /// frames.
    data: Vec<u8>,
    /// Scrollback rows the client will have accumulated after applying this
    /// frame (RFC 0002): the running high-water that only advances on a
    /// scrollback frame. Acking this frame tells the server the client holds
    /// scrollback through here, so the next body's appended count starts
    /// from it.
    sb_total: u64,
}

fn server_loop(mut conn: Connection, child: pty::PtyChild, rows: u16, cols: u16) {
    // Optional perf instrumentation (POSH_DEBUG_LOG). run() has already
    // double-forked and redirected stdio to /dev/null, so this file fd is the
    // server's only viable diagnostic sink; inert when the env var is unset.
    let mut stats = Stats::new();
    let mut term = Terminal::new(rows, cols);
    let mut fragmenter = Fragmenter::new();
    let mut assembly = FragmentAssembly::new();
    let mut inbox = InputInbox::new();
    let mut echo = EchoAck::new();

    // Idle timeouts (seconds; 0 = never). NETWORK fires on its own; SIGNAL
    // only fires when SIGUSR1 has been received.
    let network_tmout = timeout_env("POSH_SERVER_NETWORK_TMOUT") * 1000;
    let signal_tmout = timeout_env("POSH_SERVER_SIGNAL_TMOUT") * 1000;

    // Frame 0 is the implicit empty initial state shared with the client, so
    // the very first real frame can already be expressed as a diff.
    let mut current = FrameState {
        num: 0,
        data: Vec::new(),
        sb_total: 0,
    };
    // Last frame the client confirmed; None data means we no longer have its
    // bytes and must send a full dump.
    let mut acked_num: u64 = 0;
    let mut acked_data: Option<Vec<u8>> = Some(Vec::new());
    let mut outstanding: Vec<FrameState> = Vec::new();

    // Scrollback sync (RFC 0002). `peer_wants_scrollback` tracks whether the
    // most recent client message advertised SCROLLBACK (capabilities do not
    // persist; the client drops it on resize). `sb_floor` is the scrollback
    // total at which accumulation (re)started — growth below it is never
    // back-filled (forward-only; a resize resyncs at the new width).
    // `sb_high` is the total covered by the latest produced frame, and
    // `acked_sb_total` the total the client has confirmed. `current_is_sb`
    // says whether `current` is a scrollback frame, and `last_was_sb`
    // alternates the two kinds so heavy output does not starve either.
    let mut peer_wants_scrollback = false;
    let mut sb_floor: u64 = 0;
    let mut sb_high: u64 = 0;
    let mut acked_sb_total: u64 = 0;
    let mut current_is_sb = false;
    let mut last_was_sb = false;

    let mut last_gen = term.generation();
    let mut last_send: u64 = 0;
    let mut last_heard: u64 = now_ms();
    let mut client_size: (u16, u16) = (rows, cols);
    let mut pty_open = true;
    let mut shutdown = false;
    let mut shutdown_at: u64 = 0;
    // EXIT_STATUS (RFC 0001 §3): the command's shell-style exit code, and
    // whether the peer ever advertised understanding the capability.
    let mut exit_status: Option<i32> = None;
    let mut peer_wants_exit = false;
    let mut force_ack = false;
    // Set when the shell exits: forces one final frame (with FLAG_SHUTDOWN)
    // that the client must ack before we go away.
    let mut force_frame = false;

    loop {
        let now = now_ms();
        // A silent peer is forgotten after a minute: sending stops (the
        // session stays alive) until an authentic datagram arrives again.
        let peer_active = conn.has_remote() && now.saturating_sub(last_heard) < PEER_TIMEOUT;
        if network_tmout > 0 && now.saturating_sub(last_heard) >= network_tmout {
            break; // POSH_SERVER_NETWORK_TMOUT expired: give up the session
        }
        if util::take_flag(&util::SIGUSR1_RECEIVED)
            && signal_tmout > 0
            && now.saturating_sub(last_heard) >= signal_tmout
        {
            break; // signaled and idle long enough
        }

        let timeout = if peer_active {
            let mut deadline = last_send + HEARTBEAT_INTERVAL;
            if acked_num < current.num {
                deadline = deadline.min(last_send + conn.rto());
            }
            if term.generation() != last_gen || force_frame {
                deadline = deadline.min(last_send + conn.send_interval());
            }
            if force_ack {
                // Input acks go out as empty frames with no pacing gate:
                // wake promptly rather than at the frame interval.
                deadline = deadline.min(last_send + SEND_INTERVAL_MIN);
            }
            if shutdown {
                deadline = deadline.min(shutdown_at + SHUTDOWN_GRACE);
            }
            if let Some(wait) = echo.wait_time(now) {
                deadline = deadline.min(now + wait);
            }
            deadline.saturating_sub(now).min(1000) as i32
        } else if shutdown {
            // Shell exited with no reachable client; nothing to wait for.
            break;
        } else if network_tmout > 0 {
            (last_heard + network_tmout).saturating_sub(now).min(1000) as i32
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
                exit_status = util::try_reap(child.pid).map(util::exit_code);
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
                        last_heard = now_ms();
                        if caps::find(&msg.caps, caps::CAP_EXIT_STATUS).is_some() {
                            peer_wants_exit = true;
                        }
                        // Per-message (caps do not persist): does the client
                        // still want scrollback? A fresh advertisement after
                        // a lull/resize restarts appended-row counting from
                        // the current ring — forward-only, no back-fill
                        // (RFC 0002 §1, §4). The resize itself is applied in
                        // handle_client_message below, so a reactivation
                        // message anchors to the post-resize ring.
                        handle_client_message(
                            &msg,
                            &mut term,
                            &child,
                            pty_open,
                            &mut inbox,
                            &mut echo,
                            &mut client_size,
                            &mut force_ack,
                        );
                        if msg.flags & sync::CLIENT_FLAG_SHUTDOWN != 0 && !shutdown {
                            // Client asked to quit: hang up the shell and
                            // start the shutdown handshake.
                            shutdown = true;
                            shutdown_at = now_ms();
                            force_frame = true;
                            if pty_open {
                                util::kill_pgroup(child.pid, libc::SIGHUP);
                            }
                        }
                        update_acks(
                            &msg,
                            &current,
                            &mut outstanding,
                            &mut acked_num,
                            &mut acked_data,
                            &mut acked_sb_total,
                        );
                        let now_wants = caps::find(&msg.caps, caps::CAP_SCROLLBACK).is_some();
                        if now_wants && !peer_wants_scrollback {
                            // (Re)activation: accumulate forward from here.
                            sb_floor = term.primary_scrollback_total();
                            sb_high = sb_high.max(sb_floor);
                        }
                        peer_wants_scrollback = now_wants;
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }

        // Frame production and (re)transmission.
        let now = now_ms();
        if echo.update(now) {
            // The echo ack advanced: tell the client so its predictions
            // can be validated even when the screen did not change.
            force_ack = true;
        }
        let peer_active = conn.has_remote() && now.saturating_sub(last_heard) < PEER_TIMEOUT;
        if peer_active {
            let dirty = term.generation() != last_gen;
            let cur_sb_total = term.primary_scrollback_total();
            let mut send_frame = false;
            let mut send_empty = false;
            // A freshly produced frame (vs a retransmission of the current one):
            // gates the diff-economics sampling so retransmits don't skew it.
            let mut fresh_frame = false;

            // Fresh frames are paced by the SRTT-derived send interval
            // (mosh: ~two frames per RTT, clamped 20..250ms), not a fixed
            // floor — on a slow link more frames only self-congest it.
            let paced = now.saturating_sub(last_send) >= conn.send_interval();
            let want_visible = (dirty || force_frame) && paced;
            // Scrollback grew (RFC 0002): new rows on the primary screen, the
            // peer wants them, and they lie beyond both the forward floor and
            // what the latest frame already covers. Suppressed during the
            // shutdown handshake so the final visible frame is not deferred.
            let want_scrollback = peer_wants_scrollback
                && !term.is_alt_screen()
                && !force_frame
                && !shutdown
                && cur_sb_total > sb_high
                && cur_sb_total > acked_sb_total.max(sb_floor)
                && paced;
            // At most one fresh body per opportunity; when both are ready
            // (heavy output scrolling the screen) alternate so neither kind
            // starves the other.
            let make_scrollback = want_scrollback && (!want_visible || !last_was_sb);
            let make_visible = want_visible && !make_scrollback;

            if make_visible {
                last_gen = term.generation();
                force_frame = false;
                outstanding.push(FrameState {
                    num: current.num,
                    data: std::mem::take(&mut current.data),
                    sb_total: current.sb_total,
                });
                if outstanding.len() > 8 {
                    outstanding.remove(0);
                }
                current = FrameState {
                    num: current.num + 1,
                    data: stats.time_dump_vt(|| term.dump_vt()),
                    // A visible frame carries no scrollback rows, so applying it
                    // leaves the client at whatever scrollback it held at the
                    // diff base (the acked frame): acked_sb_total, NOT sb_high.
                    // sb_high counts rows put into a scrollback frame that may
                    // have been lost; if a visible-frame ack confirmed those,
                    // the rows of a dropped-then-superseded scrollback frame
                    // would never be re-shipped (finding #1).
                    sb_total: acked_sb_total,
                };
                current_is_sb = false;
                last_was_sb = false;
                send_frame = true;
                fresh_frame = true;
            } else if make_scrollback {
                // The visible screen is unchanged, so the scrollback frame
                // inherits the standing visible dump as its diff base — the
                // diff-base chain is unbroken across interleaved frames.
                let visible = current.data.clone();
                outstanding.push(FrameState {
                    num: current.num,
                    data: std::mem::take(&mut current.data),
                    sb_total: current.sb_total,
                });
                if outstanding.len() > 8 {
                    outstanding.remove(0);
                }
                current = FrameState {
                    num: current.num + 1,
                    data: visible,
                    sb_total: cur_sb_total,
                };
                sb_high = cur_sb_total;
                current_is_sb = true;
                last_was_sb = true;
                send_frame = true;
            } else if acked_num < current.num && now.saturating_sub(last_send) >= conn.rto() {
                send_frame = true;
                stats.record_retransmit();
            } else if now.saturating_sub(last_send) >= HEARTBEAT_INTERVAL {
                send_empty = true;
            } else if force_ack && acked_num >= current.num {
                // Input arrived but produced no new frame yet: ack promptly so
                // the client can clear its outbox.
                send_empty = true;
            }
            if send_frame || send_empty {
                // Every frame carries the latest input/echo acks.
                force_ack = false;
            }

            if send_frame || send_empty {
                let body = if !send_frame {
                    stats.record_frame_empty();
                    FrameBody::Empty
                } else if current_is_sb {
                    // Scrollback body (RFC 0002 §2), fresh or retransmitted:
                    // the rows that entered scrollback between the acked
                    // frame and this frame's coverage, anchored to the
                    // current ack so a retransmit or supersede is idempotent.
                    // Bounded by the rows still in the ring — anything the
                    // ring has since evicted is gone (the client's view is
                    // partial by design), and bounded by inter-frame growth,
                    // never by total scrollback depth.
                    // Work in ring positions (newest-anchored) rather than
                    // absolute indices: a width reflow changes the ring
                    // length without advancing the monotonic total, so
                    // absolute mapping is unsafe. `grown` rows have entered
                    // since this frame's coverage and sit at the tail, so the
                    // rows this frame covers end just before them; `appended`
                    // (rows since the ack/floor) is capped to what the ring
                    // still holds — evicted older rows are gone by design.
                    let ring_len = term.primary_scrollback_len();
                    let grown = cur_sb_total.saturating_sub(current.sb_total) as usize;
                    let end = ring_len.saturating_sub(grown);
                    let want = current
                        .sb_total
                        .saturating_sub(acked_sb_total.max(sb_floor)) as usize;
                    let appended = want.min(end);
                    let start = end - appended;
                    let rows: Vec<Vec<u8>> = (start..end)
                        .map(|i| term.dump_scrollback_row(i).unwrap_or_default())
                        .collect();
                    FrameBody::Scrollback {
                        base: acked_num,
                        rows,
                    }
                } else {
                    match &acked_data {
                        Some(base) => {
                            let diff = sync::make_diff(base, &current.data);
                            if diff.len() + 8 < current.data.len() {
                                stats.record_frame_diff();
                                if fresh_frame {
                                    stats.record_diff_frame(current.data.len(), diff.len());
                                }
                                FrameBody::Diff {
                                    base: acked_num,
                                    diff,
                                }
                            } else {
                                stats.record_frame_full();
                                if fresh_frame {
                                    stats.record_full_frame(current.data.len());
                                }
                                FrameBody::Full(current.data.clone())
                            }
                        }
                        None => {
                            // No acked base to diff against — a forced full dump,
                            // not a strategy choice, so it skips the economics.
                            stats.record_frame_full();
                            FrameBody::Full(current.data.clone())
                        }
                    }
                };
                if shutdown && exit_status.is_none() {
                    // The shell may not have been reapable at pty close
                    // (client-requested shutdown SIGHUPs it); retry as the
                    // handshake frames go out.
                    exit_status = util::try_reap(child.pid).map(util::exit_code);
                }
                // Capability table on the frame (RFC 0001 §3). Only peers
                // that advertised a capability receive its payload.
                let mut extras: Vec<caps::Cap> = Vec::new();
                if peer_wants_scrollback {
                    // Acknowledge that we emit scrollback bodies (RFC 0002
                    // §1); empty payload.
                    extras.push(caps::Cap {
                        id: caps::CAP_SCROLLBACK,
                        payload: vec![],
                    });
                }
                if shutdown && peer_wants_exit {
                    if let Some(code) = exit_status {
                        extras.push(caps::Cap {
                            id: caps::CAP_EXIT_STATUS,
                            payload: vec![code.clamp(0, 255) as u8],
                        });
                    }
                }
                let frame_caps = if extras.is_empty() {
                    Vec::new()
                } else {
                    caps::own_table(&extras)
                };
                // Report the remote PTY's ECHO state so an optimistic-echo
                // client knows when local echo is safe (FDR 0006). Off once the
                // pty is gone, and at password prompts / raw-mode apps.
                let echo_flag = if pty_open && pty::echo_on(child.master) {
                    sync::FLAG_ECHO
                } else {
                    0
                };
                let frame = ServerFrame {
                    flags: (if shutdown { sync::FLAG_SHUTDOWN } else { 0 }) | echo_flag,
                    caps: frame_caps,
                    frame_num: current.num,
                    input_ack: inbox.next_offset(),
                    echo_ack: echo.ack(),
                    body,
                };
                send_payload(&mut conn, &mut fragmenter, &frame.encode());
                last_send = now;
            }
        }
        stats.flush_server(now, conn.srtt(), conn.rto(), outstanding.len(), conn.bytes_tx());

        if shutdown {
            // The shell has exited: announce it (frames now carry the
            // shutdown flag) and leave once the client confirmed the final
            // state and the echo ack caught up, or after the grace period.
            if !force_frame
                && !force_ack
                && term.generation() == last_gen
                && acked_num >= current.num
                && echo.ack() >= inbox.next_offset()
            {
                break;
            }
            if now_ms().saturating_sub(shutdown_at) >= SHUTDOWN_GRACE {
                break;
            }
        }
    }

    stats.final_server(
        now_ms(),
        conn.srtt(),
        conn.rto(),
        outstanding.len(),
        conn.bytes_tx(),
    );

    if pty_open {
        util::kill_pgroup(child.pid, libc::SIGHUP);
    }
    let _ = util::try_reap(child.pid);
    util::close_fd(child.master);
}

fn handle_client_message(
    msg: &ClientMessage,
    term: &mut Terminal,
    child: &pty::PtyChild,
    pty_open: bool,
    inbox: &mut InputInbox,
    echo: &mut EchoAck,
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
        echo.record(inbox.next_offset(), now_ms());
        *force_ack = true;
    }
}

fn update_acks(
    msg: &ClientMessage,
    current: &FrameState,
    outstanding: &mut Vec<FrameState>,
    acked_num: &mut u64,
    acked_data: &mut Option<Vec<u8>>,
    acked_sb_total: &mut u64,
) {
    // Ignore acks for frames never sent: an authenticated client claiming a
    // future frame would otherwise clear `outstanding`, disable retransmits,
    // and satisfy the shutdown gate without confirming the real final state.
    if msg.acked_frame <= *acked_num || msg.acked_frame > current.num {
        return;
    }
    *acked_num = msg.acked_frame;
    let acked = if msg.acked_frame == current.num {
        Some((current.data.clone(), current.sb_total))
    } else {
        outstanding
            .iter()
            .find(|f| f.num == msg.acked_frame)
            .map(|f| (f.data.clone(), f.sb_total))
    };
    if let Some((data, sb_total)) = acked {
        *acked_data = Some(data);
        // A frame's sb_total is the scrollback the client holds after applying
        // it: a scrollback frame advances it by the rows it carries; a visible
        // frame inherits the acked base's total (it carries no rows). So acking
        // any frame confirms only scrollback the client actually received, even
        // when a scrollback frame was lost and leapfrogged (RFC 0002 §2/§3).
        *acked_sb_total = (*acked_sb_total).max(sb_total);
    } else {
        *acked_data = None;
    }
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
    /// delivery+ack, the echo ack, frame flow, and the shutdown handshake.
    #[test]
    fn server_loop_input_and_shutdown_handshake() {
        let key = Key::random();
        let (server_conn, port) = Connection::server((62100, 62199), &key, Family::Inet).unwrap();
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
        let mut echo_acked = 0u64;
        let mut saw_shutdown = false;
        let deadline = now_ms() + 15_000;
        while now_ms() < deadline {
            let msg = ClientMessage {
                flags: 0,
                caps: vec![],
                acked_frame,
                rows: 24,
                cols: 80,
                input_base: outbox.base(),
                input: outbox.pending().to_vec(),
            };
            for frag in fragmenter.make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX) {
                conn.send(&frag.to_bytes()).unwrap();
            }
            if saw_shutdown && acked_frame > 0 && echo_acked == 6 {
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
                        echo_acked = echo_acked.max(frame.echo_ack);
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
        assert_eq!(echo_acked, 6, "echo ack never caught up to the input");
        server.join().unwrap();
    }

    #[test]
    fn client_shutdown_flag_winds_down_the_server() {
        let key = Key::random();
        let (server_conn, port) = Connection::server((62200, 62299), &key, Family::Inet).unwrap();
        // A shell that would run forever without the client's quit request.
        let cmd: Vec<String> = vec!["/bin/sh".into(), "-c".into(), "sleep 600".into()];
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[]).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || server_loop(server_conn, child, 24, 80));

        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut conn = Connection::client(addr, &key).unwrap();
        let mut fragmenter = Fragmenter::new();
        let mut assembly = FragmentAssembly::new();

        let mut acked_frame = 0u64;
        let mut saw_shutdown = false;
        let deadline = now_ms() + 15_000;
        while now_ms() < deadline {
            let msg = ClientMessage {
                flags: sync::CLIENT_FLAG_SHUTDOWN,
                caps: vec![],
                acked_frame,
                rows: 24,
                cols: 80,
                input_base: 0,
                input: Vec::new(),
            };
            for frag in fragmenter.make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX) {
                conn.send(&frag.to_bytes()).unwrap();
            }
            if saw_shutdown && acked_frame > 0 {
                break;
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

        assert!(saw_shutdown, "server never confirmed the client shutdown");
        server.join().unwrap();
    }

    #[test]
    fn fresh_frames_paced_by_send_interval() {
        // A client that never supplies timestamps gives the server no RTT
        // samples, so its send_interval stays at the 250ms initial-SRTT
        // clamp. With the pty flooding output, a server paced by the fixed
        // 20ms floor would emit ~30 fresh frames in the window; one paced
        // by send_interval emits a handful. github #26.
        let key = Key::random();
        let (server_conn, port) = Connection::server((62400, 62499), &key, Family::Inet).unwrap();
        let cmd: Vec<String> = vec![
            "/bin/sh".into(),
            "-c".into(),
            "while :; do echo spam; done".into(),
        ];
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[]).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || server_loop(server_conn, child, 24, 80));

        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut conn = Connection::client(addr, &key).unwrap();
        let mut fragmenter = Fragmenter::new();
        let mut assembly = FragmentAssembly::new();

        let recv_frames = |conn: &mut Connection,
                               assembly: &mut FragmentAssembly,
                               nums: &mut std::collections::HashSet<u64>,
                               saw_shutdown: &mut bool|
         -> u64 {
            let mut highest = 0u64;
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
                        if !matches!(frame.body, FrameBody::Empty) {
                            nums.insert(frame.frame_num);
                        }
                        highest = highest.max(frame.frame_num);
                        if frame.flags & sync::FLAG_SHUTDOWN != 0 {
                            *saw_shutdown = true;
                        }
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            highest
        };

        // Hello (acked_frame 0), then measure for 600ms without acking so
        // every received frame is a freshly produced one or a retransmit of
        // it; distinct frame numbers count fresh productions.
        let hello = ClientMessage {
            flags: 0,
            caps: vec![],
            acked_frame: 0,
            rows: 24,
            cols: 80,
            input_base: 0,
            input: Vec::new(),
        };
        for frag in fragmenter.make_fragments(&hello.encode(), sync::FRAGMENT_CONTENTS_MAX) {
            conn.send(&frag.to_bytes()).unwrap();
        }
        let mut nums = std::collections::HashSet::new();
        let mut saw_shutdown = false;
        let mut highest = 0u64;
        let start = now_ms();
        while now_ms().saturating_sub(start) < 600 {
            highest = highest.max(recv_frames(
                &mut conn,
                &mut assembly,
                &mut nums,
                &mut saw_shutdown,
            ));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!nums.is_empty(), "no frames arrived at all");
        assert!(
            nums.len() <= 6,
            "{} distinct frames in 600ms — fresh frames not paced by send_interval",
            nums.len()
        );

        // Wind down: request shutdown and ack everything we see.
        let deadline = now_ms() + 15_000;
        while now_ms() < deadline {
            let msg = ClientMessage {
                flags: sync::CLIENT_FLAG_SHUTDOWN,
                caps: vec![],
                acked_frame: highest,
                rows: 24,
                cols: 80,
                input_base: 0,
                input: Vec::new(),
            };
            for frag in fragmenter.make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX) {
                conn.send(&frag.to_bytes()).unwrap();
            }
            if saw_shutdown && highest > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            highest = highest.max(recv_frames(
                &mut conn,
                &mut assembly,
                &mut nums,
                &mut saw_shutdown,
            ));
        }
        assert!(saw_shutdown, "server never confirmed the client shutdown");
        server.join().unwrap();
    }

    /// Outcome of a deterministic scrollback session (see
    /// [`run_scrollback_session`]).
    struct ScrollbackOutcome {
        /// Scrollback bodies the client applied (anchored at base == applied_num).
        sb_bodies: usize,
        /// Total rows across the applied scrollback bodies.
        sb_rows: usize,
        /// Whether any Full/Diff (visible-screen) frame was applied.
        saw_visible: bool,
        /// Sorted, de-duplicated `line N` numbers accumulated into the ring.
        lines: Vec<u64>,
    }

    /// Extracts `N` from a scrollback row whose visible text contains "line N"
    /// (the harness's reflected input), ignoring SGR/escape bytes.
    fn row_line_number(row: &[u8]) -> Option<u64> {
        let s = String::from_utf8_lossy(row);
        let idx = s.find("line ")?;
        s[idx + 5..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    }

    /// Deterministic, input-driven scrollback harness. The server child is
    /// `cat` with echo disabled (`stty -echo`), so every line the test sends
    /// through the reliable input stream is reflected back exactly once as
    /// terminal output, scrolling one row off the 24-row screen. `cat` emits
    /// nothing until fed, and the client advertises SCROLLBACK in every
    /// message, so the server pins `sb_floor` at 0 before any output exists —
    /// no race against shell startup. Lines are sent one at a time, each fully
    /// acked before the next (the loop blocks on the socket via `poll`), so
    /// there are no `sleep`s, no burst, and nothing for parallel load to flake.
    ///
    /// `advertise`: whether the client offers SCROLLBACK at all. With
    /// `lose_second_sb_frame` the client simulates a single lost scrollback
    /// datagram: it applies the first scrollback frame, then drops every
    /// transmission of the second (until a visible frame supersedes it), then
    /// resumes — losing a *middle* frame, so a gap is an internal discontinuity
    /// rather than a truncated head. (RFC 0002 §3 append rule: apply a
    /// scrollback body only when base == applied_num.)
    fn run_scrollback_session(
        advertise: bool,
        lose_second_sb_frame: bool,
        ports: (u16, u16),
    ) -> ScrollbackOutcome {
        const LINES: u64 = 64; // 64 reflected lines -> 40 scroll off a 24-row screen

        let key = Key::random();
        let (server_conn, port) = Connection::server(ports, &key, Family::Inet).unwrap();
        let cmd: Vec<String> =
            vec!["/bin/sh".into(), "-c".into(), "stty -echo; exec cat".into()];
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[]).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || server_loop(server_conn, child, 24, 80));

        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut conn = Connection::client(addr, &key).unwrap();
        let mut fragmenter = Fragmenter::new();
        let mut assembly = FragmentAssembly::new();
        let mut outbox = InputOutbox::new();
        let cap = caps::Cap {
            id: caps::CAP_SCROLLBACK,
            payload: vec![0],
        };

        let mut applied_num = 0u64;
        let mut sb_bodies = 0usize;
        let mut sb_rows = 0usize;
        let mut saw_visible = false;
        let mut accumulated: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        let mut sb_applied = 0u64; // distinct scrollback frames actually applied
        let mut drop_target: Option<u64> = None;
        let mut lines_sent = 0u64;
        let mut applied_at_push = 0u64; // applied_num when the in-flight line went out
        let mut waiting = false; // a line is in flight, its output not yet applied
        let mut idle_polls = 0u32; // consecutive drains with no new applied frame
        let mut shutdown_sent = false;
        let mut saw_shutdown = false;

        let deadline = now_ms() + 25_000;
        while now_ms() < deadline {
            // Pace by OUTPUT, not input acks: feed the next line only once the
            // previous line's output has been applied (one line in flight, so
            // each scrolls a row separately and the server interleaves visible
            // and scrollback frames). Pacing on input acks would let the input
            // race far ahead of the output. Once all lines are sent and the
            // scrollback stream goes quiet, wind `cat` down through the shutdown
            // handshake — it never exits on its own.
            if !shutdown_sent && !waiting {
                if lines_sent < LINES {
                    lines_sent += 1;
                    outbox.push(format!("line {lines_sent}\n").as_bytes());
                    applied_at_push = applied_num;
                    waiting = true;
                } else if idle_polls >= 3 {
                    shutdown_sent = true;
                }
            }

            let flags = if shutdown_sent {
                sync::CLIENT_FLAG_SHUTDOWN
            } else {
                0
            };
            let msg = ClientMessage {
                flags,
                caps: if advertise { vec![cap.clone()] } else { vec![] },
                acked_frame: applied_num,
                rows: 24,
                cols: 80,
                input_base: outbox.base(),
                input: outbox.pending().to_vec(),
            };
            for frag in fragmenter.make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX) {
                let _ = conn.send(&frag.to_bytes());
            }
            if saw_shutdown {
                break; // the send above acked the shutdown frame
            }

            // Block (bounded) until the server reacts, then drain everything.
            let prev_applied = applied_num;
            let mut fds = [util::pollfd(conn.raw_fd(), libc::POLLIN)];
            let _ = util::poll(&mut fds, 200);
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
                        outbox.ack(frame.input_ack);
                        if frame.flags & sync::FLAG_SHUTDOWN != 0 {
                            saw_shutdown = true;
                        }
                        if frame.frame_num < applied_num {
                            continue; // stale (includes retransmits of a dropped frame)
                        }
                        match &frame.body {
                            FrameBody::Scrollback { base, rows } => {
                                if frame.frame_num == applied_num || *base != applied_num {
                                    continue; // duplicate, or not anchored at our state
                                }
                                if drop_target == Some(frame.frame_num) {
                                    continue; // a retransmission of the frame we "lost"
                                }
                                if lose_second_sb_frame && drop_target.is_none() && sb_applied == 1
                                {
                                    // Lose exactly the second scrollback frame.
                                    drop_target = Some(frame.frame_num);
                                    continue;
                                }
                                sb_bodies += 1;
                                sb_rows += rows.len();
                                for r in rows {
                                    if let Some(n) = row_line_number(r) {
                                        accumulated.insert(n);
                                    }
                                }
                                sb_applied += 1;
                                applied_num = frame.frame_num;
                            }
                            FrameBody::Full(_) | FrameBody::Diff { .. } => {
                                if frame.frame_num != applied_num {
                                    saw_visible = true;
                                    applied_num = frame.frame_num;
                                }
                            }
                            FrameBody::Empty => {}
                        }
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            if applied_num > prev_applied {
                idle_polls = 0;
            } else {
                idle_polls += 1;
            }
            if waiting && applied_num > applied_at_push {
                waiting = false; // this line's output landed; feed the next
            }
        }
        server.join().unwrap();
        ScrollbackOutcome {
            sb_bodies,
            sb_rows,
            saw_visible,
            lines: accumulated.into_iter().collect(),
        }
    }

    #[test]
    fn server_emits_no_scrollback_to_a_non_advertising_client() {
        // RFC 0002 §1: against a client that never advertised SCROLLBACK,
        // the frame stream carries only Full/Diff/Empty bodies, even though
        // the session scrolled many lines off the primary screen.
        let out = run_scrollback_session(false, false, (62500, 62599));
        assert_eq!(out.sb_bodies, 0, "non-advertiser received a scrollback body");
        assert_eq!(out.sb_rows, 0);
        assert!(out.lines.is_empty());
        assert!(out.saw_visible, "should still receive visible screen frames");
    }

    #[test]
    fn server_ships_scrollback_growth_bounded_by_growth_not_depth() {
        // RFC 0002 §2/§3: an advertising client accumulates the scrolled-off
        // rows in order. 64 reflected lines scroll 40 off a 24-row screen; each
        // row ships about once (anchored to the ack), so the accumulated set is
        // the contiguous off-screen history, NOT a depth-multiplied re-dump.
        let out = run_scrollback_session(true, false, (62600, 62699));
        assert!(out.sb_bodies > 0, "advertiser never received a scrollback body");
        assert!(out.saw_visible, "visible screen frames must still flow");
        assert!(
            out.lines.len() >= 30,
            "scrollback did not accumulate the off-screen history: {} rows",
            out.lines.len()
        );
        // Contiguous: an in-order history with no gaps.
        let (min, max) = (*out.lines.first().unwrap(), *out.lines.last().unwrap());
        assert_eq!(
            out.lines.len(),
            (max - min + 1) as usize,
            "accumulated history has a gap: {:?}",
            out.lines
        );
        // Growth-bounded: applied rows ≈ distinct lines (each shipped about
        // once), NOT a whole-ring re-dump per frame (O(depth × frames)) and NOT
        // line-discipline echo doubling (~2× the lines).
        assert!(
            out.sb_rows <= out.lines.len() + 4,
            "scrollback rows look re-dumped or echo-doubled, not growth-bounded: \
             {} body rows vs {} distinct lines",
            out.sb_rows,
            out.lines.len()
        );
    }

    #[test]
    fn scrollback_lost_frame_is_not_silently_confirmed_by_visible_ack() {
        // Review finding #1: a visible (Full/Diff) frame inherits
        // sb_total = sb_high, so when the client acks that visible frame the
        // server advances acked_sb_total past scrollback rows the visible
        // frame never carried. If the scrollback frame that *did* carry them
        // was lost — and the client leapfrogged it via a base-matching
        // visible frame — those rows are never re-shipped, leaving a permanent
        // hole in the client's accumulated history.
        let out = run_scrollback_session(true, true, (62700, 62799));
        let lines = out.lines;
        assert!(
            lines.len() >= 20,
            "not enough scrollback accumulated to exercise the bug: {} lines",
            lines.len()
        );
        let min = *lines.first().unwrap();
        let max = *lines.last().unwrap();
        let span = (max - min + 1) as usize;
        let missing: Vec<u64> = lines.windows(2).flat_map(|w| w[0] + 1..w[1]).collect();
        assert_eq!(
            lines.len(),
            span,
            "scrollback history has a hole within {min}..={max} (missing lines {missing:?}): \
             a lost scrollback frame's rows were silently confirmed by a visible-frame ack \
             (sb_total=sb_high) and never re-shipped (finding #1)",
        );
    }

    #[test]
    fn update_acks_rejects_frames_never_sent() {
        let current = FrameState {
            num: 3,
            data: b"current".to_vec(),
            sb_total: 7,
        };
        let mut outstanding = vec![
            FrameState {
                num: 1,
                data: b"one".to_vec(),
                sb_total: 2,
            },
            FrameState {
                num: 2,
                data: b"two".to_vec(),
                sb_total: 5,
            },
        ];
        let mut acked_num = 1u64;
        let mut acked_data = Some(b"one".to_vec());
        let mut acked_sb_total = 2u64;
        let msg = ClientMessage {
            flags: 0,
            caps: vec![],
            acked_frame: u64::MAX,
            rows: 24,
            cols: 80,
            input_base: 0,
            input: Vec::new(),
        };

        update_acks(
            &msg,
            &current,
            &mut outstanding,
            &mut acked_num,
            &mut acked_data,
            &mut acked_sb_total,
        );

        assert_eq!(acked_num, 1, "ack for a frame never sent must be ignored");
        assert_eq!(acked_data.as_deref(), Some(b"one".as_slice()));
        assert_eq!(acked_sb_total, 2, "scrollback ack must not advance either");
        assert_eq!(outstanding.len(), 2, "outstanding frames must be kept");

        // A legitimate ack of the newest frame still works, carrying its
        // scrollback coverage forward (RFC 0002 §2).
        let msg = ClientMessage {
            acked_frame: 3,
            ..msg
        };
        update_acks(
            &msg,
            &current,
            &mut outstanding,
            &mut acked_num,
            &mut acked_data,
            &mut acked_sb_total,
        );
        assert_eq!(acked_num, 3);
        assert_eq!(acked_data.as_deref(), Some(b"current".as_slice()));
        assert_eq!(acked_sb_total, 7, "acking a frame confirms its scrollback");
        assert!(outstanding.is_empty());
    }

    #[test]
    fn timeout_env_parsing() {
        std::env::remove_var("POSH_TEST_TMOUT_A");
        assert_eq!(timeout_env("POSH_TEST_TMOUT_A"), 0);
        std::env::set_var("POSH_TEST_TMOUT_B", "30");
        assert_eq!(timeout_env("POSH_TEST_TMOUT_B"), 30);
        std::env::set_var("POSH_TEST_TMOUT_C", "-5");
        assert_eq!(timeout_env("POSH_TEST_TMOUT_C"), 0);
        std::env::set_var("POSH_TEST_TMOUT_D", "junk");
        assert_eq!(timeout_env("POSH_TEST_TMOUT_D"), 0);
    }
}
