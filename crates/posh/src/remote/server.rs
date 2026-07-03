//! Roaming remote server (mosh-server port, simplified SSP): owns the PTY
//! and a posh_term::Terminal, and syncs screen state to the client as
//! dump_vt frames (full or diffed against the last client-acked frame).

use std::time::Instant;

use posh_term::Terminal;

use crate::overlay::{close_overlay, escape_command, Overlay};
use crate::pty;
use crate::remote::caps;
use crate::remote::crypto::Key;
use crate::remote::diag;
use crate::remote::display::Snapshot;
use crate::remote::framesync::FrameProducer;
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
/// #wedge organic watchdog: the model advancing (dirty) without a visible frame
/// for this long is abnormal — a healthy server frames within one send_interval
/// (<= 250ms). Persistence past this is the Case-A stall signature (model moved,
/// frame suppressed), which is otherwise indistinguishable from an idle session
/// by transport state. Set well clear of pacing + retransmit slack.
const WEDGE_STUCK_MS: u64 = 1000;

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

/// The server-side transport bootstrap shared by [`run`] (the legacy
/// Architecture-A inner-PTY server) and the single-model relay verb
/// (`main::cmd_server_relay`, RFC 0008 §3): the UTF-8 locale check, a fresh AEAD
/// key + bound UDP `Connection`, the `POSH IP`/`POSH CONNECT` handshake lines on
/// stdout, then the double-fork into the background. Returns `Ok(None)` in the
/// PARENT (the caller returns `Ok(())`) and `Ok(Some(conn))` in the detached
/// CHILD (the un-peered transport it goes on to drive; the peer is learned
/// in-loop / by the relay handshake). Factored out so both bootstraps produce a
/// byte-identical `POSH CONNECT` line and detach identically.
pub(crate) fn bootstrap_transport(
    port_range: Option<(u16, u16)>,
    family: Family,
) -> Result<Option<Connection>> {
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
        return Ok(None);
    }
    util::redirect_stdio_devnull();
    util::install_sigusr1_handler();
    // SIGUSR2 dumps live transport state on demand (remote::diag) — the only
    // way to introspect a wedged, already-running server without restarting it.
    util::install_sigusr2_handler();
    Ok(Some(conn))
}

