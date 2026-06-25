//! Roaming remote client (mosh-client/stmclient port): raw-mode tty, a
//! reliable input stream upload, a local terminal model rebuilt from
//! server frames, speculative local echo (predict.rs), and a minimal-diff
//! renderer (display.rs) so frames morph the screen without flicker.

use std::collections::VecDeque;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Instant;

use posh_term::{Color, Screen, Style, Terminal};
use serde_json::{json, Value};

use crate::pty::{self, RawMode};
use crate::remote::caps;
use crate::remote::crypto::Key;
use crate::remote::datagram::{Connection, Family};
use crate::remote::diag;
use crate::remote::display::{self, NotificationEngine, Snapshot};
use crate::remote::framesync::{self, ApplyOutcome, FrameApplier};
use crate::remote::palette::{Palette, PaletteEvent};
use crate::remote::predict::{
    self, PredictionModel, PredictionRenderer, Predictor, RenderStyle,
};
use crate::remote::stats::{FrameKind, PredictSample, Stats};
use crate::remote::sync::{
    self, ClientMessage, FragmentAssembly, Fragmenter, FrameBody, InputOutbox, ScrollbackRing,
    ServerFrame, HEARTBEAT_INTERVAL,
};
use crate::util::{self, now_ms, Error, Result};

const STDIN: i32 = libc::STDIN_FILENO;
const STDOUT: i32 = libc::STDOUT_FILENO;
const SHUTDOWN_GRACE: u64 = 5000; // ms to wait for the shutdown ack

/// Depth of the client's local scrollback ring (RFC 0002 §3), in rows.
/// Matches the server's default primary ring so a durable local reader can
/// hold roughly what the server syncs; bounds client memory.
const SCROLLBACK_RING_DEPTH: usize = 10_000;

/// The escape (quit-sequence) key: Ctrl-^ (0x1E), as in mosh.
const ESCAPE_KEY: u8 = 0x1e;
const ESCAPE_PASS_KEY: u8 = b'^';

/// Banner shown only when Ctrl-^ can't open the palette (renderer missing or
/// wedged): the degraded escape prefix, where Ctrl-^ . still quits.
const PALETTE_FALLBACK_HELP: &str =
    "command palette unavailable — \".\" quits, \"^\" gives literal Ctrl-^";

/// Rebuild the predictor/renderer for `next` in place and force a clean repaint
/// so stale predicted cells clear; banners the new model. Backs the palette's
/// `echo.set` action.
fn apply_echo_model(st: &mut ClientState, next: PredictionModel, now: u64) {
    let (predict, renderer) = predict::build(next, st.predict_render, st.predict_overwrite);
    st.predict = predict;
    st.renderer = renderer;
    st.predict_model = next;
    st.initialized = false;
    st.notify.set_message(&format!("echo: {next:?}"), false, now);
}

/// Set client-local debug logging to `enabled`: open the default per-pid sink
/// (reusing the SIGUSR2-dump path scheme) or close it, and flip the stats
/// collector so the periodic transport summaries flow while it is on. A no-op
/// (no banner) when already in the requested state.
fn set_logging(st: &mut ClientState, enabled: bool, now: u64) {
    if enabled == util::log_active() {
        return;
    }
    if enabled {
        let path = diag::enable_logging("client");
        st.stats.set_enabled(true);
        st.notify
            .set_message(&format!("debug logging: on ({})", path.display()), false, now);
    } else {
        util::log_disable();
        st.stats.set_enabled(false);
        st.notify.set_message("debug logging: off", false, now);
    }
}

/// The palette's command list (RFC 0005 §5): the discoverable surface for the
/// escape commands. Ctrl-^ opens this in lieu of a key-prefix menu, so every
/// escape action lives here. The logging entries reflect the current state —
/// client logging from this process, server logging from the last frame's
/// FLAG_SERVER_LOG (`server_log_on`).
fn palette_commands(server_log_on: bool) -> Value {
    // Imperative labels (the verb is the action): "on"/"off" read ambiguously as
    // status, so a user who saw "…: on" assumed it was already enabled.
    let (client_log_name, client_log_enabled): (&str, bool) = if util::log_active() {
        ("Disable client debug logging", false)
    } else {
        ("Enable client debug logging", true)
    };
    let (server_log_name, server_log_enabled): (&str, bool) = if server_log_on {
        ("Disable server debug logging", false)
    } else {
        ("Enable server debug logging", true)
    };
    json!([
        { "name": "Echo: adaptive", "action": { "method": "echo.set", "params": { "model": "adaptive" } } },
        { "name": "Echo: optimistic", "action": { "method": "echo.set", "params": { "model": "optimistic" } } },
        { "name": "Echo: always", "action": { "method": "echo.set", "params": { "model": "always" } } },
        { "name": "Echo: never", "action": { "method": "echo.set", "params": { "model": "never" } } },
        { "name": client_log_name, "action": { "method": "logging.set", "params": { "enabled": client_log_enabled } } },
        { "name": server_log_name, "action": { "method": "logging.set", "params": { "scope": "server", "enabled": server_log_enabled } } },
        { "name": "Shell out (server)", "action": { "method": "shell.open" } },
        { "name": "Reset & resync (force redraw)", "action": { "method": "session.resync" } },
        { "name": "Dump wedge forensics", "action": { "method": "session.forensics" } },
        { "name": "Suspend client", "action": { "method": "client.suspend" } },
        { "name": "Quit session", "action": { "method": "app.quit" } },
    ])
}

/// Summon the command palette: spawn the renderer on first use, then show it.
/// Returns whether it opened (false if the renderer can't be spawned, leaving
/// the caller to fall back to the emergency-quit prefix).
fn open_palette(st: &mut ClientState) -> bool {
    if st.palette.is_none() {
        st.palette = Palette::spawn(st.rows, st.cols);
    }
    let commands = palette_commands(st.server_log_on);
    if let Some(p) = st.palette.as_mut() {
        p.open("Commands", commands);
        st.initialized = false; // repaint to show the overlay
        true
    } else {
        false
    }
}

/// Dispatch a palette-selected command action (RFC 0005 §7). Returns whether the
/// client should send to the server promptly (the escape-to-shell flag or quit).
fn dispatch_palette_action(
    st: &mut ClientState,
    raw: &RawMode,
    method: &str,
    params: &Value,
    now: u64,
) -> bool {
    match method {
        "echo.set" => {
            if let Some(model) = params
                .get("model")
                .and_then(Value::as_str)
                .and_then(|m| PredictionModel::parse(Some(m)).ok())
            {
                apply_echo_model(st, model, now);
            }
            false
        }
        "logging.set" => {
            let enabled = params.get("enabled").and_then(Value::as_bool);
            if params.get("scope").and_then(Value::as_str) == Some("server") {
                // Server-side toggle (#3): request the state change over the wire;
                // the server applies it and reports back via FLAG_SERVER_LOG.
                if let Some(en) = enabled {
                    st.flags |= if en {
                        sync::CLIENT_FLAG_LOG_ON
                    } else {
                        sync::CLIENT_FLAG_LOG_OFF
                    };
                    let msg = if en {
                        "server debug logging: on (requested)"
                    } else {
                        "server debug logging: off (requested)"
                    };
                    st.notify.set_message(msg, false, now);
                }
                return true; // send the request flag promptly
            }
            if let Some(en) = enabled {
                set_logging(st, en, now); // client-local (default scope)
            }
            false
        }
        "shell.open" => {
            // FDR 0008 escape-to-shell: a one-shot sticky flag the next message
            // carries; the server spawns the overlay shell in the session cwd.
            st.flags |= sync::CLIENT_FLAG_ESCAPE;
            st.notify.set_message("opening shell\u{2026}", true, now);
            true
        }
        "session.resync" => {
            // Force the server to send a Full keyframe (#wedge): the client is
            // wedged rejecting diffs against a base it isn't at and the automatic
            // stale-ack -> Full recovery didn't fire. One-shot flag like
            // CLIENT_FLAG_ESCAPE; the server drops its acked baseline so the next
            // frame is a Full this client applies unconditionally.
            st.flags |= sync::CLIENT_FLAG_RESYNC;
            st.notify.set_message("resyncing\u{2026}", true, now);
            true
        }
        "session.forensics" => {
            // Write a byte-level apply-stall forensic bundle on demand (#90/#94).
            // Only meaningful while wedged (a reack is pending); otherwise says so.
            let msg = match st.last_reack.as_ref() {
                Some(reack) => {
                    match diag::capture_forensics(st.applied_num, &st.applied_data, reack) {
                        Some(path) => format!("wedge forensics: {}", path.display()),
                        None => "wedge forensics: write failed".to_string(),
                    }
                }
                None => "wedge forensics: nothing pending (not wedged)".to_string(),
            };
            st.notify.set_message(&msg, false, now);
            false
        }
        "client.suspend" => {
            suspend(st, raw);
            false
        }
        "app.quit" => {
            request_shutdown(st);
            true
        }
        _ => false, // unknown method: the renderer already closed; ignore
    }
}

/// $POSH_GRAB_MOUSE: whether to grab the wheel on the outer terminal when the
/// session app has no mouse mode of its own, translating wheel-up/down into
/// arrow keys client-side. Off by default — grabbing costs the outer
/// terminal's native click-to-select. See posh#50/#3/#28; the faithful
/// wheel→scrollback behavior is posh#43.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrabMouse {
    Off,
    On,
}

impl GrabMouse {
    fn parse(value: Option<&str>) -> Result<GrabMouse> {
        match value {
            None | Some("") | Some("off") | Some("never") | Some("0") | Some("false") => {
                Ok(GrabMouse::Off)
            }
            Some("on") | Some("always") | Some("1") | Some("true") => Ok(GrabMouse::On),
            Some(other) => Err(Error(format!("unknown POSH_GRAB_MOUSE setting ({other})"))),
        }
    }
}

pub fn run(host: &str, port: u16, family: Family) -> Result<()> {
    util::check_utf8_locale("posh-client")?;

    // mosh convention: the key travels in the environment, never on argv
    // (argv is world-readable via ps).
    let key_str = std::env::var("POSH_KEY")
        .map_err(|_| Error::from("POSH_KEY environment variable not set"))?;
    std::env::remove_var("POSH_KEY");
    let key = Key::from_base64(key_str.trim())?;

    // Model selection: $POSH_PREDICTION_MODEL, falling back to the deprecated
    // $POSH_PREDICTION alias. Render style: $POSH_PREDICTION_RENDER (default
    // replace).
    let model_env = std::env::var("POSH_PREDICTION_MODEL")
        .ok()
        .or_else(|| std::env::var("POSH_PREDICTION").ok());
    let model = PredictionModel::parse(model_env.as_deref()).map_err(Error)?;
    let render_env = std::env::var("POSH_PREDICTION_RENDER").ok();
    let render = RenderStyle::parse(render_env.as_deref()).map_err(Error)?;
    let predict_overwrite = std::env::var("POSH_PREDICTION_OVERWRITE")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let grab_mouse = GrabMouse::parse(std::env::var("POSH_GRAB_MOUSE").ok().as_deref())?;

    let addr = resolve(host, port, family)?;
    let conn = Connection::client(addr, &key)?;

    // Handlers go in before raw mode and the takeover write: the first
    // byte on the tty is the outside world's readiness signal, and a
    // SIGTERM racing it must find the handler installed, not the default
    // disposition (github #48).
    util::install_client_signal_handlers();
    // SIGUSR2 dumps live transport state on demand (remote::diag); installed
    // here (not in the shared client bundle) so only the roaming client, which
    // consumes the flag in drive_client, arms it.
    util::install_sigusr2_handler();
    let raw = RawMode::enable(STDIN)?;
    // Take over the alternate screen (mosh smcup); close() below restores
    // the user's pre-connect shell screen on the way out.
    let _ = util::write_all_retry(STDOUT, &display::open(), 1000);
    let result = client_loop(conn, model, render, predict_overwrite, grab_mouse, &raw, addr.port());
    let _ = util::write_all_retry(STDOUT, &display::close(), 1000);
    drop(raw);
    eprintln!("\nposh: [client exited]");
    // Carry the remote session's exit status (EXIT_STATUS capability,
    // RFC 0001 §3) into our own, mirroring the local attach path (#18).
    match result {
        Ok(0) => Ok(()),
        Ok(code) => std::process::exit(code),
        Err(e) => Err(e),
    }
}

