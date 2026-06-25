//! Roaming remote server (mosh-server port, simplified SSP): owns the PTY
//! and a posh_term::Terminal, and syncs screen state to the client as
//! dump_vt frames (full or diffed against the last client-acked frame).

use std::time::Instant;

use posh_term::Terminal;

use crate::pty;
use crate::remote::caps;
use crate::remote::crypto::Key;
use crate::remote::diag;
use crate::remote::display::Snapshot;
use crate::remote::framesync::{Baseline, CurrentFrame, DumpDiff, FrameEncoder, MorphDelta};
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
    agent_forward: bool,
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
    // SIGUSR2 dumps live transport state on demand (remote::diag) — the only
    // way to introspect a wedged, already-running server without restarting it.
    util::install_sigusr2_handler();

    // Agent forwarding (FDR 0004): when active, stand up the remote endpoint
    // (the agent/sock the session shell will use as SSH_AUTH_SOCK) before the
    // shell is spawned, so its env carries the right value from birth (C5). A
    // best-effort failure here (e.g. a hardened-dir rejection) just leaves the
    // session without forwarding rather than refusing to start it.
    let agent_endpoint = if agent_forward {
        match crate::remote::agent::AgentEndpoint::from_env() {
            Ok(ep) => Some(ep),
            Err(e) => {
                util::log_write("warn", &format!("agent forwarding disabled: {e}"));
                None
            }
        }
    } else {
        None
    };

    let (rows, cols) = (24u16, 80u16);
    // posh#51: the ssh bootstrap allocates no remote pty, so sshd set no TERM;
    // terminfo::session_env gives the session shell a resolved TERM (+ the
    // client's COLORTERM) so color-by-$TERM tools (git, Charmbracelet TUIs)
    // aren't left colorless.
    let mut shell_env = crate::terminfo::session_env();
    if let Some(ep) = &agent_endpoint {
        // C5: a session created through a forwarding connection inherits
        // SSH_AUTH_SOCK pointing at the stable agent/sock.
        shell_env.push((
            "SSH_AUTH_SOCK".to_string(),
            ep.sock_path().to_string_lossy().into_owned(),
        ));
    }
    let child = pty::spawn_shell(command.as_deref(), rows, cols, &shell_env, None)?;
    util::set_nonblocking(child.master)?;

    server_loop(conn, child, rows, cols, agent_endpoint);
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
    /// The rendered screen state as of this frame — the morph base for a later
    /// `Morph` (#15), captured alongside `data` so acking this frame gives the
    /// MorphDelta encoder both bases. Carried for the same reason as `data` and
    /// inherited identically by a scrollback frame.
    snapshot: Snapshot,
    /// Off-`Snapshot` terminal state at this frame: whether the alt screen is
    /// active and the dimensions. The MorphDelta encoder reads these to detect
    /// a transition a morph cannot express (alt-screen toggle, resize) and fall
    /// back to a `Full` keyframe (#15).
    alt_screen: bool,
    dims: (u16, u16),
    /// Scrollback rows the client will have accumulated after applying this
    /// frame (RFC 0002): the running high-water that only advances on a
    /// scrollback frame. Acking this frame tells the server the client holds
    /// scrollback through here, so the next body's appended count starts
    /// from it.
    sb_total: u64,
}

/// A transient escape-to-shell overlay (FDR 0008): a second PTY running the
/// configured escape command in the session's cwd, with its own terminal model.
/// While present it is the broadcast source and the input sink; the live session
/// keeps running underneath (read into the main `term`, just not broadcast), and
/// is repainted when the overlay's shell exits.
struct Overlay {
    child: pty::PtyChild,
    term: Terminal,
}