pub fn run(
    port_range: Option<(u16, u16)>,
    family: Family,
    command: Option<Vec<String>>,
    agent_forward: bool,
) -> Result<()> {
    let Some(conn) = bootstrap_transport(port_range, family)? else {
        return Ok(()); // the detached parent
    };

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
    let mut stats = Stats::new("server");
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

    // The visible-frame production state machine (#100, posh-proto): frame
    // numbering, the acked diff/morph baseline (the dump bytes + the rendered
    // snapshot + off-Snapshot alt/dims), the `outstanding` retransmission
    // window, and the swappable DumpDiff/MorphDelta encoders. Frame 0 is the
    // implicit empty initial state shared with the client, so the very first
    // real frame can already be expressed as a diff. The producer owns the
    // per-frame state both visible and scrollback frames advance; the scrollback
    // *body* and the sb_total/floor/high accounting below stay here.
    let mut producer = FrameProducer::new(rows, cols);

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
    // Server transport-state piggyback (#6, diagnostic): when the peer advertises
    // CAP_DIAG (only in its debug posture) we attach our live frame/ack/pty state
    // to each frame so its SIGUSR2 dump can show the far side of a wedge.
    let mut peer_wants_diag = false;
    // Evolved-predictor metric forwarding (RFC 0007 §3): when the peer advertises
    // CAP_METRICS we attach the remote-host terminals, sampled at most every
    // METRICS_SAMPLE_INTERVAL ms (the /proc reads are not free).
    const METRICS_SAMPLE_INTERVAL_MS: u64 = 500;
    let mut peer_wants_metrics = false;
    let mut metrics_sample = crate::remote::hostmetrics::RemoteMetrics::default();
    let mut metrics_sampled_at: u64 = 0;
    // Server retransmit RATE (#11): the cumulative count at the last sample, and
    // the per-second rate derived from its delta over the sample window. NaN
    // until the second sample (the first has no window to divide over).
    let mut last_retransmits: u64 = 0;
    let mut metrics_retransmit_rate = f64::NAN;

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
    // #wedge organic watchdog. Auto-captures the Case-A stall (model advanced,
    // visible frame suppressed) to a lazily-opened per-pid sink, once per stall
    // episode, without the operator pre-arming logging. On by default; disable
    // with POSH_WEDGE_CAPTURE=0 (or off/false/no) if it ever misbehaves in prod.
    // `dirty_since` marks when the model went dirty-without-frame (0 = in sync);
    // `wedge_captured` bounds it to one capture per episode.
    let wedge_watchdog = !matches!(
        std::env::var("POSH_WEDGE_CAPTURE").as_deref(),
        Ok("0") | Ok("off") | Ok("false") | Ok("no")
    );
    let mut dirty_since: u64 = 0;
    let mut wedge_captured = false;
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
    // #wedge poll-wakeup aggregate (#83 Case B). The `pty_read` breadcrumb only
    // fires when a read HAPPENS; this counts the poll cycles that produced no
    // read, so an always-on log positively distinguishes the two Case-B causes.
    // Flushed ~1/s while a sink is open (`poll_log_at` = last flush). Fingerprint:
    // `wakes` climbing with `pty_pollin=0` and `gen` frozen = post-exit bytes
    // never signalled readable (source-quiet / below-master stall); `pty_pollin>0`
    // with `reads=0` = a drain bug (readable but not consumed).
    let mut poll_wakes: u64 = 0;
    let mut poll_pty_pollin: u64 = 0;
    let mut poll_pty_reads: u64 = 0;
    let mut poll_pty_bytes: u64 = 0;
    let mut poll_log_at: u64 = now_ms();

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
                current_num: producer.current_num(),
                acked_num: producer.acked_num(),
                outstanding: producer.outstanding_len(),
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
            if producer.acked_num() < producer.current_num() {
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

        // #wedge poll aggregate (#83 Case B): count this wake and whether the PTY
        // signalled readable, before the read branch consumes it.
        poll_wakes += 1;
        if pty_open && fds[pty_idx].revents & libc::POLLIN != 0 {
            poll_pty_pollin += 1;
        }

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
                    poll_pty_reads += 1;
                    poll_pty_bytes += n as u64;
                    let gen_before = term.generation();
                    term.process(&buf[..n]);
                    let responses = term.take_responses();
                    // #wedge breadcrumb (active whenever a sink is open — manual
                    // logging or an auto-capture episode). Did post-exit PTY bytes
                    // arrive and move the terminal model? A `gen X->X` here (or no
                    // line at all across a program exit) is Case B — the output
                    // never changed the model; a nonzero `resp` with no gen bump
                    // means the bytes were a pure terminal query awaiting a reply.
                    // A gen bump rules Case B out and points at the emission gate.
                    if util::log_active() {
                        util::log_write(
                            "wedge",
                            &format!(
                                "pty_read n={n} gen {gen_before}->{} resp={}B",
                                term.generation(),
                                responses.len()
                            ),
                        );
                    }
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

        // #wedge poll aggregate flush (#83 Case B): one compact line ~1/s while a
        // sink is open. `wakes` climbing with `pty_pollin=0` and `gen` frozen is
        // the source-quiet Case B; `pty_pollin>0` with `reads=0` is a drain bug.
        // Its absence (no lines at all) means the loop itself stopped cycling.
        if util::log_active() && now.saturating_sub(poll_log_at) >= 1000 {
            util::log_write(
                "wedge",
                &format!(
                    "poll wakes={poll_wakes} pty_pollin={poll_pty_pollin} \
                     reads={poll_pty_reads} bytes={poll_pty_bytes} pty_open={} gen={}",
                    pty_open as u8,
                    term.generation(),
                ),
            );
            poll_wakes = 0;
            poll_pty_pollin = 0;
            poll_pty_reads = 0;
            poll_pty_bytes = 0;
            poll_log_at = now;
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
                        // Advance the producer's acked baseline. A returned
                        // sb_total (the ack confirmed a frame we still hold)
                        // carries the client's scrollback coverage forward
                        // (RFC 0002 §2); a lost-base/rejected ack returns None and
                        // leaves acked_sb_total untouched.
                        if let Some(sb_total) = producer.ack(msg.acked_frame) {
                            acked_sb_total = acked_sb_total.max(sb_total);
                        }
                        // Force-resync (palette "Reset & resync"): the client is
                        // wedged rejecting diffs against a base it isn't at and the
                        // stale-ack -> Full auto-recovery did not fire. Applied
                        // AFTER producer.ack — which would otherwise repopulate the
                        // baseline from this very ack — so the drop sticks: with no
                        // diff base the encoder must emit a Full keyframe, and
                        // force_frame ships it even if the screen is static. The
                        // client applies a Full unconditionally, breaking the
                        // apply-stall; its ack then repopulates the baseline and
                        // incremental diffing resumes.
                        if msg.flags & sync::CLIENT_FLAG_RESYNC != 0 {
                            producer.drop_acked_base();
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
                        // CAP_DIAG (#6): per-message, like the others.
                        peer_wants_diag = caps::find(&msg.caps, caps::CAP_DIAG).is_some();
                        // CAP_METRICS (RFC 0007 §3): per-message, like the others.
                        peer_wants_metrics = caps::find(&msg.caps, caps::CAP_METRICS).is_some();
                        // A GP client wants the server-cost terminals (#11): turn
                        // on Stats instrumentation so dump_vt timing runs even
                        // with POSH_DEBUG_LOG off (the `dump_vt_us` terminal).
                        stats.set_gp_active(peer_wants_metrics);
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
                && producer.has_acked_base()
                && paced;
            // At most one fresh body per opportunity; when both are ready
            // (heavy output scrolling the screen) alternate so neither kind
            // starves the other.
            let make_scrollback = want_scrollback && (!want_visible || !last_was_sb);
            let make_visible = want_visible && !make_scrollback;

            // #wedge organic watchdog. The model advanced past the last framed
            // generation (dirty) yet this iteration emits no visible frame. A
            // healthy server clears dirty within a send_interval; persistence
            // past WEDGE_STUCK_MS is the Case-A stall (indistinguishable from an
            // idle session by transport state, so we arm on the persistence). On
            // the first stuck iteration we lazily open a per-pid sink and record
            // the gate vars; while any sink is open we log per-iteration detail;
            // on recovery we mark it. The sink is left open (it rotates at
            // LOG_MAX_SIZE) — reopening could truncate a prior incident.
            if dirty && !make_visible {
                if dirty_since == 0 {
                    dirty_since = now;
                }
                if wedge_watchdog
                    && !wedge_captured
                    && now.saturating_sub(dirty_since) >= WEDGE_STUCK_MS
                {
                    if !util::log_active() {
                        diag::enable_logging("server");
                    }
                    util::log_write(
                        "wedge",
                        &format!(
                            "STUCK {}ms: force_frame={} paced={} want_vis={} want_sb={} \
                             make_sb={} num={} acked={} send_age={} send_int={} gen={}",
                            now.saturating_sub(dirty_since),
                            force_frame as u8,
                            paced as u8,
                            want_visible as u8,
                            want_scrollback as u8,
                            make_scrollback as u8,
                            producer.current_num(),
                            producer.acked_num(),
                            now.saturating_sub(last_send),
                            conn.send_interval(),
                            src.generation(),
                        ),
                    );
                    wedge_captured = true;
                    // Surface the banner promptly: an empty ack-frame carries the
                    // fresh FLAG_WEDGE out now rather than waiting for the next
                    // heartbeat (num==acked here, so this hits the force_ack path).
                    force_ack = true;
                }
                if util::log_active() {
                    util::log_write(
                        "wedge",
                        &format!(
                            "frame_suppressed force_frame={} paced={} want_vis={} \
                             want_sb={} make_sb={} num={} acked={} send_age={} send_int={}",
                            force_frame as u8,
                            paced as u8,
                            want_visible as u8,
                            want_scrollback as u8,
                            make_scrollback as u8,
                            producer.current_num(),
                            producer.acked_num(),
                            now.saturating_sub(last_send),
                            conn.send_interval(),
                        ),
                    );
                }
            } else {
                if wedge_captured {
                    util::log_write(
                        "wedge",
                        &format!(
                            "CLEARED after {}ms: num={} gen={}",
                            now.saturating_sub(dirty_since),
                            producer.current_num(),
                            src.generation(),
                        ),
                    );
                }
                dirty_since = 0;
                wedge_captured = false;
            }

            if make_visible {
                last_gen = src.generation();
                force_frame = false;
                // The morph base for this frame (#15): the rendered state + the
                // off-Snapshot fields the keyframe rule reads. `src` is the
                // overlay terminal while an escape shell is active. A visible
                // frame carries no scrollback rows, so applying it leaves the
                // client at whatever scrollback it held at the diff base (the
                // acked frame): acked_sb_total, NOT sb_high. sb_high counts rows
                // put into a scrollback frame that may have been lost; if a
                // visible-frame ack confirmed those, the rows of a
                // dropped-then-superseded scrollback frame would never be
                // re-shipped (finding #1).
                producer.advance_visible(
                    stats.time_dump_vt(|| src.dump_vt()),
                    Snapshot::from_term(src),
                    src.is_alt_screen(),
                    (src.rows(), src.cols()),
                    acked_sb_total,
                );
                current_is_sb = false;
                last_was_sb = false;
                send_frame = true;
                fresh_frame = true;
            } else if make_scrollback {
                // The scrollback frame inherits the CONFIRMED visible base as its
                // diff base (the diff-base chain is unbroken across interleaved
                // frames; the morph base snapshot/alt/dims is inherited for the
                // same reason, #15). #95: the producer inherits the acked base,
                // NOT the latest current dump — under loss the latest visible dump
                // can be ahead of what the client holds, and acking this
                // scrollback frame would then push the diff base past a visible
                // frame the client never applied, staling its baseline. The
                // advance carries cur_sb_total as this frame's coverage; the
                // Scrollback body itself is built below. want_scrollback gates on
                // has_acked_base(); the producer's fallback is defensive.
                producer.advance_scrollback(cur_sb_total);
                sb_high = cur_sb_total;
                current_is_sb = true;
                last_was_sb = true;
                send_frame = true;
            } else if producer.acked_num() < producer.current_num()
                && now.saturating_sub(last_send) >= conn.rto()
            {
                send_frame = true;
                stats.record_retransmit();
            } else if now.saturating_sub(last_send) >= HEARTBEAT_INTERVAL {
                send_empty = true;
            } else if force_ack && producer.acked_num() >= producer.current_num() {
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
                    let frame_sb_total = producer.current_sb_total();
                    let grown = cur_sb_total.saturating_sub(frame_sb_total) as usize;
                    let end = ring_len.saturating_sub(grown);
                    let want =
                        frame_sb_total.saturating_sub(acked_sb_total.max(sb_floor)) as usize;
                    let appended = want.min(end);
                    let start = end - appended;
                    let rows: Vec<Vec<u8>> = (start..end)
                        .map(|i| term.dump_scrollback_row(i).unwrap_or_default())
                        .collect();
                    FrameBody::Scrollback {
                        base: producer.acked_num(),
                        rows,
                    }
                } else {
                    // Visible-frame body via the negotiated codec (#15). The
                    // producer encodes the current frame against its acked
                    // baseline (the byte-diff dump + the morph snapshot/alt/dims);
                    // DumpDiff reproduces today's behavior verbatim, MorphDelta
                    // emits a forward escape-delta or a Full keyframe.
                    let mut body = producer.encode_visible(peer_wants_morph);
                    // RFC 0006: stamp the diff base's checksum so the client can
                    // confirm it holds the same base before applying. The base is
                    // the acked dump the diff was computed against.
                    if peer_wants_base_sum {
                        if let Some(acked) = producer.acked_dump() {
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
                    let dump_len = producer.current_dump_len();
                    match &body {
                        FrameBody::Diff { diff, .. } => {
                            stats.record_frame_diff();
                            if fresh_frame {
                                stats.record_diff_frame(dump_len, diff.len());
                            }
                        }
                        FrameBody::Morph { escapes, .. } => {
                            stats.record_frame_diff();
                            if fresh_frame {
                                stats.record_diff_frame(dump_len, escapes.len());
                            }
                        }
                        FrameBody::Full(_) => {
                            stats.record_frame_full();
                            // A forced full dump (no baseline) is not a strategy
                            // choice, so it skips the per-strategy size sample —
                            // matching the inline path's None arm.
                            if fresh_frame && producer.has_acked_base() {
                                stats.record_full_frame(dump_len);
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
                // Server transport-state piggyback (#6): mirror exactly the
                // fields our own SIGUSR2 dump reports, so a client triaging a
                // wedge sees whether the server is still producing frames,
                // what it thinks is acked, how many are outstanding, whether
                // its terminal is changing, and whether the shell is alive.
                if peer_wants_diag {
                    extras.push(caps::encode_server_diag(&caps::ServerDiag {
                        current_num: producer.current_num(),
                        acked_num: producer.acked_num(),
                        term_gen: term.generation(),
                        outstanding: producer.outstanding_len() as u32,
                        pty_open,
                        // FDR 0004: forward the agent endpoint's state too
                        // when forwarding is active server-side (None == none).
                        agent: agent_endpoint.as_ref().map(|ep| ep.diag()),
                    }));
                }
                // Evolved-predictor remote metrics (RFC 0007 §3): sample the
                // host/app/proc signals (throttled — the /proc reads are not
                // free) and attach them. The foreground app is taken from the
                // session pty's foreground process group + the terminal title.
                if peer_wants_metrics {
                    let t = now_ms();
                    if metrics_sampled_at == 0
                        || t.saturating_sub(metrics_sampled_at) >= METRICS_SAMPLE_INTERVAL_MS
                    {
                        metrics_sample = crate::remote::hostmetrics::sample(
                            pty::foreground_pgid(child.master),
                            term.title(),
                            child.pid,
                        );
                        // Retransmit rate over the elapsed window (#11): the
                        // count delta since the last sample, per second. The
                        // first sample has no window, so the rate stays NaN.
                        let dt_ms = t.saturating_sub(metrics_sampled_at);
                        let cur_retransmits = stats.retransmits();
                        if metrics_sampled_at != 0 && dt_ms != 0 {
                            metrics_retransmit_rate =
                                cur_retransmits.saturating_sub(last_retransmits) as f64
                                    / (dt_ms as f64 / 1000.0);
                        }
                        last_retransmits = cur_retransmits;
                        metrics_sampled_at = t;
                    }
                    // Host terminals (throttled, cached) plus the two server-side
                    // counters (#11): the windowed retransmit_rate, and dump_vt_us
                    // read fresh (the cost of the dump this frame carries).
                    let host = metrics_sample.to_terminals();
                    extras.push(caps::encode_metrics([
                        host[0],
                        host[1],
                        host[2],
                        host[3],
                        host[4],
                        metrics_retransmit_rate,
                        stats.last_dump_vt_us() as f64,
                    ]));
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
                // #wedge organic watchdog: advertise an active capture episode so
                // the client can raise a sticky "stall detected" banner. Rides
                // every frame (incl. heartbeats) for the episode, so it reaches
                // the client even while the session content is stalled.
                let wedge_flag = if wedge_captured {
                    sync::FLAG_WEDGE
                } else {
                    0
                };
                let frame = ServerFrame {
                    flags: (if shutdown { sync::FLAG_SHUTDOWN } else { 0 })
                        | echo_flag
                        | overlay_flag
                        | server_log_flag
                        | wedge_flag,
                    caps: frame_caps,
                    frame_num: producer.current_num(),
                    input_ack: inbox.next_offset(),
                    echo_ack: echo.ack(),
                    body,
                };
                send_payload(&mut conn, &mut fragmenter, &frame.encode());
                last_send = now;
            }
        }
        stats.flush_server(
            now,
            conn.srtt(),
            conn.rto(),
            producer.outstanding_len(),
            conn.bytes_tx(),
        );

        if shutdown {
            // The shell has exited: announce it (frames now carry the
            // shutdown flag) and leave once the client confirmed the final
            // state and the echo ack caught up, or after the grace period.
            if !force_frame
                && !force_ack
                && term.generation() == last_gen
                && producer.acked_num() >= producer.current_num()
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
        producer.outstanding_len(),
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

pub(crate) fn send_payload(conn: &mut Connection, fragmenter: &mut Fragmenter, payload: &[u8]) {
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

    // End-to-end agent forwarding over the real transport (FDR 0004 item 7):
    // a REAL server_loop with a live AgentEndpoint, the REAL AgentClient proxy,
    // and a fake ssh-agent — the whole byte path the unit tests only cover in
    // pieces. Topology mirrors production:
    //
    //   agent consumer ──connect──▶ <server>/agent/sock   (AgentEndpoint)
    //         │                            │  AGENT_* caps over UDP frames
    //         │                            ▼
    //         │                     test-as-client ──▶ AgentClient ──connect──▶ fake agent
    //         ▼                                                                      │
    //   request bytes ───────────── round-trip ─────────────────────────────▶ canned reply
    //
    // A request written to the server's agent socket must traverse the endpoint,
    // the AGENT_DATA stream over real loopback UDP, the client proxy, reach the
    // fake agent, and the reply must come all the way back. #[ignore] (real PTY
    // + UDP + threads): run with `cargo test -p posh -- --ignored agent_forward`.
    #[test]
    #[ignore = "agent-forwarding E2E harness; run with --ignored"]
    fn agent_forward_round_trips_request_to_local_agent() {
        use crate::remote::agent::AgentEndpoint;
        use std::io::{Read, Write};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        const REQUEST: &[u8] = b"AGENT-REQUEST-PING";
        const REPLY: &[u8] = b"AGENT-REPLY-PONG-0123456789";

        // Short /tmp paths so the unix sockets stay within SUN_LEN.
        let pid = std::process::id();
        let server_base = PathBuf::from(format!("/tmp/posh-e2e-srv-{pid}"));
        std::fs::remove_dir_all(&server_base).ok();
        std::os::unix::fs::DirBuilderExt::mode(
            std::fs::DirBuilder::new().recursive(true),
            0o700,
        )
        .create(&server_base)
        .unwrap();
        let fake_agent_sock = PathBuf::from(format!("/tmp/posh-e2e-agent-{pid}.sock"));
        std::fs::remove_file(&fake_agent_sock).ok();
        let fake_agent_sock_cleanup = fake_agent_sock.clone();

        // (1) Fake ssh-agent: read the request, write the canned reply, close.
        let agent_listener = UnixListener::bind(&fake_agent_sock).unwrap();
        let agent_thread = std::thread::spawn(move || {
            if let Ok((mut s, _)) = agent_listener.accept() {
                let mut buf = vec![0u8; REQUEST.len()];
                if s.read_exact(&mut buf).is_ok() {
                    assert_eq!(buf, REQUEST, "fake agent saw the forwarded request");
                    let _ = s.write_all(REPLY);
                }
            }
        });

        // (2) Real server with a live agent endpoint, shell idling.
        let key = Key::random();
        let (server_conn, port) = Connection::server((62800, 62899), &key, Family::Inet).unwrap();
        let endpoint = AgentEndpoint::new(&server_base).unwrap();
        let agent_sock = endpoint.sock_path().to_path_buf();
        let cmd: Vec<String> = vec!["/bin/sh".into(), "-c".into(), "sleep 600".into()];
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[], None).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || {
            server_loop(server_conn, child, 24, 80, Some(endpoint));
        });

        // (3) Test-as-client: pump the transport + the agent proxy until the
        // round-trip completes or we time out. Runs on its own thread so the
        // main thread can drive the agent consumer concurrently.
        let done = Arc::new(AtomicBool::new(false));
        let client = {
            let done = done.clone();
            let source = fake_agent_sock.clone();
            std::thread::spawn(move || pump_agent_forward_client(key, port, source, done, None))
        };

        // (4) The agent consumer: connect to the SERVER's agent/sock (what a
        // forwarded `git push` would use as SSH_AUTH_SOCK), send the request,
        // read the reply. The endpoint claims the symlink at construction, so a
        // working server is connectable almost at once; a few seconds of retry
        // covers thread startup, and a miss past it fails fast (a broken path
        // must not hang the suite).
        let deadline = now_ms() + 5_000;
        let mut stream = loop {
            if let Ok(s) = UnixStream::connect(&agent_sock) {
                break s;
            }
            assert!(now_ms() < deadline, "agent/sock never became connectable");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        stream.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        stream.write_all(REQUEST).unwrap();
        let mut got = vec![0u8; REPLY.len()];
        let read_result = stream.read_exact(&mut got);

        // Tear everything down. The client + fake-agent threads join once the
        // done flag is set; the server thread runs an idle `sleep 600` shell
        // and is abandoned (it exits on PEER_TIMEOUT after we stop sending, or
        // when the test process ends — spawned threads do not block exit).
        done.store(true, Ordering::Relaxed);
        let _ = client.join();
        let _ = agent_thread.join();
        drop(server);
        std::fs::remove_dir_all(&server_base).ok();
        std::fs::remove_file(&fake_agent_sock_cleanup).ok();

        read_result.expect("reply must arrive back through the forwarded path");
        assert_eq!(
            got, REPLY,
            "the fake agent's reply round-tripped server endpoint -> UDP -> client proxy -> back"
        );
    }

    /// Client half of the agent-forwarding E2E harness: drives the posh
    /// transport as a client — advertises `AGENT_FORWARD` every message, relays
    /// `AGENT_DATA`/`AGENT_ACK` between the loopback UDP transport and a local
    /// `AgentClient` proxy that dials `source_sock`, and pumps the proxy's
    /// channels back onto the stream — until `done` is set. Mirrors the
    /// production client loop; the tests vary only what sits behind `source_sock`
    /// (a fake agent, or a real `ssh-agent`).
    fn pump_agent_forward_client(
        key: Key,
        port: u16,
        source_sock: std::path::PathBuf,
        done: std::sync::Arc<std::sync::atomic::AtomicBool>,
        roam: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) {
        use crate::remote::agent::AgentClient;
        use crate::remote::sync::AgentStream;
        use std::sync::atomic::Ordering;

        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut conn = Connection::client(addr, &key).unwrap();
        let mut fragmenter = Fragmenter::new();
        let mut assembly = FragmentAssembly::new();
        let mut proxy = AgentClient::new(source_sock);
        let mut stream = AgentStream::new();
        let mut agent_seen = false;

        while !done.load(Ordering::Relaxed) {
            // Roam injection (#17): when signalled, rebind to a new source port
            // mid-stream; the server re-pins to it on the next in-sequence
            // datagram, and the cumulative agent stream rides the new path.
            if let Some(r) = &roam {
                if r.swap(false, Ordering::Relaxed) {
                    let _ = conn.roam_rebind();
                }
            }
            // Advertise AGENT_FORWARD every message; emit DATA/ACK once the
            // server has advertised back (mirrors outgoing_caps).
            let mut extras = vec![caps::Cap {
                id: caps::CAP_AGENT_FORWARD,
                payload: vec![],
            }];
            if agent_seen {
                extras.extend(caps::encode_agent_data(stream.send_base(), stream.pending()));
                extras.push(caps::encode_agent_ack(stream.recv_ack()));
            }
            let msg = ClientMessage {
                flags: 0,
                caps: caps::own_table(&extras),
                acked_frame: 0,
                rows: 24,
                cols: 80,
                input_base: 0,
                input: Vec::new(),
            };
            for frag in fragmenter.make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX) {
                let _ = conn.send(&frag.to_bytes());
            }
            // Drain inbound frames; consume the server's agent caps.
            while let Ok(Some(payload)) = conn.recv() {
                let Ok(frag) = sync::Fragment::from_bytes(&payload) else {
                    continue;
                };
                let Some(assembled) = assembly.add(frag) else {
                    continue;
                };
                let Ok(frame) = ServerFrame::decode(&assembled) else {
                    continue;
                };
                if caps::find(&frame.caps, caps::CAP_AGENT_FORWARD).is_some() {
                    agent_seen = true;
                }
                for cap in caps::find_all(&frame.caps, caps::CAP_AGENT_DATA) {
                    if let Ok((offset, bytes)) = caps::decode_agent_data(&cap.payload) {
                        if let Ok(records) = stream.recv(offset, bytes) {
                            for reply in proxy.apply_records(&records) {
                                stream.send(&reply);
                            }
                        }
                    }
                }
                if let Some(cap) = caps::find(&frame.caps, caps::CAP_AGENT_ACK) {
                    if let Ok(upto) = caps::decode_agent_ack(&cap.payload) {
                        stream.ack(upto);
                    }
                }
            }
            // Pump the proxy's local-agent connections -> outbound stream.
            for rec in proxy.read_channels() {
                stream.send(&rec);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    // Like the harness above, but with a REAL `ssh-agent` behind the forwarded
    // socket and a REAL `ssh-add -l` as the consumer: proves the actual SSH agent
    // protocol (not just opaque bytes) survives the endpoint -> UDP -> proxy ->
    // agent round-trip, and that the SPECIFIC forwarded key is the one listed.
    // Shells out to ssh-keygen/ssh-agent/ssh-add (absent from the hermetic build
    // sandbox), so it is #[ignore]; run with `just debug-agent-e2e`.
    #[test]
    #[ignore = "real ssh-agent E2E; needs ssh-keygen/ssh-agent/ssh-add; run with --ignored"]
    fn agent_forward_real_ssh_agent_lists_forwarded_key() {
        use crate::remote::agent::AgentEndpoint;
        use std::os::unix::fs::DirBuilderExt;
        use std::path::PathBuf;
        use std::process::{Command, Stdio};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let base = PathBuf::from(format!("/tmp/posh-e2e-real-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)
            .unwrap();

        // (1) An ephemeral key the real agent will hold, plus the SHA256
        // fingerprint `ssh-add -l` must report once it round-trips.
        let key_path = base.join("id_ed25519");
        assert!(
            Command::new("ssh-keygen")
                // Fixed -C comment: the default embeds $USER@$HOSTNAME, which
                // would leak the runner's identity into captured test output.
                .args(["-t", "ed25519", "-N", "", "-C", "posh-agent-e2e", "-q", "-f"])
                .arg(&key_path)
                .status()
                .expect("run ssh-keygen")
                .success(),
            "ssh-keygen failed"
        );
        let fp_out = Command::new("ssh-keygen")
            .arg("-lf")
            .arg(key_path.with_extension("pub"))
            .output()
            .expect("run ssh-keygen -lf");
        let fp_text = String::from_utf8_lossy(&fp_out.stdout);
        let fingerprint = fp_text
            .split_whitespace()
            .find(|t| t.starts_with("SHA256:"))
            .expect("a SHA256 fingerprint in ssh-keygen -lf output")
            .to_string();

        // (2) A real ssh-agent bound to a known socket, foreground (-D) so we own
        // the child and reap it on teardown; load the key into it.
        let real_agent_sock = base.join("real-agent.sock");
        let mut agent = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&real_agent_sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ssh-agent");
        let deadline = now_ms() + 5_000;
        while !real_agent_sock.exists() {
            assert!(now_ms() < deadline, "ssh-agent never bound its socket");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            Command::new("ssh-add")
                .arg(&key_path)
                .env("SSH_AUTH_SOCK", &real_agent_sock)
                .status()
                .expect("run ssh-add")
                .success(),
            "ssh-add failed to load the key into the real agent"
        );

        // (3) Real posh server with a live agent endpoint + an idling shell.
        let server_base = base.join("srv");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&server_base)
            .unwrap();
        let key = Key::random();
        let (server_conn, port) = Connection::server((62700, 62799), &key, Family::Inet).unwrap();
        let endpoint = AgentEndpoint::new(&server_base).unwrap();
        let forwarded_sock = endpoint.sock_path().to_path_buf();
        let cmd: Vec<String> = vec!["/bin/sh".into(), "-c".into(), "sleep 600".into()];
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[], None).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || {
            server_loop(server_conn, child, 24, 80, Some(endpoint));
        });

        // (4) Client transport pump whose proxy dials the REAL ssh-agent.
        let done = Arc::new(AtomicBool::new(false));
        let client = {
            let done = done.clone();
            let source = real_agent_sock.clone();
            std::thread::spawn(move || pump_agent_forward_client(key, port, source, done, None))
        };

        // (5) `ssh-add -l` against the FORWARDED socket must list the key. Retry
        // the whole command — forwarding takes a few hundred ms to come up
        // (symlink claim + the client advertising back + the proxy connecting) —
        // and bound each attempt with `timeout` so a broken path fails fast
        // instead of blocking on the agent read.
        let deadline = now_ms() + 10_000;
        let mut last = String::new();
        let mut listed = false;
        while now_ms() < deadline {
            let out = Command::new("timeout")
                .arg("4")
                .arg("ssh-add")
                .arg("-l")
                .env("SSH_AUTH_SOCK", &forwarded_sock)
                .output()
                .expect("run ssh-add -l");
            last = String::from_utf8_lossy(&out.stdout).into_owned();
            if out.status.success() && last.contains(&fingerprint) {
                listed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // (6) Teardown before asserting, so a failure still cleans up.
        done.store(true, Ordering::Relaxed);
        let _ = client.join();
        let _ = agent.kill();
        let _ = agent.wait();
        drop(server);
        std::fs::remove_dir_all(&base).ok();

        assert!(
            listed,
            "ssh-add -l via the forwarded socket never listed the forwarded key \
             (fingerprint {fingerprint}); last stdout: {last:?}"
        );
    }

    // Like the real-ssh-agent test, but the server is the REAL `posh server -A`
    // BINARY — a detached, double-forked process — instead of an in-thread
    // server_loop. This proves the actual server CLI path stands up a working
    // forwarded endpoint: arg parsing, `AgentEndpoint::from_env` reading
    // POSH_DIR, and the SSH_AUTH_SOCK export. The client stays the in-process
    // pump: the raw `posh client` carries NO forwarding (main.rs) — that
    // orchestration lives only in the ssh-bootstrapped `posh host` path, which
    // needs a real sshd and is covered by the manual walkthrough (FDR 0004,
    // docs/manual-testing.md), not an automated test. #[ignore]: spawns the
    // binary + needs ssh tooling; run with `just debug-agent-e2e`.
    #[test]
    #[ignore = "real posh-server process E2E; needs the posh binary + ssh tooling; run with --ignored"]
    fn agent_forward_real_server_process_lists_key() {
        use std::io::BufRead;
        use std::os::unix::fs::DirBuilderExt;
        use std::path::PathBuf;
        use std::process::{Command, Stdio};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        // The posh binary cargo builds alongside this test: target/<profile>/posh,
        // the sibling of the unit-test exe in target/<profile>/deps/.
        let posh_bin = {
            let exe = std::env::current_exe().expect("current_exe");
            exe.parent()
                .and_then(|p| p.parent())
                .expect("target/<profile> dir")
                .join("posh")
        };
        assert!(
            posh_bin.exists(),
            "posh binary not found at {posh_bin:?} (cargo test should have built it)"
        );

        let base = PathBuf::from(format!("/tmp/posh-srvproc-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)
            .unwrap();

        // (1) An ephemeral key in a real ssh-agent — the LOCAL agent the pump
        // proxies (the client side of forwarding).
        let key_path = base.join("id_ed25519");
        assert!(
            Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-N", "", "-C", "posh-srvproc-e2e", "-q", "-f"])
                .arg(&key_path)
                .status()
                .expect("run ssh-keygen")
                .success(),
            "ssh-keygen failed"
        );
        let fp_out = Command::new("ssh-keygen")
            .arg("-lf")
            .arg(key_path.with_extension("pub"))
            .output()
            .expect("run ssh-keygen -lf");
        let fp_text = String::from_utf8_lossy(&fp_out.stdout);
        let fingerprint = fp_text
            .split_whitespace()
            .find(|t| t.starts_with("SHA256:"))
            .expect("a SHA256 fingerprint")
            .to_string();
        let local_agent_sock = base.join("local-agent.sock");
        let mut agent = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&local_agent_sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ssh-agent");
        let deadline = now_ms() + 5_000;
        while !local_agent_sock.exists() {
            assert!(now_ms() < deadline, "ssh-agent never bound its socket");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            Command::new("ssh-add")
                .arg(&key_path)
                .env("SSH_AUTH_SOCK", &local_agent_sock)
                .status()
                .expect("run ssh-add")
                .success(),
            "ssh-add failed to load the key into the local agent"
        );

        // (2) The REAL `posh server -A` binary. POSH_DIR fixes the forwarded
        // agent/sock path; it prints `POSH CONNECT <port> <key>` then double-forks
        // into a detached server (which inherits the bound UDP socket + the env).
        let server_dir = base.join("srv");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&server_dir)
            .unwrap();
        let mut server = Command::new(&posh_bin)
            .args(["server", "-p", "62400:62499", "-A", "--", "sleep", "600"])
            .env("LC_ALL", "C.UTF-8")
            .env("POSH_DIR", &server_dir)
            // The detached server is double-forked (unreapable here); it
            // self-terminates this long after the pump stops sending.
            .env("POSH_SERVER_NETWORK_TMOUT", "10")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn posh server");
        let connect = std::io::BufReader::new(server.stdout.take().expect("server stdout piped"))
            .lines()
            .map_while(|line| line.ok())
            .find(|l| l.starts_with("POSH CONNECT "))
            .expect("posh server printed POSH CONNECT");
        let _ = server.wait(); // the parent exits right after the double-fork
        let mut fields = connect
            .strip_prefix("POSH CONNECT ")
            .expect("POSH CONNECT prefix")
            .split_whitespace();
        let port: u16 = fields.next().expect("port").parse().expect("port number");
        let key = Key::from_base64(fields.next().expect("key")).expect("valid base64 key");

        // (3) The in-process pump as the client, proxying the real ssh-agent.
        let done = Arc::new(AtomicBool::new(false));
        let client = {
            let done = done.clone();
            let source = local_agent_sock.clone();
            std::thread::spawn(move || pump_agent_forward_client(key, port, source, done, None))
        };

        // (4) `ssh-add -l` against the forwarded socket must list the key.
        let forwarded_sock = server_dir.join("agent").join("sock");
        let deadline = now_ms() + 15_000;
        let mut last = String::new();
        let mut listed = false;
        while now_ms() < deadline {
            let out = Command::new("timeout")
                .arg("4")
                .arg("ssh-add")
                .arg("-l")
                .env("SSH_AUTH_SOCK", &forwarded_sock)
                .output()
                .expect("run ssh-add -l");
            last = String::from_utf8_lossy(&out.stdout).into_owned();
            if out.status.success() && last.contains(&fingerprint) {
                listed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }

        // (5) Teardown. The detached server is not directly reapable (double-fork);
        // it self-terminates on POSH_SERVER_NETWORK_TMOUT once the pump stops.
        done.store(true, Ordering::Relaxed);
        let _ = client.join();
        let _ = agent.kill();
        let _ = agent.wait();
        std::fs::remove_dir_all(&base).ok();

        assert!(
            listed,
            "ssh-add -l via the real-server-process forwarded socket never listed \
             the key (fingerprint {fingerprint}); last stdout: {last:?}"
        );
    }

    // Agent forwarding must survive a network roam — the FDR 0004 `stable`
    // criterion. The in-process pump establishes forwarding to a real ssh-agent
    // and a real `ssh-add -l` lists the key; then the client ROAMS (rebinds to a
    // new source port mid-stream), the server re-pins to it, and a second
    // `ssh-add -l` must STILL list the key (the cumulative agent stream rides the
    // new path). #[ignore]: needs ssh tooling; run with `just debug-agent-e2e`.
    #[test]
    #[ignore = "agent-forwarding roam survival; needs ssh tooling; run with --ignored"]
    fn agent_forward_survives_roam() {
        use crate::remote::agent::AgentEndpoint;
        use std::os::unix::fs::DirBuilderExt;
        use std::path::{Path, PathBuf};
        use std::process::{Command, Stdio};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let base = PathBuf::from(format!("/tmp/posh-roam-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)
            .unwrap();

        // A real ssh-agent holding an ephemeral key.
        let key_path = base.join("id_ed25519");
        assert!(
            Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-N", "", "-C", "posh-roam-e2e", "-q", "-f"])
                .arg(&key_path)
                .status()
                .expect("run ssh-keygen")
                .success(),
            "ssh-keygen failed"
        );
        let fp_out = Command::new("ssh-keygen")
            .arg("-lf")
            .arg(key_path.with_extension("pub"))
            .output()
            .expect("run ssh-keygen -lf");
        let fp_text = String::from_utf8_lossy(&fp_out.stdout);
        let fingerprint = fp_text
            .split_whitespace()
            .find(|t| t.starts_with("SHA256:"))
            .expect("a SHA256 fingerprint")
            .to_string();
        let agent_sock = base.join("agent.sock");
        let mut agent = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&agent_sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ssh-agent");
        let deadline = now_ms() + 5_000;
        while !agent_sock.exists() {
            assert!(now_ms() < deadline, "ssh-agent never bound its socket");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            Command::new("ssh-add")
                .arg(&key_path)
                .env("SSH_AUTH_SOCK", &agent_sock)
                .status()
                .expect("run ssh-add")
                .success(),
            "ssh-add failed to load the key into the agent"
        );

        // In-thread server with a live agent endpoint + idling shell.
        let server_base = base.join("srv");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&server_base)
            .unwrap();
        let key = Key::random();
        let (server_conn, port) = Connection::server((62600, 62699), &key, Family::Inet).unwrap();
        let endpoint = AgentEndpoint::new(&server_base).unwrap();
        let forwarded_sock = endpoint.sock_path().to_path_buf();
        let cmd: Vec<String> = vec!["/bin/sh".into(), "-c".into(), "sleep 600".into()];
        let child = crate::pty::spawn_shell(Some(&cmd), 24, 80, &[], None).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let server = std::thread::spawn(move || {
            server_loop(server_conn, child, 24, 80, Some(endpoint));
        });

        // The pump, with a roam trigger wired in.
        let done = Arc::new(AtomicBool::new(false));
        let roam = Arc::new(AtomicBool::new(false));
        let client = {
            let done = done.clone();
            let roam = roam.clone();
            let source = agent_sock.clone();
            std::thread::spawn(move || {
                pump_agent_forward_client(key, port, source, done, Some(roam))
            })
        };

        // `ssh-add -l` against the forwarded socket lists the key (retrying
        // through the forwarding/roam settle window, each attempt timeout-bounded).
        let lists_key = |sock: &Path| -> bool {
            let deadline = now_ms() + 12_000;
            while now_ms() < deadline {
                let out = Command::new("timeout")
                    .arg("4")
                    .arg("ssh-add")
                    .arg("-l")
                    .env("SSH_AUTH_SOCK", sock)
                    .output()
                    .expect("run ssh-add -l");
                if out.status.success()
                    && String::from_utf8_lossy(&out.stdout).contains(&fingerprint)
                {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            false
        };

        let before = lists_key(&forwarded_sock);
        // Roam: the pump rebinds to a new source port; the server re-pins on its
        // next in-sequence datagram. A brief settle, then probe again.
        roam.store(true, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let after = lists_key(&forwarded_sock);

        // Teardown before asserting, so a failure still cleans up.
        done.store(true, Ordering::Relaxed);
        let _ = client.join();
        let _ = agent.kill();
        let _ = agent.wait();
        drop(server);
        std::fs::remove_dir_all(&base).ok();

        assert!(before, "ssh-add -l did not list the key before the roam");
        assert!(
            after,
            "ssh-add -l did not list the key AFTER the roam — forwarding did not survive"
        );
    }
}