fn resolve(host: &str, port: u16, family: Family) -> Result<SocketAddr> {
    // System resolver first — this honors Tailscale MagicDNS when tailscaled
    // has wired it into the resolver (the default on most hosts).
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map(Iterator::collect)
        .unwrap_or_default();
    let pick = match family {
        Family::Inet => addrs.iter().find(|a| a.is_ipv4()),
        Family::Inet6 => addrs.iter().find(|a| a.is_ipv6()),
        // Prefer IPv4 (the common path for roaming UDP), fall back to v6.
        Family::Auto => addrs.iter().find(|a| a.is_ipv4()).or_else(|| addrs.first()),
    };
    if let Some(addr) = pick.copied() {
        return Ok(addr);
    }

    // Fallback: a tailnet MagicDNS name the system resolver couldn't reach
    // (MagicDNS off, a container, split-DNS). `tailnet::resolve` shells out to
    // `tailscale status --json` and degrades to None when unavailable.
    if let Some(ip) = crate::tailnet::resolve(host) {
        let family_ok = match family {
            Family::Inet => ip.is_ipv4(),
            Family::Inet6 => ip.is_ipv6(),
            Family::Auto => true,
        };
        if family_ok {
            return Ok(SocketAddr::new(ip, port));
        }
    }

    Err(Error(format!(
        "could not resolve {host} (system resolver and tailnet)"
    )))
}

struct ClientState {
    conn: Connection,
    fragmenter: Fragmenter,
    outbox: InputOutbox,
    rows: u16,
    cols: u16,
    flags: u8,
    last_send: u64,
    // Frame 0 is the implicit empty initial state.
    applied_num: u64,
    applied_data: Vec<u8>,
    /// Server screen state, rebuilt from the latest applied frame.
    server_term: Terminal,
    /// Local, partial, monotonically-growing accumulation of the session's
    /// primary-screen scrollback (RFC 0002 §3). Fed by `BODY_SCROLLBACK`
    /// frames; survives `Full` visible resets; cleared on a width resize.
    /// Rendered by the wheel scroll-view (FDR 0005): `compose_scroll_frame`
    /// reads a window of it when `scroll_offset > 0`.
    scrollback: ScrollbackRing,
    /// Set on resize to drop the `SCROLLBACK` advertisement for exactly the
    /// next outgoing message (RFC 0002 §4: a resize ceases scrollback so the
    /// server restarts appended-row counting afresh at the new width).
    suppress_scrollback_once: bool,
    /// What the physical tty currently shows.
    last_drawn: Snapshot,
    /// False when the outer terminal state is unknown (startup, resize,
    /// Ctrl-L): the next frame repaints from scratch.
    initialized: bool,
    predict: Box<dyn Predictor>,
    renderer: Box<dyn PredictionRenderer>,
    /// Cached prediction config so the model can be rebuilt live (Ctrl-^ e
    /// cycles it). The trait objects above are swapped; these record what to
    /// rebuild from.
    predict_model: PredictionModel,
    predict_render: RenderStyle,
    predict_overwrite: bool,
    notify: NotificationEngine,
    /// $POSH_GRAB_MOUSE policy; on, intercepted wheel events become arrow keys
    /// instead of driving the scrollback scroll-view (the legacy posh#50 grab).
    grab_mouse: GrabMouse,
    /// Byte-fed state machine over the intercepted wheel: reports scroll ticks
    /// (default) or translates to arrows (grab); its persistent state
    /// reassembles sequences split across reads (posh#52).
    mouse_filter: MouseFilter,
    quit_pending: bool,
    shutdown_requested: bool,
    shutdown_requested_at: u64,
    shutdown_seen: bool,
    /// Remote session exit code from the EXIT_STATUS capability on the
    /// shutdown frame; 0 against baseline servers or on user-quit.
    exit_status: i32,
    /// (applied_num, server_term generation) at the last compose, plus
    /// whether any overlay was live then — the idle fast-path key. github #35.
    last_render_state: (u64, u64),
    last_render_overlays: bool,
    /// Scroll-view position (FDR 0005): rows the viewport top sits above the
    /// live bottom. 0 = live view; > 0 freezes the live view and renders a
    /// window of `scrollback` via `compose_scroll_frame`.
    scroll_offset: usize,
    /// Idle fast-path key for the scroll view: (offset, ring len, generation)
    /// at the last scroll compose. None forces the next scroll frame to repaint.
    last_scroll_state: Option<(usize, usize, u64)>,
    /// Latest server-reported remote-PTY ECHO state (FLAG_ECHO). Gates
    /// optimistic local echo (FDR 0006); defaults off until the first frame.
    echo_on: bool,
    /// Latest server-reported debug-logging state (FLAG_SERVER_LOG, #3); drives
    /// the palette's "Server debug logging" command label. Off until reported.
    server_log_on: bool,
    /// Pending keystroke timestamps for the input-latency gauge: (outbox
    /// end-offset after a stdin read, queue time ms). Drained as the server's
    /// `input_ack` covers each offset. Capped so a stalled link can't grow it.
    input_sent: VecDeque<(u64, u64)>,
    /// Frame-sync codec selection (`POSH_FRAMESYNC`, #15). Drives whether we
    /// advertise `CAP_MORPH` and which `applier` we route visible-frame bodies
    /// through. Defaults to DumpDiff (today's behavior) when the env is unset.
    framesync: framesync::FrameSync,
    /// Client-side codec that applies a received visible-frame body to
    /// `server_term`. DumpDiff reparses a fresh model; MorphDelta morphs the
    /// existing one in place (and falls back to a reparse for `Full` keyframes).
    applier: Box<dyn FrameApplier>,
    /// Optional performance instrumentation (POSH_DEBUG_LOG); inert when unset.
    stats: Stats,
    /// The command-palette overlay renderer (Ctrl-^ p), spawned lazily on first
    /// summon and kept resident; `None` until then or if it can't be launched.
    palette: Option<Palette>,
    /// The last visible-frame body the applier rejected via `ReackAndWait`
    /// (#90/#94 forensics): `(rx_num, rx_base, kind, body_bytes)`. Captured to
    /// disk once per wedge episode so the divergent base can be analysed
    /// offline (`diag::capture_forensics`).
    last_reack: Option<(u64, u64, FrameKind, Vec<u8>)>,
    /// One-shot guard so a wedge writes a single forensic bundle, not one per
    /// retransmit (the reack loop fires constantly); reset when apply advances.
    forensic_captured: bool,
}

#[allow(clippy::too_many_arguments)]
fn client_loop(
    conn: Connection,
    model: PredictionModel,
    render: RenderStyle,
    predict_overwrite: bool,
    grab_mouse: GrabMouse,
    raw: &RawMode,
    port: u16,
) -> Result<i32> {
    util::set_nonblocking(STDIN)?;

    let (rows, cols) = pty::term_size(STDOUT);
    let now = now_ms();
    let (predict, renderer) = predict::build(model, render, predict_overwrite);
    // Frame-sync codec (#15): opt into MorphDelta with POSH_FRAMESYNC=morph;
    // unset/empty/other stays on DumpDiff (today's behavior, default-off).
    let framesync = framesync::FrameSync::parse(std::env::var("POSH_FRAMESYNC").ok().as_deref());
    let applier = framesync.applier();
    let mut st = ClientState {
        conn,
        fragmenter: Fragmenter::new(),
        outbox: InputOutbox::new(),
        rows,
        cols,
        flags: 0,
        last_send: 0,
        applied_num: 0,
        applied_data: Vec::new(),
        server_term: Terminal::with_scrollback(rows, cols, 0),
        scrollback: ScrollbackRing::new(SCROLLBACK_RING_DEPTH),
        suppress_scrollback_once: false,
        last_drawn: Snapshot::blank(rows, cols),
        initialized: false,
        predict,
        renderer,
        predict_model: model,
        predict_render: render,
        predict_overwrite,
        notify: NotificationEngine::new(now),
        grab_mouse,
        mouse_filter: MouseFilter::default(),
        quit_pending: false,
        shutdown_requested: false,
        shutdown_requested_at: 0,
        shutdown_seen: false,
        exit_status: 0,
        last_render_state: (u64::MAX, u64::MAX),
        last_render_overlays: false,
        scroll_offset: 0,
        last_scroll_state: None,
        echo_on: false,
        server_log_on: false,
        input_sent: VecDeque::new(),
        framesync,
        applier,
        stats: Stats::new(),
        palette: None,
        last_reack: None,
        forensic_captured: false,
    };
    let result = drive_client(&mut st, raw, port);
    // Tear down the palette renderer (if any) before the final stats flush.
    if let Some(p) = st.palette.take() {
        p.shutdown();
    }
    // One final summary regardless of how the loop exited (graceful, timeout,
    // or error), so the log always ends with the last-observed transport state.
    let now = now_ms();
    let predict_stats = st.predict.stats();
    st.stats.final_client(
        now,
        st.conn.srtt(),
        st.conn.rto(),
        st.conn.send_interval(),
        predict_sample(&predict_stats),
        predict_stats.srtt_trigger,
        st.conn.bytes_rx(),
        st.conn.bytes_tx(),
    );
    result
}