/// `$POSH_ESCAPE_CMD` parsed into argv (whitespace-split; `sc exec` and most
/// commands need nothing fancier). `None` (unset/blank) means spawn `$SHELL` as
/// a login shell — the same default as the session shell.
fn escape_command() -> Option<Vec<String>> {
    std::env::var("POSH_ESCAPE_CMD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.split_whitespace().map(str::to_string).collect())
}

/// Tear down an active escape overlay: hang up its shell's process group, reap
/// it, and close the master fd. No-op when there is no overlay.
fn close_overlay(overlay: &mut Option<Overlay>) {
    if let Some(o) = overlay.take() {
        util::kill_pgroup(o.child.pid, libc::SIGHUP);
        let _ = util::try_reap(o.child.pid);
        util::close_fd(o.child.master);
    }
}

pub(crate) fn server_loop(
    mut conn: Connection,
    child: pty::PtyChild,
    rows: u16,
    cols: u16,
    mut agent_endpoint: Option<crate::remote::agent::AgentEndpoint>,
) {
    // Optional perf instrumentation (POSH_DEBUG_LOG). run() has already
    // double-forked and redirected stdio to /dev/null, so this file fd is the
    // server's only viable diagnostic sink; inert when the env var is unset.
    let mut stats = Stats::new();
    let mut term = Terminal::new(rows, cols);
    let mut fragmenter = Fragmenter::new();
    let mut assembly = FragmentAssembly::new();
    let mut inbox = InputInbox::new();
    let mut echo = EchoAck::new();

    // Escape-to-shell overlay (FDR 0008): the command to spawn, a cwd fallback
    // for when the session never reported an OSC-7 pwd, and the live overlay.
    let escape_cmd = escape_command();
    let fallback_cwd: String = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .or_else(|| std::env::var("HOME").ok())
        .unwrap_or_else(|| "/".to_string());
    let mut overlay: Option<Overlay> = None;

    // Idle timeouts (seconds; 0 = never). NETWORK fires on its own; SIGNAL
    // only fires when SIGUSR1 has been received.
    let network_tmout = timeout_env("POSH_SERVER_NETWORK_TMOUT") * 1000;
    let signal_tmout = timeout_env("POSH_SERVER_SIGNAL_TMOUT") * 1000;

    // Frame 0 is the implicit empty initial state shared with the client, so
    // the very first real frame can already be expressed as a diff.
    let mut current = FrameState {
        num: 0,
        data: Vec::new(),
        snapshot: Snapshot::blank(rows, cols),
        alt_screen: false,
        dims: (rows, cols),
        sb_total: 0,
    };
    // Last frame the client confirmed; None data means we no longer have its
    // bytes and must send a full dump. `acked_baseline` mirrors `acked_data`
    // for the MorphDelta encoder: the rendered snapshot + off-Snapshot state
    // (alt-screen, dims) at the acked frame, so a morph can be built against
    // it and a non-expressible transition detected (#15). It is Some exactly
    // when `acked_data` is.
    let mut acked_num: u64 = 0;
    let mut acked_data: Option<Vec<u8>> = Some(Vec::new());
    let mut acked_baseline: Option<(Snapshot, bool, (u16, u16))> =
        Some((Snapshot::blank(rows, cols), false, (rows, cols)));
    let mut outstanding: Vec<FrameState> = Vec::new();

    // Frame-sync codec negotiation (#15): the client advertises CAP_MORPH only
    // behind POSH_FRAMESYNC=morph. `peer_wants_morph` tracks the latest
    // message's advertisement (caps do not persist); when set we encode visible
    // frames with MorphDelta, else DumpDiff (today's behavior). Both encoders
    // are held so selection is a per-frame choice, not a reallocation.
    let mut peer_wants_morph = false;
    // Base-integrity (RFC 0006): when the peer advertises CAP_BASE_SUM we stamp
    // each visible Diff/Morph with a checksum of its diff base, so the client can
    // detect a divergent base and resync instead of mis-applying it (#94).
    let mut peer_wants_base_sum = false;
    let mut dumpdiff_enc = DumpDiff;
    let mut morph_enc = MorphDelta::default();

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

    // Agent forwarding (FDR 0004). The bidirectional agent byte stream and the
    // peer's per-message AGENT_FORWARD advertisement. `agent_seen` latches once
    // the peer has advertised, so we may emit AGENT_DATA/AGENT_ACK (RFC 0001:
    // never before seeing the peer's AGENT_FORWARD). The stream + endpoint are
    // inert unless `agent_endpoint` is Some.
    let mut agent_stream = sync::AgentStream::new();
    let mut agent_seen = false;

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
        let iter_start = stats.enabled().then(Instant::now);
        let now = now_ms();
        // A silent peer is forgotten after a minute: sending stops (the
        // session stays alive) until an authentic datagram arrives again.
        let peer_active = conn.has_remote() && now.saturating_sub(last_heard) < PEER_TIMEOUT;
        if network_tmout > 0 && now.saturating_sub(last_heard) >= network_tmout {
            break; // POSH_SERVER_NETWORK_TMOUT expired: give up the session
        }
        // Agent-forwarding maintenance (FDR 0004): symlink takeover, dead-sock
        // GC, and a stricter peer-liveness gate (AGENT_PEER_ACTIVE, 15 s — vs
        // the loop's 60 s PEER_TIMEOUT) so a roamed-away peer fast-fails a
        // blocked `git push` rather than hanging it. Any channels the tick
        // closes are framed back to the client.
        if let Some(ep) = agent_endpoint.as_mut() {
            let agent_peer_active = conn.has_remote()
                && now.saturating_sub(last_heard) < crate::remote::agent::AGENT_PEER_ACTIVE;
            for rec in ep.tick(agent_peer_active, now) {
                agent_stream.send(&rec);
                force_ack = true;
            }
        }
        if util::take_flag(&util::SIGUSR1_RECEIVED)
            && signal_tmout > 0
            && now.saturating_sub(last_heard) >= signal_tmout
        {
            break; // signaled and idle long enough
        }
        // SIGUSR2: snapshot live transport state to the diagnostic sink. The
        // peer address + last-heard/last-send ages + acked-vs-current here are
        // what distinguish a roam we haven't re-pinned from one-way packet loss.
        if util::take_flag(&util::SIGUSR2_RECEIVED) {
            diag::ServerState {
                peer_active,
                has_remote: conn.has_remote(),
                remote: conn.remote(),
                last_heard_age_ms: now.saturating_sub(last_heard),
                last_send_age_ms: (last_send != 0).then(|| now.saturating_sub(last_send)),
                current_num: current.num,
                acked_num,
                outstanding: outstanding.len(),
                srtt: conn.srtt(),
                rto: conn.rto(),
                send_interval: conn.send_interval(),
                bytes_rx: conn.bytes_rx(),
                bytes_tx: conn.bytes_tx(),
                term_gen: term.generation(),
                pty_open,
            }
            .dump();
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
        let pty_idx = if pty_open {
            fds.push(util::pollfd(child.master, libc::POLLIN));
            fds.len() - 1
        } else {
            usize::MAX
        };
        let overlay_idx = if let Some(o) = &overlay {
            fds.push(util::pollfd(o.child.master, libc::POLLIN));
            fds.len() - 1
        } else {
            usize::MAX
        };
        // Agent-forwarding fds (FDR 0004): the listener then each open channel,
        // in the order `AgentEndpoint::pollfds` returns. `agent_fd_base` is the
        // index of the first; `usize::MAX` when forwarding is inactive.
        let (agent_fd_base, agent_fd_count) = match &agent_endpoint {
            Some(ep) => {
                let agent_fds = ep.pollfds();
                let base = fds.len();
                fds.extend_from_slice(&agent_fds);
                (base, agent_fds.len())
            }
            None => (usize::MAX, 0),
        };
        let poll_start = stats.enabled().then(Instant::now);
        match util::poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        let idle_us = poll_start.map_or(0, |t| t.elapsed().as_micros() as u64);

        // Session shell output -> the main terminal model. Read even while an
        // overlay is up so the live session stays current underneath; it just
        // isn't broadcast until the overlay closes.
        if pty_open && fds[pty_idx].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
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
                // The whole session is ending: tear down any escape overlay.
                close_overlay(&mut overlay);
            }
        }

        // Escape-overlay shell output -> the overlay terminal model (the active
        // broadcast source). On EOF/EIO the shell exited: drop the overlay and
        // force a frame so the live session repaints.
        if overlay_idx != usize::MAX
            && fds[overlay_idx].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
        {
            let mut closed = false;
            if let Some(o) = overlay.as_mut() {
                let mut buf = [0u8; 4096];
                match util::read_fd(o.child.master, &mut buf) {
                    Ok(0) => closed = true,
                    Ok(n) => {
                        o.term.process(&buf[..n]);
                        let responses = o.term.take_responses();
                        if !responses.is_empty() {
                            let _ = util::write_all_retry(o.child.master, &responses, 100);
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => closed = true,
                }
            }
            if closed {
                close_overlay(&mut overlay);
                force_frame = true; // repaint the restored session
            }
        }

        // Agent-forwarding sockets (FDR 0004): accept new connections and read
        // agent-client bytes, framing both as records onto the outbound stream.
        // `read_channels` already scans every channel, so a single signalled
        // agent fd suffices to drive it; the listener is the base index.
        if agent_fd_base != usize::MAX {
            if let Some(ep) = agent_endpoint.as_mut() {
                let agent_revents = (agent_fd_base..agent_fd_base + agent_fd_count)
                    .any(|i| fds[i].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0);
                if agent_revents {
                    for rec in ep.accept_pending() {
                        agent_stream.send(&rec);
                    }
                    for rec in ep.read_channels() {
                        agent_stream.send(&rec);
                        force_ack = true; // pace agent chunks promptly (design §2)
                    }
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
                        last_heard = now_ms();
                        if caps::find(&msg.caps, caps::CAP_EXIT_STATUS).is_some() {
                            peer_wants_exit = true;
                        }
                        // Agent forwarding (FDR 0004): consume the peer's agent
                        // caps into the stream + endpoint. AGENT_FORWARD latches
                        // `agent_seen` (gates our own AGENT_DATA/ACK); AGENT_DATA
                        // chunks feed the inbox -> decoder -> channel writes;
                        // AGENT_ACK drains our outbox. Only meaningful when the
                        // endpoint exists; a decoder error tears it down (a
                        // corrupt authenticated stream is unrecoverable).
                        if let Some(ep) = agent_endpoint.as_mut() {
                            if caps::find(&msg.caps, caps::CAP_AGENT_FORWARD).is_some() {
                                agent_seen = true;
                            }
                            let mut decode_failed = false;
                            for cap in caps::find_all(&msg.caps, caps::CAP_AGENT_DATA) {
                                let Ok((offset, bytes)) = caps::decode_agent_data(&cap.payload)
                                else {
                                    decode_failed = true;
                                    break;
                                };
                                match agent_stream.recv(offset, bytes) {
                                    Ok(records) => ep.apply_records(&records),
                                    Err(_) => {
                                        decode_failed = true;
                                        break;
                                    }
                                }
                            }
                            if let Some(cap) = caps::find(&msg.caps, caps::CAP_AGENT_ACK) {
                                if let Ok(upto) = caps::decode_agent_ack(&cap.payload) {
                                    agent_stream.ack(upto);
                                }
                            }
                            if decode_failed {
                                agent_endpoint = None; // drop the endpoint + channels
                                agent_seen = false;
                            }
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
                            overlay.as_mut(),
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
                        // Server-side debug logging toggle (#3): the palette's
                        // "Server debug logging" command. Idempotent set bits open
                        // / close the per-pid sink and flip the stats collector;
                        // the resulting state rides back as FLAG_SERVER_LOG.
                        if msg.flags & sync::CLIENT_FLAG_LOG_ON != 0 && !util::log_active() {
                            diag::enable_logging("server");
                            stats.set_enabled(true);
                        }
                        if msg.flags & sync::CLIENT_FLAG_LOG_OFF != 0 && util::log_active() {
                            util::log_disable();
                            stats.set_enabled(false);
                        }
                        // Escape-to-shell (FDR 0008): spawn the overlay once per
                        // request. Sticky flag + the is_none() guard make
                        // retransmits idempotent; the client stops once it sees
                        // FLAG_OVERLAY echoed back.
                        if msg.flags & sync::CLIENT_FLAG_ESCAPE != 0
                            && overlay.is_none()
                            && !shutdown
                        {
                            let cwd = if term.pwd().is_empty() {
                                fallback_cwd.clone()
                            } else {
                                term.pwd().to_string()
                            };
                            match pty::spawn_shell(
                                escape_cmd.as_deref(),
                                client_size.0,
                                client_size.1,
                                &crate::terminfo::session_env(),
                                Some(&cwd),
                            ) {
                                Ok(oc) => {
                                    let _ = util::set_nonblocking(oc.master);
                                    overlay = Some(Overlay {
                                        child: oc,
                                        term: Terminal::new(client_size.0, client_size.1),
                                    });
                                    force_frame = true;
                                }
                                Err(e) => util::log_write(
                                    "error",
                                    &format!("escape-to-shell spawn failed: {e}"),
                                ),
                            }
                        }
                        update_acks(
                            &msg,
                            &current,
                            &mut outstanding,
                            &mut acked_num,
                            &mut acked_data,
                            &mut acked_baseline,
                            &mut acked_sb_total,
                        );
                        // Force-resync (palette "Reset & resync"): the client is
                        // wedged rejecting diffs against a base it isn't at and the
                        // stale-ack -> Full auto-recovery did not fire. Applied
                        // AFTER update_acks — which would otherwise repopulate the
                        // baseline from this very ack — so the drop sticks: with no
                        // diff base the encoder must emit a Full keyframe, and
                        // force_frame ships it even if the screen is static. The
                        // client applies a Full unconditionally, breaking the
                        // apply-stall; its ack then repopulates the baseline and
                        // incremental diffing resumes.
                        if msg.flags & sync::CLIENT_FLAG_RESYNC != 0 {
                            acked_data = None;
                            acked_baseline = None;
                            force_frame = true;
                        }
                        let now_wants = caps::find(&msg.caps, caps::CAP_SCROLLBACK).is_some();
                        if now_wants && !peer_wants_scrollback {
                            // (Re)activation: accumulate forward from here.
                            sb_floor = term.primary_scrollback_total();
                            sb_high = sb_high.max(sb_floor);
                        }
                        peer_wants_scrollback = now_wants;
                        // CAP_MORPH (#15): per-message, like scrollback.
                        peer_wants_morph = caps::find(&msg.caps, caps::CAP_MORPH).is_some();
                        peer_wants_base_sum =
                            caps::find(&msg.caps, caps::CAP_BASE_SUM).is_some();
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
            // Broadcast source: the overlay terminal while an escape shell is up
            // (FDR 0008), else the live session. The session still updates `term`
            // underneath either way; only what we frame here changes.
            let src: &Terminal = overlay.as_ref().map(|o| &o.term).unwrap_or(&term);
            let dirty = src.generation() != last_gen;
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
                && overlay.is_none() // the transient overlay carries no scrollback
                && !term.is_alt_screen()
                && !force_frame
                && !shutdown
                && cur_sb_total > sb_high
                && cur_sb_total > acked_sb_total.max(sb_floor)
                // #95: never emit scrollback before a visible baseline is
                // confirmed. A scrollback frame carries the acked visible dump
                // forward as its diff base; with no acked baseline it would leap
                // applied_num from the empty initial state past the (unapplied)
                // first Full, staling the client's visible baseline -> apply-stall.
                && acked_data.is_some()
                && paced;
            // At most one fresh body per opportunity; when both are ready
            // (heavy output scrolling the screen) alternate so neither kind
            // starves the other.
            let make_scrollback = want_scrollback && (!want_visible || !last_was_sb);
            let make_visible = want_visible && !make_scrollback;

            if make_visible {
                last_gen = src.generation();
                force_frame = false;
                outstanding.push(FrameState {
                    num: current.num,
                    data: std::mem::take(&mut current.data),
                    snapshot: std::mem::replace(&mut current.snapshot, Snapshot::blank(1, 1)),
                    alt_screen: current.alt_screen,
                    dims: current.dims,
                    sb_total: current.sb_total,
                });
                if outstanding.len() > 8 {
                    outstanding.remove(0);
                }
                current = FrameState {
                    num: current.num + 1,
                    data: stats.time_dump_vt(|| src.dump_vt()),
                    // The morph base for this frame (#15): the rendered state +
                    // the off-Snapshot fields the keyframe rule reads. `src` is
                    // the overlay terminal while an escape shell is active.
                    snapshot: Snapshot::from_term(src),
                    alt_screen: src.is_alt_screen(),
                    dims: (src.rows(), src.cols()),
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
                // The scrollback frame inherits a visible dump as its diff base
                // (the diff-base chain is unbroken across interleaved frames; the
                // morph base snapshot/alt/dims is inherited for the same reason,
                // #15). #95: it MUST inherit the CONFIRMED baseline (acked_data /
                // acked_baseline), NOT the latest `current.data`. Under loss the
                // latest visible dump can be ahead of what the client holds, so
                // acking this scrollback frame (which advances applied_num but not
                // the client's visible content) would push the server's visible
                // diff base past a visible frame the client never applied, leaving
                // its baseline stale -> every later visible Diff short-bases ->
                // permanent apply-stall. Anchoring to acked_data pins the diff
                // base to the last visible state the client actually confirmed.
                // want_scrollback gates on acked_data.is_some(); the fallback is
                // defensive and preserves the old behavior if that ever changes.
                let (visible, visible_snapshot, visible_alt, visible_dims) =
                    match (acked_data.clone(), acked_baseline.clone()) {
                        (Some(d), Some((s, a, dim))) => (d, s, a, dim),
                        _ => (
                            current.data.clone(),
                            current.snapshot.clone(),
                            current.alt_screen,
                            current.dims,
                        ),
                    };
                outstanding.push(FrameState {
                    num: current.num,
                    data: std::mem::take(&mut current.data),
                    snapshot: std::mem::replace(&mut current.snapshot, Snapshot::blank(1, 1)),
                    alt_screen: current.alt_screen,
                    dims: current.dims,
                    sb_total: current.sb_total,
                });
                if outstanding.len() > 8 {
                    outstanding.remove(0);
                }
                current = FrameState {
                    num: current.num + 1,
                    data: visible,
                    snapshot: visible_snapshot,
                    alt_screen: visible_alt,
                    dims: visible_dims,
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
                    // Visible-frame body via the negotiated codec (#15). The
                    // acked baseline (Some exactly when acked_data is) gives the
                    // encoder both the byte-diff base (dump) and the morph base
                    // (snapshot + off-Snapshot alt/dims). DumpDiff reproduces
                    // today's behavior verbatim; MorphDelta emits a forward
                    // escape-delta or a Full keyframe.
                    let baseline = acked_data.as_ref().zip(acked_baseline.as_ref()).map(
                        |(dump, (snapshot, alt, dims))| Baseline {
                            num: acked_num,
                            dump: dump.clone(),
                            snapshot: snapshot.clone(),
                            alt_screen: *alt,
                            rows: dims.0,
                            cols: dims.1,
                        },
                    );
                    let cur = CurrentFrame {
                        dump: &current.data,
                        snapshot: &current.snapshot,
                        alt_screen: current.alt_screen,
                        rows: current.dims.0,
                        cols: current.dims.1,
                    };
                    let mut body = if peer_wants_morph {
                        morph_enc.encode(baseline.as_ref(), &cur)
                    } else {
                        dumpdiff_enc.encode(baseline.as_ref(), &cur)
                    };
                    // RFC 0006: stamp the diff base's checksum so the client can
                    // confirm it holds the same base before applying. The base is
                    // the acked dump the diff was computed against (acked_data).
                    if peer_wants_base_sum {
                        if let Some(acked) = acked_data.as_deref() {
                            // Diff only: the client's applied_data IS the DumpDiff
                            // base, so a byte checksum verifies it. A Morph base is
                            // a snapshot, not the client's held dump bytes, so the
                            // byte checksum does not apply (Morph base_sum stays
                            // None -- the field is reserved for a future snapshot
                            // checksum).
                            if let FrameBody::Diff { base_sum, .. } = &mut body {
                                *base_sum = Some(sync::base_checksum(acked));
                            }
                        }
                    }
                    // Diff economics sampling, preserved from the inline path:
                    // a diff-shaped body (Diff/Morph) is the incremental case;
                    // Full is the keyframe. The fresh_frame gate keeps
                    // retransmits out of the per-strategy size sample.
                    match &body {
                        FrameBody::Diff { diff, .. } => {
                            stats.record_frame_diff();
                            if fresh_frame {
                                stats.record_diff_frame(current.data.len(), diff.len());
                            }
                        }
                        FrameBody::Morph { escapes, .. } => {
                            stats.record_frame_diff();
                            if fresh_frame {
                                stats.record_diff_frame(current.data.len(), escapes.len());
                            }
                        }
                        FrameBody::Full(_) => {
                            stats.record_frame_full();
                            // A forced full dump (no baseline) is not a strategy
                            // choice, so it skips the per-strategy size sample —
                            // matching the inline path's None arm.
                            if fresh_frame && baseline.is_some() {
                                stats.record_full_frame(current.data.len());
                            }
                        }
                        _ => {}
                    }
                    body
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
                // Agent forwarding (FDR 0004): advertise AGENT_FORWARD whenever
                // the endpoint is up so the peer may begin; emit AGENT_DATA
                // chunks + AGENT_ACK only once the peer has advertised back
                // (RFC 0001: not before seeing the peer's AGENT_FORWARD).
                if agent_endpoint.is_some() {
                    extras.push(caps::Cap {
                        id: caps::CAP_AGENT_FORWARD,
                        payload: vec![],
                    });
                    if agent_seen {
                        extras.extend(caps::encode_agent_data(
                            agent_stream.send_base(),
                            agent_stream.pending(),
                        ));
                        extras.push(caps::encode_agent_ack(agent_stream.recv_ack()));
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
                // Echo state of the ACTIVE pty — the overlay shell while it is up
                // (FDR 0008), else the session — so optimistic echo tracks
                // whichever the client is typing into.
                let active_master = overlay
                    .as_ref()
                    .map(|o| o.child.master)
                    .unwrap_or(child.master);
                let echo_flag = if (overlay.is_some() || pty_open) && pty::echo_on(active_master) {
                    sync::FLAG_ECHO
                } else {
                    0
                };
                // Tell the client an escape overlay is active so it stops
                // retransmitting CLIENT_FLAG_ESCAPE (the request was honored).
                let overlay_flag = if overlay.is_some() {
                    sync::FLAG_OVERLAY
                } else {
                    0
                };
                // Report the server's debug-logging state so the client's "Server
                // debug logging" palette command shows the truth (#3).
                let server_log_flag = if util::log_active() {
                    sync::FLAG_SERVER_LOG
                } else {
                    0
                };
                let frame = ServerFrame {
                    flags: (if shutdown { sync::FLAG_SHUTDOWN } else { 0 })
                        | echo_flag
                        | overlay_flag
                        | server_log_flag,
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

        // Per-iteration loop timing (perf instrumentation): busy = the whole
        // iteration minus the poll wait.
        if let Some(start) = iter_start {
            let total = start.elapsed().as_micros() as u64;
            stats.record_loop_iter(total.saturating_sub(idle_us), idle_us);
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
    close_overlay(&mut overlay);
}

#[allow(clippy::too_many_arguments)]
fn handle_client_message(
    msg: &ClientMessage,
    term: &mut Terminal,
    child: &pty::PtyChild,
    pty_open: bool,
    overlay: Option<&mut Overlay>,
    inbox: &mut InputInbox,
    echo: &mut EchoAck,
    client_size: &mut (u16, u16),
    force_ack: &mut bool,
) {
    // Split the overlay borrow into its fd (Copy) and its terminal (&mut) so
    // resize can touch both and input can be routed without a second borrow.
    let (ov_master, ov_term) = match overlay {
        Some(o) => (Some(o.child.master), Some(&mut o.term)),
        None => (None, None),
    };
    if msg.rows > 0 && msg.cols > 0 && (msg.rows, msg.cols) != *client_size {
        *client_size = (msg.rows, msg.cols);
        pty::set_term_size(child.master, msg.rows, msg.cols);
        term.resize(msg.rows, msg.cols);
        // Keep the overlay sized to the client too (FDR 0008).
        if let Some(m) = ov_master {
            pty::set_term_size(m, msg.rows, msg.cols);
        }
        if let Some(t) = ov_term {
            t.resize(msg.rows, msg.cols);
        }
    }
    if let Some(new_input) = inbox.accept(msg.input_base, &msg.input) {
        // Input goes to the active pty: the overlay shell while it is up, else
        // the session.
        match ov_master {
            Some(m) => {
                let _ = util::write_all_retry(m, new_input, 500);
            }
            None if pty_open => {
                let _ = util::write_all_retry(child.master, new_input, 500);
            }
            None => {}
        }
        echo.record(inbox.next_offset(), now_ms());
        *force_ack = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn update_acks(
    msg: &ClientMessage,
    current: &FrameState,
    outstanding: &mut Vec<FrameState>,
    acked_num: &mut u64,
    acked_data: &mut Option<Vec<u8>>,
    acked_baseline: &mut Option<(Snapshot, bool, (u16, u16))>,
    acked_sb_total: &mut u64,
) {
    // Ignore acks for frames never sent: an authenticated client claiming a
    // future frame would otherwise clear `outstanding`, disable retransmits,
    // and satisfy the shutdown gate without confirming the real final state.
    if msg.acked_frame <= *acked_num || msg.acked_frame > current.num {
        return;
    }
    *acked_num = msg.acked_frame;
    // The acked frame's bytes, morph base (snapshot + off-Snapshot alt/dims),
    // and scrollback total, from `current` or the retained outstanding frame.
    let acked = if msg.acked_frame == current.num {
        Some((
            current.data.clone(),
            current.snapshot.clone(),
            current.alt_screen,
            current.dims,
            current.sb_total,
        ))
    } else {
        outstanding.iter().find(|f| f.num == msg.acked_frame).map(|f| {
            (
                f.data.clone(),
                f.snapshot.clone(),
                f.alt_screen,
                f.dims,
                f.sb_total,
            )
        })
    };
    if let Some((data, snapshot, alt, dims, sb_total)) = acked {
        *acked_data = Some(data);
        // The morph baseline tracks acked_data exactly (#15): both Some, or
        // both None when we no longer hold the acked frame's state.
        *acked_baseline = Some((snapshot, alt, dims));
        // A frame's sb_total is the scrollback the client holds after applying
        // it: a scrollback frame advances it by the rows it carries; a visible
        // frame inherits the acked base's total (it carries no rows). So acking
        // any frame confirms only scrollback the client actually received, even
        // when a scrollback frame was lost and leapfrogged (RFC 0002 §2/§3).
        *acked_sb_total = (*acked_sb_total).max(sb_total);
    } else {
        *acked_data = None;
        *acked_baseline = None;
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
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[], None).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || server_loop(server_conn, child, 24, 80, None));

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
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[], None).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || server_loop(server_conn, child, 24, 80, None));

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
    fn escape_flag_spawns_and_tears_down_a_shell_overlay() {
        // FDR 0008: CLIENT_FLAG_ESCAPE makes the server spawn a shell overlay
        // whose frames carry FLAG_OVERLAY; sending that shell `exit\n` closes it
        // (FLAG_OVERLAY clears); then a shutdown winds the session down. Covers
        // the spawn, the broadcast-source swap, input routing to the overlay,
        // and teardown — the parts no unit test reaches.
        let key = Key::random();
        let (server_conn, port) = Connection::server((62300, 62399), &key, Family::Inet).unwrap();
        // The session shell just idles; the overlay is the unit under test.
        let cmd: Vec<String> = vec!["/bin/sh".into(), "-c".into(), "sleep 600".into()];
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[], None).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || server_loop(server_conn, child, 24, 80, None));

        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut conn = Connection::client(addr, &key).unwrap();
        let mut fragmenter = Fragmenter::new();
        let mut assembly = FragmentAssembly::new();
        let mut outbox = InputOutbox::new();

        let mut acked_frame = 0u64;
        let mut saw_overlay = false;
        let mut overlay_cleared = false;
        let mut sent_exit = false;
        let mut shutting_down = false;
        let mut saw_shutdown = false;
        let deadline = now_ms() + 15_000;
        while now_ms() < deadline {
            let flags = if shutting_down {
                sync::CLIENT_FLAG_SHUTDOWN
            } else if !saw_overlay {
                sync::CLIENT_FLAG_ESCAPE // request until the overlay appears
            } else {
                0
            };
            let msg = ClientMessage {
                flags,
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
                        if frame.flags & sync::FLAG_OVERLAY != 0 {
                            saw_overlay = true;
                        } else if saw_overlay {
                            overlay_cleared = true; // overlay shell exited
                        }
                        if frame.flags & sync::FLAG_SHUTDOWN != 0 {
                            saw_shutdown = true;
                        }
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            // Once the overlay is up, send its shell `exit`; once it has closed,
            // wind the whole session down.
            if saw_overlay && !sent_exit {
                outbox.push(b"exit\n");
                sent_exit = true;
            }
            if overlay_cleared {
                shutting_down = true;
            }
        }

        assert!(saw_overlay, "never saw FLAG_OVERLAY after CLIENT_FLAG_ESCAPE");
        assert!(overlay_cleared, "overlay never closed after `exit`");
        assert!(saw_shutdown, "server never wound down");
        server.join().unwrap();
    }

    #[test]
    fn server_logging_toggle_reports_state() {
        // #3: CLIENT_FLAG_LOG_ON makes the server enable its debug logging and
        // report FLAG_SERVER_LOG on its frames; CLIENT_FLAG_LOG_OFF clears it.
        // Uses the process-global log sink, so it restores the prior state on exit.
        let restore = util::log_active();
        let key = Key::random();
        let (server_conn, port) = Connection::server((62600, 62699), &key, Family::Inet).unwrap();
        let cmd: Vec<String> = vec!["/bin/sh".into(), "-c".into(), "sleep 600".into()];
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[], None).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || server_loop(server_conn, child, 24, 80, None));

        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut conn = Connection::client(addr, &key).unwrap();
        let mut fragmenter = Fragmenter::new();
        let mut assembly = FragmentAssembly::new();

        let mut acked = 0u64;
        let mut saw_log_on = false;
        let mut saw_log_off = false;
        let mut shutting = false;
        let mut saw_shutdown = false;
        let deadline = now_ms() + 15_000;
        while now_ms() < deadline {
            let flags = if shutting {
                sync::CLIENT_FLAG_SHUTDOWN
            } else if !saw_log_on {
                sync::CLIENT_FLAG_LOG_ON
            } else if !saw_log_off {
                sync::CLIENT_FLAG_LOG_OFF
            } else {
                0
            };
            let msg = ClientMessage {
                flags,
                caps: vec![],
                acked_frame: acked,
                rows: 24,
                cols: 80,
                input_base: 0,
                input: vec![],
            };
            for frag in fragmenter.make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX) {
                conn.send(&frag.to_bytes()).unwrap();
            }
            if saw_shutdown && acked > 0 {
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
                        acked = acked.max(frame.frame_num);
                        if frame.flags & sync::FLAG_SERVER_LOG != 0 {
                            saw_log_on = true;
                        } else if saw_log_on {
                            saw_log_off = true; // cleared after having been on
                        }
                        if frame.flags & sync::FLAG_SHUTDOWN != 0 {
                            saw_shutdown = true;
                        }
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            if saw_log_off {
                shutting = true;
            }
        }
        if !restore {
            util::log_disable(); // restore the global log state for other tests
        }
        assert!(
            saw_log_on,
            "server never reported FLAG_SERVER_LOG after CLIENT_FLAG_LOG_ON"
        );
        assert!(
            saw_log_off,
            "server never cleared FLAG_SERVER_LOG after CLIENT_FLAG_LOG_OFF"
        );
        assert!(saw_shutdown, "server never wound down");
        server.join().unwrap();
    }

    #[test]
    fn resync_flag_forces_a_full_keyframe() {
        // #wedge: CLIENT_FLAG_RESYNC (the palette "Reset & resync" command) makes
        // the server drop its acked baseline and emit a fresh Full keyframe even
        // when the screen is static — the manual unwedge for an apply-stall. We
        // ack the handshake frame, send RESYNC, then require a LATER Full (a
        // frame_num beyond the one acked when we asked), proving it is a freshly
        // forced keyframe and not the initial handshake Full retransmitted.
        let key = Key::random();
        let (server_conn, port) = Connection::server((62700, 62799), &key, Family::Inet).unwrap();
        // Emit one line so a visible frame is actually produced (a blank screen
        // never changes generation, so the server would only send heartbeats and
        // `acked` would never advance to trigger the resync), then idle.
        let cmd: Vec<String> = vec!["/bin/sh".into(), "-c".into(), "echo resync; sleep 600".into()];
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[], None).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || server_loop(server_conn, child, 24, 80, None));

        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut conn = Connection::client(addr, &key).unwrap();
        let mut fragmenter = Fragmenter::new();
        let mut assembly = FragmentAssembly::new();

        let mut acked = 0u64;
        let mut resync_at: Option<u64> = None;
        let mut saw_resync_full = false;
        let mut shutting = false;
        let mut saw_shutdown = false;
        let deadline = now_ms() + 15_000;
        while now_ms() < deadline {
            // Once we've acked the handshake frame, ask for a resync; once the
            // forced Full comes back, wind the session down.
            let want_resync = resync_at.is_none() && acked >= 1 && !saw_resync_full;
            if want_resync {
                resync_at = Some(acked);
            }
            let flags = if shutting || saw_resync_full {
                shutting = true;
                sync::CLIENT_FLAG_SHUTDOWN
            } else if want_resync {
                sync::CLIENT_FLAG_RESYNC
            } else {
                0
            };
            let msg = ClientMessage {
                flags,
                caps: vec![],
                acked_frame: acked,
                rows: 24,
                cols: 80,
                input_base: 0,
                input: vec![],
            };
            for frag in fragmenter.make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX) {
                conn.send(&frag.to_bytes()).unwrap();
            }
            if saw_shutdown {
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
                        acked = acked.max(frame.frame_num);
                        if let Some(at) = resync_at {
                            if frame.frame_num > at && matches!(frame.body, FrameBody::Full(_)) {
                                saw_resync_full = true;
                            }
                        }
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
        assert!(
            saw_resync_full,
            "server never sent a fresh Full keyframe after CLIENT_FLAG_RESYNC"
        );
        assert!(saw_shutdown, "server never wound down");
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
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[], None).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || server_loop(server_conn, child, 24, 80, None));

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
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[], None).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || server_loop(server_conn, child, 24, 80, None));

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
                            // Morph never arrives here (this harness's client
                            // does not advertise CAP_MORPH), but the match must
                            // be total; treat it as the visible-frame case.
                            FrameBody::Full(_)
                            | FrameBody::Diff { .. }
                            | FrameBody::Morph { .. } => {
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
        // A bare FrameState for the ack bookkeeping test: the morph base fields
        // (#15) are present but their values are immaterial here — this test
        // exercises acked_num/acked_data/acked_sb_total movement.
        let frame = |num: u64, data: &[u8], sb_total: u64| FrameState {
            num,
            data: data.to_vec(),
            snapshot: Snapshot::blank(24, 80),
            alt_screen: false,
            dims: (24, 80),
            sb_total,
        };
        let current = frame(3, b"current", 7);
        let mut outstanding = vec![frame(1, b"one", 2), frame(2, b"two", 5)];
        let mut acked_num = 1u64;
        let mut acked_data = Some(b"one".to_vec());
        let mut acked_baseline = Some((Snapshot::blank(24, 80), false, (24u16, 80u16)));
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
            &mut acked_baseline,
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
            &mut acked_baseline,
            &mut acked_sb_total,
        );
        assert_eq!(acked_num, 3);
        assert_eq!(acked_data.as_deref(), Some(b"current".as_slice()));
        // The morph baseline tracks acked_data: both Some after a real ack (#15).
        assert!(acked_baseline.is_some(), "morph baseline tracks acked_data");
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