/// Drives the client event loop until detach, shell exit, timeout, or error.
/// Split from `client_loop` so the final stats flush runs on every exit path.
fn drive_client(st: &mut ClientState, raw: &RawMode, port: u16) -> Result<i32> {
    let mut assembly = FragmentAssembly::new();

    // Connect diagnostics (mosh stmclient): before the first authentic
    // frame, hint after 250ms and give up after POSH_CONNECT_TMOUT seconds
    // (default 15, 0 disables) instead of waiting forever on a firewalled
    // port or a server that failed to start.
    let started = now_ms();
    let connect_timeout: u64 = std::env::var("POSH_CONNECT_TMOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|s| s.saturating_mul(1000))
        .unwrap_or(15_000);
    let mut heard = false;

    // Hello: teaches the server our address and terminal size.
    send_message(st);

    let result: Result<i32> = 'client: loop {
        let iter_start = st.stats.enabled().then(Instant::now);
        let now = now_ms();
        let mut deadline = st.last_send + HEARTBEAT_INTERVAL;
        if !st.outbox.is_empty() || st.flags != 0 {
            deadline = deadline.min(st.last_send + st.conn.rto());
        }
        deadline = deadline.min(now + st.notify.wait_time(now));
        if st.predict.needs_timer() {
            // Outstanding predictions need 50ms ticks for glitch detection.
            deadline = deadline.min(now + 50);
        }
        if !heard {
            // Pre-contact: tick for the 250ms hint / connect timeout.
            deadline = deadline.min(now + 250);
        }
        let timeout = deadline.saturating_sub(now).min(1000) as i32;

        let mut fds = vec![
            util::pollfd(STDIN, libc::POLLIN),
            util::pollfd(st.conn.raw_fd(), libc::POLLIN),
        ];
        // Poll the palette renderer's fds (its PTY + control socket) while it is
        // resident, so its output drains and selections are seen promptly.
        let palette_base = st.palette.as_ref().map(|p| {
            let base = fds.len();
            fds.push(util::pollfd(p.master_fd(), libc::POLLIN));
            fds.push(util::pollfd(p.ctrl_fd(), libc::POLLIN));
            base
        });
        let mut send_now = false;
        let poll_start = st.stats.enabled().then(Instant::now);
        match util::poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => break 'client Err(e.into()),
        }
        let idle_us = poll_start.map_or(0, |t| t.elapsed().as_micros() as u64);

        if util::take_flag(&util::SIGWINCH_RECEIVED) {
            let size = pty::term_size(STDOUT);
            st.rows = size.0;
            st.cols = size.1;
            if let Some(p) = st.palette.as_mut() {
                p.resize(st.rows, st.cols);
            }
            st.predict.reset();
            st.initialized = false; // full repaint at the new size
            // RFC 0002 §4: a width change rewraps the server's ring, so
            // absolute row continuity ends. Drop the accumulated ring,
            // discard the (not-yet-built) scroll view by virtue of the
            // repaint, and stop advertising SCROLLBACK for the resize
            // message so the server restarts appended-row counting afresh.
            st.scrollback.clear();
            st.scroll_offset = 0; // FDR 0005: a resize returns to the live view
            st.suppress_scrollback_once = true;
            send_now = true;
        }

        if util::take_flag(&util::SIGTERM_RECEIVED) {
            // SIGTERM/SIGINT/SIGHUP: wind down through the normal shutdown
            // handshake so run() restores the tty and the server hangs up
            // the shell instead of lingering until the network timeout.
            request_shutdown(st);
            send_now = true;
        }

        if util::take_flag(&util::SIGCONT_RECEIVED) {
            // Resumed after SIGSTOP/fg: the outer terminal state is unknown.
            st.initialized = false;
        }

        // SIGUSR2: snapshot live transport state to the diagnostic sink. Goes to
        // a file, never the tty — stdout is the alternate-screen TUI and stderr
        // is the user's outer shell, so writing either would corrupt the display.
        if util::take_flag(&util::SIGUSR2_RECEIVED) {
            let ps = predict_sample(&st.predict.stats());
            diag::ClientState {
                remote: st.conn.remote(),
                last_send_age_ms: (st.last_send != 0).then(|| now.saturating_sub(st.last_send)),
                applied_num: st.applied_num,
                outbox_base: st.outbox.base(),
                outbox_pending: st.outbox.pending().len(),
                scrollback_len: st.scrollback.len(),
                srtt: st.conn.srtt(),
                rto: st.conn.rto(),
                send_interval: st.conn.send_interval(),
                bytes_rx: st.conn.bytes_rx(),
                bytes_tx: st.conn.bytes_tx(),
                predict_active: ps.active,
                predict_shown: ps.shown,
                predict_epoch_lag: ps.epoch_lag,
                term_gen: st.server_term.generation(),
                rows: st.rows,
                cols: st.cols,
                echo_on: st.echo_on,
                codec: st.framesync.label(),
                apply: st.stats.apply_snapshot(),
            }
            .dump();
            // Also drop a byte-level forensic bundle if an apply-stall is
            // pending, so `just debug-posh-dump` captures the divergent base.
            if let Some(reack) = st.last_reack.as_ref() {
                let _ = diag::capture_forensics(st.applied_num, &st.applied_data, reack);
            }
        }

        // Keystrokes -> quit sequence / prediction / reliable input stream.
        if fds[0].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 4096];
            match util::read_fd(STDIN, &mut buf) {
                Ok(0) => {
                    // EOF on the local tty: ask the server to wind down.
                    request_shutdown(st);
                    send_now = true;
                }
                Ok(n) => {
                    if process_user_input(st, &buf[..n]) {
                        send_now = true;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => break 'client Err(e.into()),
            }
        }

        // Server frames.
        if fds[1].revents & libc::POLLIN != 0 {
            loop {
                match st.conn.recv() {
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
                        if !heard {
                            heard = true;
                            if st.notify.message().starts_with("Nothing received") {
                                st.notify.set_message("", false, now_ms());
                            }
                        }
                        if process_frame(st, &frame) {
                            send_now = true; // ack the new state promptly
                        }
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }

        // Palette overlay: drain the renderer's screen so the next compose
        // reflects it, and act on any selection/cancel it reported.
        if let Some(base) = palette_base {
            if fds[base].revents & libc::POLLIN != 0 {
                if let Some(p) = st.palette.as_mut() {
                    p.pump();
                }
            }
            if fds[base + 1].revents & libc::POLLIN != 0 {
                match st.palette.as_mut().map(Palette::poll_events) {
                    Some(PaletteEvent::Action { method, params }) => {
                        if dispatch_palette_action(st, raw, &method, &params, now_ms()) {
                            send_now = true;
                        }
                        st.initialized = false; // palette closed -> repaint session
                    }
                    Some(PaletteEvent::Cancelled) => {
                        st.initialized = false; // palette closed -> repaint session
                    }
                    _ => {}
                }
            }
        }

        let now = now_ms();
        if !heard {
            let waited = now.saturating_sub(started);
            if connect_timeout > 0 && waited >= connect_timeout {
                break 'client Err(Error(format!(
                    "Timed out waiting for server on UDP port {port}."
                )));
            }
            if waited >= 250 && st.notify.message().is_empty() {
                st.notify.set_message(
                    &format!("Nothing received from server on UDP port {port}."),
                    true,
                    now,
                );
            }
        }
        render(st, now);
        // Self-logging apply-stall detector (#wedge): a visible model frozen
        // past the threshold while diff frames keep arriving emits one diagnostic
        // line. Cheap no-op when POSH_DEBUG_LOG is unset.
        st.stats.check_wedge(
            now,
            st.server_term.generation(),
            st.applied_num,
            st.framesync.label(),
        );
        let predict_stats = st.predict.stats();
        st.stats.flush_client(
            now,
            st.conn.srtt(),
            st.conn.rto(),
            st.conn.send_interval(),
            predict_sample(&predict_stats),
            predict_stats.srtt_trigger,
            st.conn.bytes_rx(),
            st.conn.bytes_tx(),
        );

        if send_now
            || ((!st.outbox.is_empty() || st.flags != 0)
                && now.saturating_sub(st.last_send) >= st.conn.rto())
            || now.saturating_sub(st.last_send) >= HEARTBEAT_INTERVAL
        {
            send_message(st);
        }

        if st.shutdown_seen {
            // Shell exited (or our quit was acknowledged); the final-state
            // ack went out just above.
            break 'client Ok(st.exit_status);
        }
        if st.shutdown_requested && now.saturating_sub(st.shutdown_requested_at) >= SHUTDOWN_GRACE {
            break 'client Ok(0); // server unreachable; leave anyway
        }

        // Per-iteration loop timing (perf instrumentation): busy = the whole
        // iteration minus the poll wait.
        if let Some(start) = iter_start {
            let total = start.elapsed().as_micros() as u64;
            st.stats.record_loop_iter(total.saturating_sub(idle_us), idle_us);
        }
    };
    result
}

/// mosh stmclient.cc suspend sequence: restore the outer terminal and the
/// tty driver, stop our process group, and on SIGCONT re-enter raw mode and
/// force a full repaint.
fn suspend(st: &mut ClientState, raw: &RawMode) {
    let _ = util::write_all_retry(STDOUT, &display::close(), 1000);
    raw.restore();
    let _ = util::write_all_retry(STDOUT, b"\r\n\x1b[37;44m[posh is suspended.]\x1b[m\r\n", 1000);
    util::stop_own_pgroup();
    // Execution resumes here after SIGCONT (fg): back onto the alternate
    // screen before the forced repaint below redraws it.
    raw.reapply();
    let _ = util::write_all_retry(STDOUT, &display::open(), 1000);
    st.predict.reset();
    st.initialized = false;
}

fn request_shutdown(st: &mut ClientState) {
    if !st.shutdown_requested {
        st.shutdown_requested = true;
        st.shutdown_requested_at = now_ms();
        st.flags |= sync::CLIENT_FLAG_SHUTDOWN;
        st.notify
            .set_message("Exiting on user request...", true, now_ms());
    }
}

/// Whether the client intercepts the outer terminal's wheel right now: the
/// inner app has set no mouse mode of its own AND it is on the primary screen
/// (the only screen with scrollback). True at a bare prompt — where the wheel
/// drives the scrollback scroll-view (FDR 0005) by default, or the legacy
/// wheel→arrow grab transform when `POSH_GRAB_MOUSE=on` (posh#50). This is the
/// "enable wheel reporting" predicate (render side); the input side then picks
/// arrows-vs-scroll handling via `grab_mouse` (`POSH_GRAB_MOUSE`).
fn wheel_active(st: &ClientState) -> bool {
    st.server_term.mouse_mode() == posh_term::MouseMode::None && !st.server_term.is_alt_screen()
}

/// Lines moved per wheel tick (matches a typical terminal's wheel step).
const WHEEL_STEP: usize = 3;

/// Sets the scroll-view offset, clamped to the available history (the ring
/// depth). On a real change it invalidates both render memos so the next render
/// repaints the appropriate view (scroll or, at offset 0, live).
fn set_scroll(st: &mut ClientState, offset: usize) {
    let offset = offset.min(st.scrollback.len());
    if offset != st.scroll_offset {
        st.scroll_offset = offset;
        st.last_render_state = (u64::MAX, u64::MAX);
        st.last_scroll_state = None;
    }
}

/// Applies wheel ticks to the scroll offset: + = up (scroll back into history),
/// - = down (toward live). Reaching 0 returns to the live view (FDR 0005).
fn scroll_by(st: &mut ClientState, ticks: i32) {
    let delta = ticks * WHEEL_STEP as i32;
    let new = (st.scroll_offset as i64 + i64::from(delta)).max(0) as usize;
    set_scroll(st, new);
}

/// Whether local echo is safe to show right now: the remote PTY is echoing
/// (server-reported FLAG_ECHO) and the primary screen is active (not a
/// full-screen app). The optimistic model uses this (via `set_echo_safe`) to
/// suppress echo for passwords and TUIs; other models ignore it.
fn optimistic_echo_on(st: &ClientState) -> bool {
    st.echo_on && !st.server_term.is_alt_screen()
}

/// Cap on a buffered candidate SGR mouse sequence. A real one is at most
/// `ESC [ < 223 ; 65535 ; 65535 M` (22 bytes); a longer run with no
/// terminator is not a mouse sequence, so the filter gives up and flushes it
/// raw — bounding the buffer and never swallowing real input forever. posh#52.
const MAX_MOUSE_SEQ: usize = 32;

/// A byte-fed state machine that intercepts SGR mouse sequences
/// (`ESC [ < Cb ; Cx ; Cy (M|m)`) in the input stream and translates the
/// wheel ones to arrow keys, dropping the rest — the wheel-grab transform
/// (posh#50). Modeled on mosh's `UserInput` (and posh-term's own parser): the
/// state persists across calls, so a sequence split across `read()`s
/// reassembles at *any* byte boundary with no held-buffer special-casing
/// (posh#52). Only bytes that are part of a live `ESC[<…` match are withheld;
/// the instant a match fails (or overflows `MAX_MOUSE_SEQ`), every buffered
/// byte is flushed verbatim — so all non-mouse input (Esc, arrows, ctrl-keys,
/// UTF-8) round-trips losslessly.
///
/// Accepted tradeoff: a lone trailing `ESC` (and a partial `ESC[`) is held
/// until the next byte resolves whether it begins a mouse sequence — the
/// classic Esc-vs-escape-sequence ambiguity every VT input layer faces (cf.
/// vim `ttimeoutlen`, readline `keyseq-timeout`). So a *solo* Esc keypress is
/// withheld until the next key. This only bites under `POSH_GRAB_MOUSE=on`
/// AND when the inner app has set no mouse mode (a bare prompt, where a lone
/// Esc rarely matters); mosh's `UserInput` holds ESC the same way. A
/// millisecond timeout flush (the other standard resolution) is deliberately
/// not added — it would put a deadline in the poll loop for a default-off
/// feature's edge. Rationale recorded in docs/decisions/0002.
#[derive(Default)]
struct MouseFilter {
    state: MouseState,
    /// Bytes consumed for the in-progress candidate, replayed verbatim if the
    /// candidate turns out not to be a (complete) mouse sequence.
    pending: Vec<u8>,
}

#[derive(Default, PartialEq)]
enum MouseState {
    #[default]
    Ground,
    Esc,        // saw ESC
    Bracket,    // saw ESC [
    Body,       // saw ESC [ < ; collecting Cb;Cx;Cy until M/m
}

/// What a `MouseFilter::feed` batch yields: the non-mouse bytes to forward, and
/// the net wheel ticks recognized (+ = up/scroll-back, - = down). In scroll
/// mode wheel events populate `wheel` and produce no bytes; in arrows mode
/// (legacy `POSH_GRAB_MOUSE`) they are translated into arrow keys in `bytes`.
#[derive(Default)]
struct FilterOut {
    bytes: Vec<u8>,
    wheel: i32,
}

impl MouseFilter {
    /// Feed one input batch; returns the bytes to forward plus any net wheel
    /// ticks. `scroll` selects the wheel handling: true → report ticks for the
    /// scrollback view; false → translate to arrow keys (legacy grab). Any
    /// incomplete trailing sequence stays in `self` for the next call.
    fn feed(&mut self, buf: &[u8], app_cursor_keys: bool, scroll: bool) -> FilterOut {
        let mut out = FilterOut {
            bytes: Vec::with_capacity(buf.len() + self.pending.len()),
            wheel: 0,
        };
        for &b in buf {
            self.step(b, app_cursor_keys, scroll, &mut out);
        }
        out
    }

    fn step(&mut self, b: u8, app_cursor_keys: bool, scroll: bool, out: &mut FilterOut) {
        match self.state {
            MouseState::Ground => {
                if b == 0x1b {
                    self.pending.push(b);
                    self.state = MouseState::Esc;
                } else {
                    out.bytes.push(b);
                }
            }
            MouseState::Esc => {
                if b == b'[' {
                    self.pending.push(b);
                    self.state = MouseState::Bracket;
                } else {
                    // Not ESC [ — a real Esc or some other ESC sequence.
                    // Flush ESC and reprocess this byte from Ground.
                    self.flush(out);
                    self.step(b, app_cursor_keys, scroll, out);
                }
            }
            MouseState::Bracket => {
                if b == b'<' {
                    self.pending.push(b);
                    self.state = MouseState::Body;
                } else {
                    // ESC [ <other> — a real CSI (arrow, etc.), not mouse.
                    self.flush(out);
                    self.step(b, app_cursor_keys, scroll, out);
                }
            }
            MouseState::Body => {
                if b == b'M' || b == b'm' {
                    // Complete: translate the button code, drop non-wheel.
                    let body = &self.pending[3..]; // after ESC [ <
                    let cb = body.split(|&c| c == b';').next().and_then(|s| {
                        std::str::from_utf8(s).ok().and_then(|s| s.parse::<u32>().ok())
                    });
                    match cb {
                        // Wheel up/down: report a scroll tick (scroll mode) or
                        // translate to an arrow key (legacy grab mode).
                        Some(64) if scroll => out.wheel += 1,
                        Some(65) if scroll => out.wheel -= 1,
                        Some(64) => out.bytes.extend_from_slice(arrow_up(app_cursor_keys)),
                        Some(65) => out.bytes.extend_from_slice(arrow_down(app_cursor_keys)),
                        // click / motion / other button → dropped; a malformed
                        // ESC[<M with no button code (cb == None) drops too,
                        // which is correct: the grabbed app requested no mouse
                        // reporting, so no mouse event should reach it.
                        _ => {}
                    }
                    self.pending.clear();
                    self.state = MouseState::Ground;
                } else if b.is_ascii_digit() || b == b';' {
                    self.pending.push(b);
                    if self.pending.len() > MAX_MOUSE_SEQ {
                        // Not a real mouse sequence; give up and flush raw.
                        self.flush(out);
                    }
                } else {
                    // Unexpected byte in the body: not a valid mouse sequence.
                    self.flush(out);
                    self.step(b, app_cursor_keys, scroll, out);
                }
            }
        }
    }

    /// Emit the buffered candidate verbatim and reset to Ground (the bytes
    /// weren't a mouse sequence after all).
    fn flush(&mut self, out: &mut FilterOut) {
        out.bytes.extend_from_slice(&self.pending);
        self.pending.clear();
        self.state = MouseState::Ground;
    }

    /// Reset to Ground and return any held partial verbatim. Called when the
    /// grab disengages mid-sequence (the app took over the mouse): the held
    /// bytes are real user input and must not be dropped — handing them back
    /// lets the caller forward the now-complete sequence to the app that just
    /// asked for mouse reporting, rather than losing the prefix and leaking a
    /// corrupt tail. posh#52.
    fn take_pending(&mut self) -> Vec<u8> {
        self.state = MouseState::Ground;
        std::mem::take(&mut self.pending)
    }
}

fn arrow_up(app_cursor_keys: bool) -> &'static [u8] {
    if app_cursor_keys {
        b"\x1bOA"
    } else {
        b"\x1b[A"
    }
}

fn arrow_down(app_cursor_keys: bool) -> &'static [u8] {
    if app_cursor_keys {
        b"\x1bOB"
    } else {
        b"\x1b[B"
    }
}

/// Feeds user bytes through the Ctrl-^ quit-sequence state machine, the
/// prediction engine, and into the reliable input stream. Returns true when
/// anything needs sending.
fn process_user_input(st: &mut ClientState, buf: &[u8]) -> bool {
    let now = now_ms();
    let mut dirty = false;

    // While the palette overlay is up, the renderer owns the keyboard: forward
    // raw keystrokes to it and send nothing to the session. It reports the
    // selection/cancel over the control channel (handled in the poll loop).
    if let Some(p) = st.palette.as_ref().filter(|p| p.is_open()) {
        p.forward_input(buf);
        return false;
    }

    // Input-latency baseline (perf): the outbox offset before this read's
    // keystrokes, so the bytes queued below can be timestamped and their
    // keystroke→consumed round-trip measured when the server acks them.
    let outbox_start = st.outbox.end_offset();

    // When we are intercepting the wheel (bare prompt, primary screen), run
    // input through the mouse filter before the byte loop. The wheel drives the
    // scrollback scroll-view by default, or the legacy wheel→arrow grab when
    // POSH_GRAB_MOUSE=on; other mouse events are dropped. The filter's
    // persistent state reassembles sequences split across reads (posh#52).
    let grabbed;
    let buf: &[u8] = if wheel_active(st) {
        let app_cursor_keys = st.server_term.app_cursor_keys();
        let scroll_mode = st.grab_mouse != GrabMouse::On;
        let out = st.mouse_filter.feed(buf, app_cursor_keys, scroll_mode);
        if out.wheel != 0 {
            scroll_by(st, out.wheel); // local view change; no network send
        }
        grabbed = out.bytes;
        &grabbed
    } else {
        // Not intercepting. If the filter holds a partial from when interception
        // was last active (the app enabled its own mouse mode mid-sequence,
        // flipping it off between reads), hand those bytes back and prepend them
        // so the app — which now wants mouse events — receives the complete
        // sequence, rather than us dropping the prefix and leaking the tail.
        let pending = st.mouse_filter.take_pending();
        if pending.is_empty() {
            buf
        } else {
            let mut joined = pending;
            joined.extend_from_slice(buf);
            grabbed = joined;
            &grabbed
        }
    };

    // Any keystroke while scrolled returns to the live view (FDR 0005), then is
    // forwarded normally below — you are about to type at the prompt.
    if !buf.is_empty() && st.scroll_offset > 0 {
        set_scroll(st, 0);
    }

    // Don't predict for bulk pastes.
    let paste = buf.len() > 100;
    if paste {
        st.predict.reset();
    }

    // Optimistic echo is gated on the remote PTY echoing and the primary screen
    // being active (FDR 0006): tell the predictor whether echo is safe so the
    // optimistic model suppresses (passwords / full-screen apps). Other models
    // ignore this. The compose path re-asserts the same gate before rendering.
    st.predict.set_echo_safe(optimistic_echo_on(st));

    let push = |st: &mut ClientState, byte: u8| {
        if !paste {
            st.predict.set_frame_sent(st.outbox.end_offset());
            st.predict.on_user_byte(byte, &st.last_drawn, now);
        }
        st.outbox.push(&[byte]);
    };

    for &byte in buf {
        if st.quit_pending {
            // Degraded escape prefix: only entered when the palette could not
            // open, so Ctrl-^ . still quits (the rest pass through literally).
            st.quit_pending = false;
            match byte {
                b'.' => {
                    request_shutdown(st);
                    dirty = true;
                    continue;
                }
                ESCAPE_KEY | ESCAPE_PASS_KEY => {
                    // Ctrl-^ twice (or Ctrl-^ ^) sends a literal Ctrl-^.
                    push(st, ESCAPE_KEY);
                }
                other => {
                    // Anything else is sent literally, escape key included.
                    push(st, ESCAPE_KEY);
                    push(st, other);
                }
            }
            if st.notify.message() == PALETTE_FALLBACK_HELP {
                st.notify.set_message("", false, now);
            }
            dirty = true;
            continue;
        }

        if byte == ESCAPE_KEY {
            // Ctrl-^ opens the command palette directly (it is the escape menu:
            // echo, logging, shell-out, suspend, quit all live there). If the
            // renderer can't be spawned, fall back to the emergency-quit prefix.
            if open_palette(st) {
                dirty = true;
            } else {
                st.quit_pending = true;
                st.notify.set_message(PALETTE_FALLBACK_HELP, true, now);
            }
            continue;
        }

        if byte == 0x0c {
            // Ctrl-L: ask for a full repaint of the outer terminal.
            st.initialized = false;
        }

        push(st, byte);
        dirty = true;
    }
    // If this read queued input, timestamp the resulting outbox offset for the
    // input-latency gauge (drained in process_frame when input_ack covers it).
    if st.stats.enabled() {
        let end = st.outbox.end_offset();
        if end > outbox_start {
            st.input_sent.push_back((end, now));
            if st.input_sent.len() > 256 {
                st.input_sent.pop_front(); // bound a stalled link's backlog
            }
        }
    }
    dirty
}

/// Handles one decoded server frame: acks, prediction bookkeeping, and
/// state application. Returns true when an ack should go out.
fn process_frame(st: &mut ClientState, frame: &ServerFrame) -> bool {
    let now = now_ms();
    // Classify every received frame by wire body (includes retransmissions and
    // duplicates — that is what arrived on the link).
    match &frame.body {
        FrameBody::Full(_) => st.stats.record_frame_full(),
        // A Morph body is the incremental analog of a Diff (a delta against the
        // acked base), so it shares the Diff economics counter (#15).
        FrameBody::Diff { .. } | FrameBody::Morph { .. } => st.stats.record_frame_diff(),
        FrameBody::Empty => st.stats.record_frame_empty(),
        // Scrollback bodies carry no visible-screen change, so they stay out of
        // the Full/Diff/Empty economics — but are counted separately (#wedge) so
        // a scrollback storm/reset is not invisible.
        FrameBody::Scrollback { .. } => st.stats.record_frame_scrollback(),
    }
    st.notify.server_heard(now);
    st.outbox.ack(frame.input_ack);
    // Input latency (perf): the server has now consumed input up to input_ack,
    // so record the keystroke→consumed round-trip for any pending entries it
    // covers (the deque is offset-ordered, so drain from the front).
    while let Some(&(offset, queued)) = st.input_sent.front() {
        if offset > frame.input_ack {
            break;
        }
        st.input_sent.pop_front();
        st.stats.record_input_ms(now.saturating_sub(queued));
    }
    st.predict
        .on_server_frame(frame.input_ack, frame.echo_ack, st.conn.send_interval());
    // Remote PTY echo state for the optimistic-echo gate (FDR 0006).
    st.echo_on = frame.flags & sync::FLAG_ECHO != 0;
    // Server debug-logging state for the palette's "Server debug logging" label (#3).
    st.server_log_on = frame.flags & sync::FLAG_SERVER_LOG != 0;
    // The escape-to-shell overlay is up (FDR 0008): the request was honored, so
    // drop the "opening shell…" notice (the request flag is already one-shot).
    if frame.flags & sync::FLAG_OVERLAY != 0 && st.notify.message() == "opening shell\u{2026}" {
        st.notify.set_message("", false, now);
    }
    if frame.flags & sync::FLAG_SHUTDOWN != 0 {
        st.shutdown_seen = true;
        // EXIT_STATUS rides the shutdown frame's capability table; the
        // server only sends it because we advertised it (RFC 0001 §3).
        if let Some(cap) = caps::find(&frame.caps, caps::CAP_EXIT_STATUS) {
            if let Some(&code) = cap.payload.first() {
                st.exit_status = code as i32;
            }
        }
    }
    apply_frame(st, frame)
}

/// Applies a frame to the local terminal model. Frames reconstruct complete
/// screen state, so application is: fresh Terminal, then feed the dump_vt
/// stream. Returns true when the frame advanced (or repeated) server state
/// and an ack should go out.
/// Clear the apply-stall forensic latch once the client makes forward progress,
/// so a later wedge episode captures a fresh bundle and a manual/SIGUSR2 capture
/// after recovery does not write a stale one.
fn clear_reack(st: &mut ClientState) {
    st.last_reack = None;
    st.forensic_captured = false;
}

fn apply_frame(st: &mut ClientState, frame: &ServerFrame) -> bool {
    // Apply-path instrumentation (#wedge): record the visible/scrollback frame
    // at the gate so a frozen client self-reports which frame it is rejecting.
    // Empty heartbeats carry no visible body and are skipped to keep the signal
    // clean (and to keep `last_rx` pointing at the last real frame).
    match &frame.body {
        FrameBody::Empty => {}
        FrameBody::Full(_) => {
            st.stats
                .record_apply_rx(frame.frame_num, frame.frame_num, FrameKind::Full)
        }
        FrameBody::Diff { base, .. } => {
            st.stats.record_apply_rx(frame.frame_num, *base, FrameKind::Diff)
        }
        FrameBody::Morph { base, .. } => {
            st.stats.record_apply_rx(frame.frame_num, *base, FrameKind::Morph)
        }
        FrameBody::Scrollback { base, .. } => {
            st.stats
                .record_apply_rx(frame.frame_num, *base, FrameKind::Scrollback)
        }
    }
    if frame.frame_num < st.applied_num {
        st.stats.record_apply_stale();
        return true; // stale retransmission: re-ack our newer state
    }
    // Scrollback growth (RFC 0002 §3): append rows to the local ring without
    // disturbing the visible model. `base` is the frame the growth was
    // measured from; we apply only when we are exactly at it, so a
    // retransmitted or superseding body never double-appends. The visible
    // `applied_data` is unchanged by a scrollback frame and stays valid as
    // the base for a later `Diff` that builds on this frame number.
    if let FrameBody::Scrollback { base, rows } = &frame.body {
        if frame.frame_num == st.applied_num {
            st.stats.record_apply_dup();
            return true; // duplicate retransmission: re-ack, don't reapply
        }
        if *base != st.applied_num {
            st.stats.record_apply_basemis();
            return true; // growth against a state we are not at; re-ack
        }
        let grew = rows.len();
        st.scrollback.append(rows);
        if st.scroll_offset > 0 {
            // Keep the frozen viewport anchored on the same content as new rows
            // arrive (FDR 0005: output accumulates but does not yank to bottom).
            set_scroll(st, st.scroll_offset + grew);
        }
        st.applied_num = frame.frame_num;
        st.stats.record_apply_advanced();
        clear_reack(st);
        return true;
    }
    // Base-anchored bodies (Diff, Morph) apply only when we are exactly at
    // their base; otherwise re-ack our (stale) state and let the server fall
    // back to a Full keyframe once it sees the ack. Same base-mismatch rule
    // for both, so a lost frame is handled identically whichever codec is in
    // use (#15).
    match &frame.body {
        FrameBody::Empty => return false,
        FrameBody::Diff { base, .. } | FrameBody::Morph { base, .. } => {
            if *base != st.applied_num {
                st.stats.record_apply_basemis();
                return true;
            }
        }
        FrameBody::Full(_) => {}
        // Handled above (returns early); listed so the match stays total.
        FrameBody::Scrollback { .. } => unreachable!("scrollback handled above"),
    }
    if frame.frame_num == st.applied_num {
        st.stats.record_apply_dup();
        return true; // duplicate retransmission: re-ack, don't reapply
    }
    // Route the body through the selected codec's applier. Time the apply —
    // the client-side mirror of the server's dump_vt_us. For DumpDiff this is
    // the full-dump reparse (the suspected hot spot); for MorphDelta it is the
    // forward `process(escapes)` on the existing model (the optimization).
    let apply_timer = st.stats.enabled().then(Instant::now);
    let outcome = st.applier.apply(
        st.rows,
        st.cols,
        &st.applied_data,
        &mut st.server_term,
        &frame.body,
    );
    if let Some(t) = apply_timer {
        st.stats.record_apply_us(t.elapsed().as_micros() as u64);
    }
    match outcome {
        ApplyOutcome::Advanced { dump } => {
            st.applied_num = frame.frame_num;
            st.applied_data = dump;
            st.stats.record_apply_advanced();
            // The manual "Reset & resync" (CLIENT_FLAG_RESYNC) forces a Full
            // keyframe; once that Full applies and advances us, the recovery is
            // complete, so drop the sticky "resyncing…" notice (#93). Mirrors
            // the one-shot "opening shell…" drop above. Only a Full resolves a
            // resync — a Diff/Morph advancing does not.
            if matches!(frame.body, FrameBody::Full(_))
                && st.notify.message() == "resyncing\u{2026}"
            {
                st.notify.set_message("", false, now_ms());
            }
            clear_reack(st);
            true
        }
        // MorphDelta advanced the model in place without re-dumping it (#15).
        // applied_data stays at the last Full keyframe's dump; a Morph session
        // never sends a Diff body that would read it.
        ApplyOutcome::AdvancedNoDump => {
            st.applied_num = frame.frame_num;
            st.stats.record_apply_advanced();
            clear_reack(st);
            true
        }
        // Undecodable diff against a matching-base frame: re-ack and wait for a
        // Full keyframe, model untouched. This is the #90 apply-stall; stash the
        // rejected body and capture a forensic bundle once per episode so the
        // divergent base can be analysed offline (the divergence origin is not
        // unit-reproducible -- it needs the live bytes).
        ApplyOutcome::ReackAndWait => {
            st.stats.record_apply_reack();
            let captured = match &frame.body {
                FrameBody::Diff { base, diff } => Some((*base, FrameKind::Diff, diff.clone())),
                FrameBody::Morph { base, escapes } => {
                    Some((*base, FrameKind::Morph, escapes.clone()))
                }
                _ => None,
            };
            if let Some((base, kind, bytes)) = captured {
                st.last_reack = Some((frame.frame_num, base, kind, bytes));
                if !st.forensic_captured {
                    st.forensic_captured = true;
                    if let Some(reack) = st.last_reack.as_ref() {
                        let _ = diag::capture_forensics(st.applied_num, &st.applied_data, reack);
                    }
                }
            }
            // Auto-escalate (#90): an undecodable diff against a matching-base
            // frame never self-heals -- the server keeps rebuilding the same
            // diff against a base the client cannot reconstruct. Actively
            // request a Full keyframe instead of passively waiting for one that
            // never comes. One-shot flag (cleared on send); it coalesces across
            // the retransmit storm and rides the next ack, so the manual "Reset
            // & resync" command is no longer the only way out of an apply-stall.
            st.flags |= sync::CLIENT_FLAG_RESYNC;
            true
        }
        // Empty is returned early above; an applier should not produce this
        // for a visible body, but if it does, treat it as no advance.
        ApplyOutcome::NoChange => {
            st.stats.record_apply_nochange();
            false
        }
    }
}

/// mosh's output_new_frame: server state + prediction overlay + status
/// banner, diffed against what the tty currently shows.
fn render(st: &mut ClientState, now: u64) {
    let bytes = if st.scroll_offset > 0 {
        compose_scroll_frame(st)
    } else {
        compose_frame(st, now)
    };
    if bytes.is_empty() {
        st.stats.record_render_skip();
    } else {
        st.stats.record_render(bytes.len());
        let _ = util::write_all_retry(STDOUT, &bytes, 1000);
    }
}

/// Builds this tick's escape stream (empty when the screen already
/// matches). Idle ticks skip the full-grid snapshot: with the model
/// unadvanced, the screen initialized, and no overlay live now or at the
/// previous compose, the diff is provably empty. Overlays are
/// time-driven, so "live" includes the lateness banner being DUE
/// (server_late), not just shown — predictions only change while active,
/// and a just-cleared overlay still gets one closing compose via
/// last_render_overlays. github #35.
fn compose_frame(st: &mut ClientState, now: u64) -> Vec<u8> {
    let model_state = (st.applied_num, st.server_term.generation());
    let palette_open = st.palette.as_ref().is_some_and(Palette::is_open);
    let overlays_live = st.predict.active()
        || !st.notify.message().is_empty()
        || st.notify.server_late(now)
        || palette_open;
    if st.initialized
        && model_state == st.last_render_state
        && !overlays_live
        && !st.last_render_overlays
    {
        return Vec::new();
    }
    st.last_render_state = model_state;
    st.last_render_overlays = overlays_live;

    // Time the actual render compute (snapshot + prediction/banner overlay +
    // diff), excluding the idle fast-path above so the average reflects real
    // work. enabled() is read and dropped before the borrows below.
    let compose_timer = st.stats.enabled().then(Instant::now);
    // Optimistic echo gate (FDR 0006): when echo is unsafe (password prompt /
    // full-screen app) the optimistic model drops its pending overlay so the
    // authoritative paint stands; other models ignore this.
    st.predict.set_echo_safe(optimistic_echo_on(st));
    let base = Snapshot::from_term(&st.server_term);
    st.predict.cull(&base, now);
    let mut next = base;
    st.predict.render(&mut next, &*st.renderer);
    // The palette overlay sits above the session (greyed) but below the banner.
    if palette_open {
        if let Some(rterm) = st.palette.as_ref().and_then(Palette::screen) {
            composite_palette(&mut next, rterm, st.rows, st.cols);
        }
    }
    st.notify.adjust(now);
    st.notify.apply(&mut next, now);

    let wheel = wheel_active(st);
    let bytes = display::new_frame(st.initialized, &st.last_drawn, &next, wheel);
    st.initialized = true;
    st.last_drawn = next;
    if let Some(t) = compose_timer {
        st.stats.record_compose_us(t.elapsed().as_micros() as u64);
    }
    bytes
}

/// Composite the open palette over the session snapshot: grey the session
/// behind it, then paint the renderer's non-blank bounding box anchored a third
/// of the way down, centered, and map the renderer's cursor into that box.
fn composite_palette(next: &mut Snapshot, rterm: &Terminal, rows: u16, cols: u16) {
    // Grey the session background: keep the glyphs, flatten the style.
    let dim = Style {
        fg: Color::Rgb(0x70, 0x70, 0x70),
        dim: true,
        ..Style::default()
    };
    for row in next.cells.iter_mut() {
        for cell in row.iter_mut() {
            cell.style = dim;
        }
    }

    let screen = rterm.screen();
    let Some((r0, c0, r1, c1)) = bbox(screen) else {
        next.cursor_visible = false;
        return;
    };
    let bh = r1 - r0 + 1;
    let bw = c1 - c0 + 1;
    // Anchor a third of the way down, but shift up so a panel taller than the
    // remaining space doesn't clip off the bottom (#3: the longer command list
    // pushed "Server debug logging" and below past the screen edge).
    let dr = (rows / 3).min(rows.saturating_sub(bh));
    let dc = cols.saturating_sub(bw) / 2;
    for r in 0..bh {
        for c in 0..bw {
            if let (Some(src), Some(dst)) =
                (screen.cell(r0 + r, c0 + c), next.cell_mut(dr + r, dc + c))
            {
                *dst = src.clone();
            }
        }
    }

    // Put the real cursor in the palette's input field (mapped from the
    // renderer's cursor); hide it when that cursor falls outside the box.
    let cur = rterm.cursor();
    if cur.visible && cur.row >= r0 && cur.row <= r1 && cur.col >= c0 && cur.col <= c1 {
        next.cursor_row = dr + (cur.row - r0);
        next.cursor_col = dc + (cur.col - c0);
        next.cursor_visible = true;
    } else {
        next.cursor_visible = false;
    }
}

/// Non-blank bounding box of a screen: (top, left, bottom, right), or None.
fn bbox(scr: &Screen) -> Option<(u16, u16, u16, u16)> {
    let mut found: Option<(u16, u16, u16, u16)> = None;
    for r in 0..scr.rows() {
        for c in 0..scr.cols() {
            if scr.cell(r, c).is_some_and(|cell| !cell.is_blank()) {
                found = Some(match found {
                    None => (r, c, r, c),
                    Some((r0, c0, r1, c1)) => (r0.min(r), c0.min(c), r1.max(r), c1.max(c)),
                });
            }
        }
    }
    found
}

/// Builds the scroll-view escape stream (FDR 0005): a window of the accumulated
/// scrollback ring plus the current visible grid, rendered frozen at
/// `scroll_offset`, with a top status-bar indicator. Returns empty when the
/// view already matches (the offset, ring length, and server generation are
/// unchanged). The live view is bypassed while scrolled; a window resize or a
/// keystroke returns `scroll_offset` to 0 and resumes the live path.
fn compose_scroll_frame(st: &mut ClientState) -> Vec<u8> {
    let memo = (st.scroll_offset, st.scrollback.len(), st.server_term.generation());
    if st.initialized && st.last_scroll_state == Some(memo) {
        return Vec::new();
    }
    st.last_scroll_state = Some(memo);

    let rows = st.rows as usize;
    let sb_len = st.scrollback.len();
    // Visible grid rows serialized in the same per-row byte format as the ring,
    // so the whole logical history is one uniform sequence (FDR 0005).
    let visible = st.server_term.dump_visible_rows();
    let total = sb_len + visible.len();
    let offset = st.scroll_offset.min(sb_len);
    // Viewport: the `rows` logical rows ending `offset` above the live bottom.
    let top = total.saturating_sub(rows).saturating_sub(offset);
    let end = (top + rows).min(total);

    // Replay the window through a scratch terminal (posh_term regenerates the
    // wrap seams by autowrapping), then diff it like any other frame.
    let mut term = Terminal::with_scrollback(st.rows, st.cols, 0);
    let count = end - top;
    for (j, i) in (top..end).enumerate() {
        let row: &[u8] = if i < sb_len {
            st.scrollback.row(i).unwrap_or(&[])
        } else {
            &visible[i - sb_len]
        };
        // The final row drops its trailing CRLF so it doesn't scroll the grid.
        if j + 1 == count {
            term.process(row.strip_suffix(b"\r\n").unwrap_or(row));
        } else {
            term.process(row);
        }
    }

    let mut snap = Snapshot::from_term(&term);
    snap.cursor_visible = false; // no live cursor in history
    display::apply_scroll_indicator(&mut snap, offset);
    let bytes = display::new_frame(st.initialized, &st.last_drawn, &snap, wheel_active(st));
    st.initialized = true;
    st.last_drawn = snap;
    bytes
}

/// Snapshots the prediction engine's display gauges for the stats log.
fn predict_sample(stats: &predict::PredictorStats) -> PredictSample {
    let (correct, nocredit, incorrect) = stats.outcomes;
    let (nocredit_unknown, nocredit_blank, nocredit_matched) = stats.nocredit_reasons;
    PredictSample {
        active: stats.active,
        shown: stats.shown_cells,
        epoch_lag: stats.epoch_lag,
        resets: stats.mispredict_resets,
        correct,
        nocredit,
        incorrect,
        nocredit_unknown,
        nocredit_blank,
        nocredit_matched,
    }
}

/// The capability table this client advertises in every message (the
/// protocol is connectionless): protocol version, "I understand exit-status
/// frames", and — unless this is the post-resize message that must cease
/// scrollback (RFC 0002 §4) — "I keep a scrollback ring and understand
/// BODY_SCROLLBACK" with payload 0 requesting the server's default ring
/// depth (RFC 0002 §1). Consumes the one-shot resize suppression.
fn outgoing_caps(st: &mut ClientState) -> Vec<caps::Cap> {
    let mut extra = vec![caps::Cap {
        id: caps::CAP_EXIT_STATUS,
        payload: vec![],
    }];
    if st.suppress_scrollback_once {
        st.suppress_scrollback_once = false;
    } else {
        extra.push(caps::Cap {
            id: caps::CAP_SCROLLBACK,
            payload: vec![0],
        });
    }
    // Incremental frame sync (#15): advertise CAP_MORPH only behind the
    // POSH_FRAMESYNC=morph opt-in. A default session never sends it, so the
    // server selects DumpDiff and the byte stream is unchanged.
    if st.framesync.advertises_morph() {
        extra.push(caps::Cap {
            id: caps::CAP_MORPH,
            payload: vec![],
        });
    }
    caps::own_table(&extra)
}
fn send_message(st: &mut ClientState) {
    let msg = ClientMessage {
        flags: st.flags,
        caps: outgoing_caps(st),
        acked_frame: st.applied_num,
        rows: st.rows,
        cols: st.cols,
        input_base: st.outbox.base(),
        input: st.outbox.pending().to_vec(),
    };
    // CLIENT_FLAG_ESCAPE, the server-logging toggles, and the resync request are
    // one-shot: they rode this message, so clear them now (the server acts
    // idempotently on repeats, and the user can retry if this datagram is lost).
    // SHUTDOWN stays sticky.
    st.flags &= !(sync::CLIENT_FLAG_ESCAPE
        | sync::CLIENT_FLAG_LOG_ON
        | sync::CLIENT_FLAG_LOG_OFF
        | sync::CLIENT_FLAG_RESYNC);
    for frag in st
        .fragmenter
        .make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX)
    {
        let _ = st.conn.send(&frag.to_bytes());
    }
    st.last_send = now_ms();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_families_on_loopback() {
        // Numeric literals resolve to themselves; family filters apply.
        let v4 = resolve("127.0.0.1", 1234, Family::Auto).unwrap();
        assert!(v4.is_ipv4());
        let v4 = resolve("127.0.0.1", 1234, Family::Inet).unwrap();
        assert!(v4.is_ipv4());
        let v6 = resolve("::1", 1234, Family::Inet6).unwrap();
        assert!(v6.is_ipv6());
        assert_eq!(v6.port(), 1234);
        // Family mismatch is an error rather than a silent fallback.
        assert!(resolve("127.0.0.1", 1234, Family::Inet6).is_err());
        assert!(resolve("::1", 1234, Family::Inet).is_err());
    }

    #[test]
    fn grab_mouse_parse() {
        use GrabMouse::*;
        assert_eq!(GrabMouse::parse(None).unwrap(), Off);
        assert_eq!(GrabMouse::parse(Some("")).unwrap(), Off);
        assert_eq!(GrabMouse::parse(Some("off")).unwrap(), Off);
        assert_eq!(GrabMouse::parse(Some("never")).unwrap(), Off);
        assert_eq!(GrabMouse::parse(Some("on")).unwrap(), On);
        assert_eq!(GrabMouse::parse(Some("always")).unwrap(), On);
        assert_eq!(GrabMouse::parse(Some("1")).unwrap(), On);
        assert!(GrabMouse::parse(Some("sometimes")).is_err());
    }

    /// Feed a whole batch through a fresh filter in legacy arrows mode (no split
    /// across reads), returning the forwarded bytes.
    fn filter_once(buf: &[u8], app_cursor_keys: bool) -> Vec<u8> {
        MouseFilter::default().feed(buf, app_cursor_keys, false).bytes
    }

    #[test]
    fn grabbed_wheel_becomes_arrows_and_other_events_drop() {
        // Wheel-up (Cb 64) and wheel-down (Cb 65) → CSI cursor keys; a click
        // (Cb 0) and motion are dropped; surrounding literal bytes survive.
        assert_eq!(filter_once(b"\x1b[<64;10;5M", false), b"\x1b[A");
        assert_eq!(filter_once(b"\x1b[<65;10;5M", false), b"\x1b[B");
        assert_eq!(filter_once(b"\x1b[<0;3;4M", false), b"");
        assert_eq!(filter_once(b"\x1b[<0;3;4m", false), b"");
        // Application cursor keys → SS3 form.
        assert_eq!(filter_once(b"\x1b[<64;1;1M", true), b"\x1bOA");
        assert_eq!(filter_once(b"\x1b[<65;1;1M", true), b"\x1bOB");
        // Literal bytes around a wheel event pass through; two ticks coalesce.
        assert_eq!(filter_once(b"a\x1b[<64;1;1Mb\x1b[<65;1;1M", false), b"a\x1b[Ab\x1b[B");
        // A plain keystroke is untouched.
        assert_eq!(filter_once(b"x", false), b"x");
    }

    #[test]
    fn non_mouse_escape_sequences_round_trip_losslessly() {
        // The filter must never CORRUPT real input. A real arrow key (ESC [ A),
        // a ctrl-arrow, an ESC O cursor key, and a control byte all emerge
        // verbatim once complete — the candidate dies at the non-`<` byte and
        // everything buffered is flushed unchanged.
        assert_eq!(filter_once(b"\x1b[A", false), b"\x1b[A"); // real up-arrow
        assert_eq!(filter_once(b"\x1b[1;5C", false), b"\x1b[1;5C"); // ctrl-right
        assert_eq!(filter_once(b"\x1bOA", false), b"\x1bOA"); // SS3 up
        assert_eq!(filter_once(b"\x03", false), b"\x03"); // Ctrl-C

        // A lone trailing ESC is HELD (it could begin a mouse seq next read) —
        // the byte machine's nature, matching mosh's UserInput. It is not lost:
        // the next byte completes the decision and flushes it.
        let mut f = MouseFilter::default();
        assert_eq!(f.feed(b"\x1b", false, false).bytes, b"", "lone ESC held pending next byte");
        assert_eq!(f.feed(b"a", false, false).bytes, b"\x1ba", "next byte flushes the held ESC");
    }

    #[test]
    fn grabbed_split_sequence_reassembles_at_any_boundary() {
        // posh#52: the persistent state machine reassembles a wheel sequence
        // split across reads at EVERY byte boundary, with no raw leak — the
        // case the old buffer-scan could only partly handle.
        for split in 1..b"\x1b[<64;10;5M".len() {
            let seq = b"\x1b[<64;10;5M";
            let mut f = MouseFilter::default();
            let mut out = f.feed(&seq[..split], false, false).bytes;
            out.extend(f.feed(&seq[split..], false, false).bytes);
            assert_eq!(out, b"\x1b[A", "split at {split} must reassemble to one arrow");
        }
    }

    #[test]
    fn grab_flip_mid_sequence_hands_back_the_held_partial() {
        // posh#52 / review candidate 1: if grab disengages (app took the
        // mouse) while a wheel sequence is half-read, the held prefix must be
        // handed back, not dropped — so the app receives the complete event.
        let mut f = MouseFilter::default();
        assert_eq!(f.feed(b"\x1b[<64", false, false).bytes, b"", "front half held while grabbed");
        // Grab flips off; the caller drains the partial and prepends the tail.
        let pending = f.take_pending();
        assert_eq!(pending, b"\x1b[<64", "held prefix returned, not lost");
        let mut delivered = pending;
        delivered.extend_from_slice(b";1;1M");
        assert_eq!(delivered, b"\x1b[<64;1;1M", "app gets the whole sequence");
        // And the filter is back at Ground for whatever comes next.
        assert_eq!(f.feed(b"x", false, false).bytes, b"x");
    }

    #[test]
    fn grabbed_partial_is_bounded_and_flushed_not_held_forever() {
        // An ESC[< that never terminates must not grow the buffer without
        // bound: past MAX_MOUSE_SEQ it isn't a real mouse sequence, so it's
        // flushed raw rather than swallowing input indefinitely.
        let mut junk = b"\x1b[<".to_vec();
        junk.extend(std::iter::repeat(b'9').take(MAX_MOUSE_SEQ));
        let out = filter_once(&junk, false);
        assert_eq!(out, junk, "over-long candidate is flushed literally");
    }

    #[test]
    fn wheel_active_requires_primary_screen_without_app_mouse_mode() {
        let mut st = test_state(5, 20);
        // Bare prompt, primary screen, no app mouse mode → wheel intercepted.
        assert!(wheel_active(&st));
        // App enables mouse tracking → posh steps back, passes events through.
        st.server_term.process(b"\x1b[?1000h");
        assert!(!wheel_active(&st));
        // No app mouse mode again, but on the alt screen → no scrollback there.
        st.server_term.process(b"\x1b[?1000l\x1b[?1049h");
        assert!(!wheel_active(&st));
    }

    #[test]
    fn echo_safe_gate_requires_echo_on_and_primary_screen() {
        // FDR 0006: echo-safety is computed from the remote PTY echoing AND the
        // primary screen being active. Echo-off (password) or alt-screen
        // (full-screen app) suppress. The model decides what to do with the
        // flag via set_echo_safe — the optimistic model drops its overlay, mosh
        // ignores it — so this gate is now model-independent.
        let mut st = test_state(24, 80);
        st.predict = predict::build(PredictionModel::Optimistic, RenderStyle::Replace, false).0;

        assert!(!optimistic_echo_on(&st), "default echo_on=false => off");

        st.echo_on = true;
        assert!(optimistic_echo_on(&st), "echo on, primary screen => on");

        st.server_term.process(b"\x1b[?1049h"); // enter alt screen
        assert!(st.server_term.is_alt_screen());
        assert!(!optimistic_echo_on(&st), "alt-screen suppresses");
        st.server_term.process(b"\x1b[?1049l");

        st.echo_on = false;
        assert!(!optimistic_echo_on(&st), "echo off (password) suppresses");
    }

    #[test]
    fn resolve_ipv6_literal_with_brackets_in_port_form() {
        let addr = resolve("::1", 60001, Family::Auto).unwrap();
        match addr {
            SocketAddr::V6(a) => assert_eq!(a.ip().to_string(), "::1"),
            SocketAddr::V4(_) => panic!("expected v6"),
        }
    }

    /// A real RawMode over a throwaway pty slave. `dispatch_palette_action`
    /// needs a `&RawMode` (the suspend command restores/re-enters raw mode).
    fn pty_raw_mode() -> RawMode {
        // SAFETY: openpty fills m/s with valid fds; the slave is a tty, which is
        // all RawMode::enable needs. The fds intentionally leak for the test.
        unsafe {
            let (mut m, mut s) = (0, 0);
            assert_eq!(
                libc::openpty(
                    &mut m,
                    &mut s,
                    std::ptr::null_mut(),
                    std::ptr::null::<libc::termios>() as *mut _,
                    std::ptr::null_mut(),
                ),
                0,
                "openpty"
            );
            let _ = m; // keep the pty pair alive for the test
            RawMode::enable(s).unwrap()
        }
    }

    #[test]
    fn dispatch_shell_open_sets_escape_flag() {
        // The palette's "Shell out" command sets CLIENT_FLAG_ESCAPE (FDR 0008)
        // and asks to send promptly.
        let raw = pty_raw_mode();
        let mut st = test_state(24, 80);
        let send = dispatch_palette_action(&mut st, &raw, "shell.open", &json!({}), 0);
        assert!(send, "shell-out asks to send promptly");
        assert_ne!(st.flags & sync::CLIENT_FLAG_ESCAPE, 0, "escape flag set");
    }

    #[test]
    fn dispatch_quit_requests_shutdown() {
        let raw = pty_raw_mode();
        let mut st = test_state(24, 80);
        let send = dispatch_palette_action(&mut st, &raw, "app.quit", &json!({}), 0);
        assert!(send, "quit asks to send promptly");
        assert!(st.shutdown_requested, "quit requests shutdown");
    }

    #[test]
    fn dispatch_resync_sets_one_shot_resync_flag() {
        // The palette's "Reset & resync" command sets CLIENT_FLAG_RESYNC and asks
        // to send promptly so the server forces a Full keyframe (#wedge).
        let raw = pty_raw_mode();
        let mut st = test_state(24, 80);
        let send = dispatch_palette_action(&mut st, &raw, "session.resync", &json!({}), 0);
        assert!(send, "resync asks to send promptly");
        assert_ne!(st.flags & sync::CLIENT_FLAG_RESYNC, 0, "resync flag set");
    }

    #[test]
    fn resync_banner_clears_once_full_applies() {
        // #93: "Reset & resync" sets a sticky "resyncing…" banner; the forced
        // Full keyframe applying must drop it, otherwise it persists forever.
        let raw = pty_raw_mode();
        let mut st = test_state(3, 20);
        let _ = dispatch_palette_action(&mut st, &raw, "session.resync", &json!({}), 0);
        assert_eq!(
            st.notify.message(),
            "resyncing\u{2026}",
            "resync sets the sticky banner"
        );
        let full = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"recovered".to_vec()),
        };
        assert!(apply_frame(&mut st, &full));
        assert_eq!(
            st.notify.message(),
            "",
            "the applied Full clears the resync banner"
        );
    }

    #[test]
    fn resync_banner_survives_an_unrelated_diff() {
        // A Diff advancing the model is not the resync result, so the banner
        // must stay up until the forced Full lands (#93).
        let raw = pty_raw_mode();
        let mut st = test_state(3, 20);
        let base = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"a".to_vec()),
        };
        assert!(apply_frame(&mut st, &base));
        let _ = dispatch_palette_action(&mut st, &raw, "session.resync", &json!({}), 0);
        let diff = ServerFrame {
            frame_num: 2,
            body: FrameBody::Diff {
                base: 1,
                diff: sync::make_diff(b"a", b"ab"),
            },
            ..base
        };
        assert!(apply_frame(&mut st, &diff));
        assert_eq!(
            st.notify.message(),
            "resyncing\u{2026}",
            "a Diff must not clear the resync banner"
        );
    }

    #[test]
    fn dispatch_forensics_without_wedge_reports_nothing_pending() {
        // The "Dump wedge forensics" command is local (no wire send) and, with
        // no apply-stall pending, says so rather than writing a stale bundle.
        let raw = pty_raw_mode();
        let mut st = test_state(24, 80);
        let send = dispatch_palette_action(&mut st, &raw, "session.forensics", &json!({}), 0);
        assert!(!send, "forensics capture is local, no wire send");
        assert!(
            st.notify.message().contains("nothing pending"),
            "notify: {}",
            st.notify.message(),
        );
    }

    #[test]
    fn apply_frame_stashes_reack_on_short_base_diff() {
        // A Diff whose base matches applied_num but whose prefix+suffix exceeds
        // the client's (shorter) applied_data -> apply_diff None -> ReackAndWait.
        // The body must be stashed for forensics. (Auto-capture file write is
        // suppressed here via the one-shot guard so the test does no I/O.)
        let mut st = test_state(24, 80);
        st.applied_num = 5;
        st.applied_data = b"PREFIX".to_vec(); // len 6 < prefix+suffix below
        st.forensic_captured = true; // suppress the auto-capture write
        let diff = sync::make_diff(b"PREFIX_oldmiddle_SUFFIX", b"PREFIX_newmiddle_SUFFIX");
        let frame = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 6,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Diff { base: 5, diff },
        };
        assert!(apply_frame(&mut st, &frame), "reack re-acks (returns true)");
        assert_eq!(st.applied_num, 5, "model untouched on reack");
        let (num, base, kind, _) = st.last_reack.as_ref().expect("reack body stashed");
        assert_eq!((*num, *base), (6, 5));
        assert_eq!(*kind, FrameKind::Diff);
        assert_ne!(
            st.flags & sync::CLIENT_FLAG_RESYNC,
            0,
            "an apply-stall auto-escalates: request a Full keyframe (#90)",
        );
    }

    #[test]
    fn apply_frame_advance_clears_reack_latch() {
        let mut st = test_state(24, 80);
        st.last_reack = Some((9, 8, FrameKind::Diff, vec![1, 2, 3]));
        st.forensic_captured = true;
        // A Full keyframe advances and must clear the forensic latch so a later
        // wedge episode captures afresh.
        let frame = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"\x1b[2J\x1b[Hhi".to_vec()),
        };
        assert!(apply_frame(&mut st, &frame));
        assert!(st.last_reack.is_none(), "advance clears stale reack");
        assert!(!st.forensic_captured, "advance resets the one-shot guard");
    }

    #[test]
    fn palette_commands_includes_both_logging_scopes() {
        let cmds = palette_commands(false);
        let arr = cmds.as_array().expect("commands is an array");
        let names: Vec<&str> = arr.iter().filter_map(|c| c["name"].as_str()).collect();
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("client debug logging")),
            "client logging missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("server debug logging")),
            "server logging missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("resync")),
            "resync command missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.to_lowercase().contains("wedge forensics")),
            "forensics command missing: {names:?}"
        );
        assert_eq!(arr.len(), 11, "expected 11 commands, got {names:?}");
    }

    #[test]
    fn dispatch_server_logging_sets_one_shot_wire_flag() {
        // logging.set scope=server requests the toggle over the wire (#3): an
        // idempotent one-shot flag, not a client-local change.
        let raw = pty_raw_mode();

        let mut on = test_state(24, 80);
        let send = dispatch_palette_action(
            &mut on,
            &raw,
            "logging.set",
            &json!({ "scope": "server", "enabled": true }),
            0,
        );
        assert!(send, "server-logging toggle sends promptly");
        assert_ne!(on.flags & sync::CLIENT_FLAG_LOG_ON, 0, "LOG_ON set");
        assert_eq!(on.flags & sync::CLIENT_FLAG_LOG_OFF, 0);

        let mut off = test_state(24, 80);
        dispatch_palette_action(
            &mut off,
            &raw,
            "logging.set",
            &json!({ "scope": "server", "enabled": false }),
            0,
        );
        assert_ne!(off.flags & sync::CLIENT_FLAG_LOG_OFF, 0, "LOG_OFF set");
        assert_eq!(off.flags & sync::CLIENT_FLAG_LOG_ON, 0);
    }

    /// ClientState over a throwaway loopback connection, for unit tests
    /// of frame application and composition.
    fn test_state(rows: u16, cols: u16) -> ClientState {
        let key = Key::random();
        let conn = Connection::client("127.0.0.1:9".parse().unwrap(), &key).unwrap();
        ClientState {
            conn,
            fragmenter: Fragmenter::new(),
            outbox: InputOutbox::new(),
            rows,
            cols,
            flags: 0,
            last_send: 0,
            applied_num: 0,
            applied_data: Vec::new(),
            server_term: Terminal::with_scrollback(rows, cols, 0),
            scrollback: ScrollbackRing::new(SCROLLBACK_RING_DEPTH),
            suppress_scrollback_once: false,
            last_drawn: Snapshot::blank(rows, cols),
            initialized: false,
            predict: predict::build(PredictionModel::Never, RenderStyle::Replace, false).0,
            renderer: predict::build(PredictionModel::Never, RenderStyle::Replace, false).1,
            predict_model: PredictionModel::Never,
            predict_render: RenderStyle::Replace,
            predict_overwrite: false,
            notify: NotificationEngine::new(0),
            grab_mouse: GrabMouse::Off,
            mouse_filter: MouseFilter::default(),
            quit_pending: false,
            shutdown_requested: false,
            shutdown_requested_at: 0,
            shutdown_seen: false,
            exit_status: 0,
            last_render_state: (u64::MAX, u64::MAX),
            last_render_overlays: false,
            scroll_offset: 0,
            last_scroll_state: None,
            echo_on: false,
            server_log_on: false,
            input_sent: VecDeque::new(),
            framesync: framesync::FrameSync::DumpDiff,
            applier: Box::new(framesync::DumpDiff),
            stats: Stats::new(),
            palette: None,
            last_reack: None,
            forensic_captured: false,
        }
    }

    #[test]
    fn dispatch_echo_set_swaps_predictor_and_banners() {
        let raw = pty_raw_mode();
        let mut st = test_state(3, 30);
        st.predict_model = PredictionModel::Never;
        st.initialized = true;

        let send = dispatch_palette_action(&mut st, &raw, "echo.set", &json!({ "model": "optimistic" }), 0);
        assert!(!send, "echo.set is client-local, no prompt send");
        assert_eq!(st.predict_model, PredictionModel::Optimistic);
        assert!(st.notify.message().contains("Optimistic"), "banner names the new model");
        assert!(!st.initialized, "swapping the predictor forces a clean repaint");
    }

    #[test]
    fn composite_palette_keeps_a_tall_panel_on_screen() {
        // #3: a panel nearly as tall as the screen must not have its bottom
        // commands clipped off by the 1/3-down anchor. Renderer marks its top
        // and near-bottom rows; both must survive compositing onto a short screen.
        let rows = 16u16;
        let mut snap = Snapshot::blank(rows, 80);
        let mut rterm = Terminal::new(rows, 80);
        rterm.process(b"TOPMARK");
        rterm.process(b"\x1b[14;1HBOTMARK"); // row 14 (1-indexed) => screen row 13
        composite_palette(&mut snap, &rterm, rows, 80);

        let text: String = snap.cells.iter().flatten().map(|c| c.ch).collect();
        assert!(text.contains("TOPMARK"), "top of the panel present");
        assert!(
            text.contains("BOTMARK"),
            "bottom of the panel must stay on screen, not clip off the edge"
        );
    }

    #[test]
    fn composite_palette_dims_session_and_anchors_renderer() {
        let mut snap = Snapshot::blank(24, 80);
        snap.cell_mut(0, 0).unwrap().ch = 'X'; // a session glyph to check greying

        let mut rterm = Terminal::new(24, 80);
        rterm.process(b"HI"); // renderer screen: "HI" at row 0, cols 0-1

        composite_palette(&mut snap, &rterm, 24, 80);

        // Session is greyed (glyph kept, style flattened to dim).
        let bg = snap.cell_mut(0, 0).unwrap();
        assert_eq!(bg.ch, 'X');
        assert!(bg.style.dim, "session cell is dimmed behind the palette");

        // bbox("HI") = rows 0, cols 0..1 (bw=2); anchored at row 24/3=8,
        // col (80-2)/2=39.
        assert_eq!(snap.cells[8][39].ch, 'H');
        assert_eq!(snap.cells[8][40].ch, 'I');
    }

    #[test]
    fn compose_skips_idle_ticks_but_not_time_driven_banners() {
        // github #35: idle ticks must not rebuild the full-grid snapshot —
        // but the skip may never eat time-driven output: the lateness
        // banner appears (and counts up) without any model change.
        let mut st = test_state(3, 30);
        assert!(
            !compose_frame(&mut st, 0).is_empty(),
            "first compose paints from scratch"
        );
        assert!(
            compose_frame(&mut st, 100).is_empty(),
            "idle tick composes nothing"
        );
        let late = compose_frame(&mut st, 10_000);
        assert!(
            String::from_utf8_lossy(&late).contains("Last contact"),
            "lateness banner must survive the idle fast path: {late:?}"
        );
        assert!(
            !compose_frame(&mut st, 11_000).is_empty(),
            "banner count-up keeps rendering"
        );
    }

    #[test]
    fn compose_renders_on_applied_frames() {
        let mut st = test_state(3, 20);
        let _ = compose_frame(&mut st, 0);
        let frame = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"hello".to_vec()),
        };
        assert!(apply_frame(&mut st, &frame));
        let bytes = compose_frame(&mut st, 10);
        assert!(
            String::from_utf8_lossy(&bytes).contains("hello"),
            "applied frame must compose: {bytes:?}"
        );
        assert!(
            compose_frame(&mut st, 20).is_empty(),
            "and the tick after it is idle again"
        );
    }

    /// RFC 0002 §3: a `BODY_SCROLLBACK` advances the accumulated ring in row
    /// order without touching the visible model, and only when the client is
    /// at the body's base.
    #[test]
    fn scrollback_frames_accumulate_in_ring_order() {
        let mut st = test_state(3, 20);
        // A visible frame first so applied_num advances to a real base.
        let visible = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"prompt$ ".to_vec()),
        };
        assert!(apply_frame(&mut st, &visible));
        assert_eq!(st.applied_num, 1);
        assert!(st.scrollback.is_empty());

        // Two scrollback frames in sequence, each anchored to the prior.
        let sb1 = ServerFrame {
            frame_num: 2,
            body: FrameBody::Scrollback {
                base: 1,
                rows: vec![b"line one\r\n".to_vec(), b"line two\r\n".to_vec()],
            },
            ..visible.clone()
        };
        assert!(apply_frame(&mut st, &sb1));
        assert_eq!(st.applied_num, 2);
        assert_eq!(st.scrollback.len(), 2);
        let sb2 = ServerFrame {
            frame_num: 3,
            body: FrameBody::Scrollback {
                base: 2,
                rows: vec![b"line three\r\n".to_vec()],
            },
            ..visible.clone()
        };
        assert!(apply_frame(&mut st, &sb2));
        assert_eq!(st.applied_num, 3);
        assert_eq!(st.scrollback.len(), 3);
        assert_eq!(st.scrollback.row(0), Some(&b"line one\r\n"[..]));
        assert_eq!(st.scrollback.row(2), Some(&b"line three\r\n"[..]));

        // A body whose base does not match the client's state is not applied
        // (no double-append), and re-acks our newer state.
        let stale = ServerFrame {
            frame_num: 4,
            body: FrameBody::Scrollback {
                base: 2, // we are at 3
                rows: vec![b"dup\r\n".to_vec()],
            },
            ..visible.clone()
        };
        assert!(apply_frame(&mut st, &stale));
        assert_eq!(st.scrollback.len(), 3, "base mismatch must not append");
        assert_eq!(st.applied_num, 3);
    }

    /// RFC 0002 §1/§4: the client advertises `SCROLLBACK` in steady state,
    /// but ceases for exactly the post-resize message so the server restarts
    /// appended-row counting at the new width, then resumes.
    #[test]
    fn resize_ceases_scrollback_advertisement_for_one_message() {
        let mut st = test_state(5, 20);
        // Steady state: advertised every message.
        let caps = outgoing_caps(&mut st);
        assert!(caps::find(&caps, caps::CAP_SCROLLBACK).is_some());

        // Simulate the SIGWINCH bookkeeping: ring dropped, advertisement
        // suppressed once.
        st.scrollback.append(&[b"row\r\n".to_vec()]);
        st.scrollback.clear();
        st.suppress_scrollback_once = true;
        assert!(st.scrollback.is_empty());

        // The resize message must NOT advertise scrollback (still carries
        // the rest of the table).
        let caps = outgoing_caps(&mut st);
        assert!(
            caps::find(&caps, caps::CAP_SCROLLBACK).is_none(),
            "resize message must cease scrollback"
        );
        assert!(caps::find(&caps, caps::CAP_EXIT_STATUS).is_some());

        // And the very next message re-advertises to resume accumulation.
        let caps = outgoing_caps(&mut st);
        assert!(
            caps::find(&caps, caps::CAP_SCROLLBACK).is_some(),
            "scrollback resumes after the resize message"
        );
    }

    /// RFC 0002 §3: a `Full` visible reset re-establishes the visible screen
    /// but MUST NOT clear the durable accumulated scrollback ring.
    #[test]
    fn full_body_preserves_accumulated_scrollback() {
        let mut st = test_state(3, 20);
        let base = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"a".to_vec()),
        };
        assert!(apply_frame(&mut st, &base));
        let sb = ServerFrame {
            frame_num: 2,
            body: FrameBody::Scrollback {
                base: 1,
                rows: vec![b"kept\r\n".to_vec()],
            },
            ..base.clone()
        };
        assert!(apply_frame(&mut st, &sb));
        assert_eq!(st.scrollback.len(), 1);

        // A later Full (e.g. after loss) resets the visible model only.
        let full = ServerFrame {
            frame_num: 3,
            body: FrameBody::Full(b"recovered".to_vec()),
            ..base
        };
        assert!(apply_frame(&mut st, &full));
        assert_eq!(st.scrollback.len(), 1, "Full must not clear the ring");
        assert_eq!(st.scrollback.row(0), Some(&b"kept\r\n"[..]));
    }

    // --- scrollback scroll-view (FDR 0005) -----------------------------------

    #[test]
    fn scroll_mode_reports_wheel_ticks_not_arrows() {
        // scroll=true: wheel up/down become ticks (+/-), not arrow keys.
        let up = MouseFilter::default().feed(b"\x1b[<64;1;1M", false, true);
        assert_eq!(up.wheel, 1);
        assert!(up.bytes.is_empty(), "scroll mode emits no arrow bytes");
        let down = MouseFilter::default().feed(b"\x1b[<65;1;1M", false, true);
        assert_eq!(down.wheel, -1);
        // A click is dropped (wheel 0); surrounding keystrokes pass through.
        let mixed = MouseFilter::default().feed(b"a\x1b[<0;3;4Mb", false, true);
        assert_eq!(mixed.wheel, 0);
        assert_eq!(mixed.bytes, b"ab");
    }

    #[test]
    fn scroll_offset_clamps_to_ring_and_returns_to_live_at_bottom() {
        let mut st = test_state(5, 20);
        for _ in 0..2 {
            st.scrollback.append(&[b"x\r\n".to_vec()]); // ring depth 2
        }
        scroll_by(&mut st, 1); // +WHEEL_STEP lines, clamped to the ring depth
        assert_eq!(st.scroll_offset, 2);
        scroll_by(&mut st, -1); // back down past the bottom → live view
        assert_eq!(st.scroll_offset, 0);
    }

    #[test]
    fn append_while_scrolled_bumps_offset_to_freeze_viewport() {
        let mut st = test_state(3, 20);
        for i in 0..5 {
            st.scrollback.append(&[format!("r{i}\r\n").into_bytes()]);
        }
        st.applied_num = 1;
        set_scroll(&mut st, 5); // scrolled to the top of history
        // Two more rows scroll off while we are scrolled up.
        let sb = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 2,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Scrollback {
                base: 1,
                rows: vec![b"r5\r\n".to_vec(), b"r6\r\n".to_vec()],
            },
        };
        assert!(apply_frame(&mut st, &sb));
        assert_eq!(st.scrollback.len(), 7);
        assert_eq!(
            st.scroll_offset, 7,
            "offset bumped by the appended rows so the viewport stays anchored"
        );
    }

    #[test]
    fn scroll_frame_renders_history_window_with_indicator() {
        let mut st = test_state(5, 20);
        for s in ["line0", "line1", "line2", "line3"] {
            st.scrollback.append(&[format!("{s}\r\n").into_bytes()]);
        }
        set_scroll(&mut st, 4); // scroll to the top of history
        let bytes = compose_scroll_frame(&mut st);
        assert!(!bytes.is_empty(), "scroll frame must render");

        // Replay onto a fresh terminal of the same size and read it back.
        let mut v = Terminal::with_scrollback(5, 20, 0);
        v.process(&bytes);
        let snap = Snapshot::from_term(&v);
        let row = |r: u16| -> String {
            (0..20)
                .filter_map(|c| snap.cell(r, c))
                .map(|cell| if cell.ch == '\0' { ' ' } else { cell.ch })
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert!(row(0).contains("SCROLLBACK"), "indicator on the top row: {:?}", row(0));
        assert!(row(1).contains("line1"), "history rendered below the bar: {:?}", row(1));
    }

    #[test]
    fn scroll_frame_is_memoized_until_state_changes() {
        let mut st = test_state(5, 20);
        for _ in 0..4 {
            st.scrollback.append(&[b"h\r\n".to_vec()]);
        }
        set_scroll(&mut st, 3);
        assert!(!compose_scroll_frame(&mut st).is_empty(), "first scroll frame paints");
        assert!(
            compose_scroll_frame(&mut st).is_empty(),
            "unchanged scroll state re-renders nothing"
        );
        set_scroll(&mut st, 2); // offset changed → repaint
        assert!(!compose_scroll_frame(&mut st).is_empty());
    }

    #[test]
    fn deccolm_frame_does_not_resize_local_model_past_tty_width() {
        let mut st = test_state(24, 80);
        let (rows, cols) = (24u16, 80u16);

        // Server dump replaying 132-column mode (DECSET 40 allows DECCOLM,
        // DECSET 3 switches): the local model must stay at the tty size or
        // every subsequent render paints a 132-col image onto 80 cols.
        let frame = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"\x1b[?40h\x1b[?3h132-col mode".to_vec()),
        };
        assert!(apply_frame(&mut st, &frame));
        assert_eq!(st.server_term.rows(), rows);
        assert_eq!(
            st.server_term.cols(),
            cols,
            "DECCOLM resized the client model away from the tty width"
        );
    }
}
