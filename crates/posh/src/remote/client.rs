//! Roaming remote client (mosh-client/stmclient port): raw-mode tty, a
//! reliable input stream upload, a local terminal model rebuilt from
//! server frames, speculative local echo (predict.rs), and a minimal-diff
//! renderer (display.rs) so frames morph the screen without flicker.

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Instant;

use posh_term::Terminal;

use crate::pty::{self, RawMode};
use crate::remote::caps;
use crate::remote::crypto::Key;
use crate::remote::datagram::{Connection, Family};
use crate::remote::display::{self, NotificationEngine, Snapshot};
use crate::remote::predict::{DisplayPreference, PredictionEngine};
use crate::remote::stats::{PredictSample, Stats};
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
const ESCAPE_KEY_HELP: &str = "Commands: Ctrl-Z suspends, \".\" quits, \"^\" gives literal Ctrl-^";

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

    let prediction_env = std::env::var("POSH_PREDICTION").ok();
    let prediction = DisplayPreference::parse(prediction_env.as_deref()).map_err(Error)?;
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
    let raw = RawMode::enable(STDIN)?;
    // Take over the alternate screen (mosh smcup); close() below restores
    // the user's pre-connect shell screen on the way out.
    let _ = util::write_all_retry(STDOUT, &display::open(), 1000);
    let result = client_loop(conn, prediction, predict_overwrite, grab_mouse, &raw, addr.port());
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
    /// Not yet rendered (the wheel scroll-view is FDR 0005, out of the wire
    /// contract) — this is the durable accumulation it will read from.
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
    predict: PredictionEngine,
    notify: NotificationEngine,
    /// $POSH_GRAB_MOUSE policy; gates the wheel-grab in grab_active().
    grab_mouse: GrabMouse,
    /// Byte-fed state machine that translates grabbed wheel events to arrows;
    /// its persistent state reassembles sequences split across reads (posh#52).
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
    /// Optional performance instrumentation (POSH_DEBUG_LOG); inert when unset.
    stats: Stats,
}

fn client_loop(
    conn: Connection,
    prediction: DisplayPreference,
    predict_overwrite: bool,
    grab_mouse: GrabMouse,
    raw: &RawMode,
    port: u16,
) -> Result<i32> {
    util::set_nonblocking(STDIN)?;

    let (rows, cols) = pty::term_size(STDOUT);
    let now = now_ms();
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
        predict: PredictionEngine::new(prediction, predict_overwrite),
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
        stats: Stats::new(),
    };
    let result = drive_client(&mut st, raw, port);
    // One final summary regardless of how the loop exited (graceful, timeout,
    // or error), so the log always ends with the last-observed transport state.
    let now = now_ms();
    st.stats.final_client(
        now,
        st.conn.srtt(),
        st.conn.rto(),
        st.conn.send_interval(),
        predict_sample(&st.predict),
        st.predict.srtt_trigger_on(),
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

        let mut fds = [
            util::pollfd(STDIN, libc::POLLIN),
            util::pollfd(st.conn.raw_fd(), libc::POLLIN),
        ];
        let mut send_now = false;
        match util::poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => break 'client Err(e.into()),
        }

        if util::take_flag(&util::SIGWINCH_RECEIVED) {
            let size = pty::term_size(STDOUT);
            st.rows = size.0;
            st.cols = size.1;
            st.predict.reset();
            st.initialized = false; // full repaint at the new size
            // RFC 0002 §4: a width change rewraps the server's ring, so
            // absolute row continuity ends. Drop the accumulated ring,
            // discard the (not-yet-built) scroll view by virtue of the
            // repaint, and stop advertising SCROLLBACK for the resize
            // message so the server restarts appended-row counting afresh.
            st.scrollback.clear();
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
                    if process_user_input(st, &buf[..n], raw) {
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
        st.stats.flush_client(
            now,
            st.conn.srtt(),
            st.conn.rto(),
            st.conn.send_interval(),
            predict_sample(&st.predict),
            st.predict.srtt_trigger_on(),
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

/// Whether posh is grabbing the wheel on the outer terminal right now: the
/// $POSH_GRAB_MOUSE policy is on AND the session app has set no mouse mode of
/// its own (so the wheel would otherwise become arrows in the outer terminal).
/// Both the render side (what mode we assert) and the input side (whether to
/// intercept mouse events) read this, so they can never disagree.
fn grab_active(st: &ClientState) -> bool {
    st.grab_mouse == GrabMouse::On && st.server_term.mouse_mode() == posh_term::MouseMode::None
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

impl MouseFilter {
    /// Feed one input batch; returns the rewritten bytes to forward. Any
    /// incomplete trailing sequence stays in `self` for the next call.
    fn feed(&mut self, buf: &[u8], app_cursor_keys: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(buf.len() + self.pending.len());
        for &b in buf {
            self.step(b, app_cursor_keys, &mut out);
        }
        out
    }

    fn step(&mut self, b: u8, app_cursor_keys: bool, out: &mut Vec<u8>) {
        match self.state {
            MouseState::Ground => {
                if b == 0x1b {
                    self.pending.push(b);
                    self.state = MouseState::Esc;
                } else {
                    out.push(b);
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
                    self.step(b, app_cursor_keys, out);
                }
            }
            MouseState::Bracket => {
                if b == b'<' {
                    self.pending.push(b);
                    self.state = MouseState::Body;
                } else {
                    // ESC [ <other> — a real CSI (arrow, etc.), not mouse.
                    self.flush(out);
                    self.step(b, app_cursor_keys, out);
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
                        Some(64) => out.extend_from_slice(arrow_up(app_cursor_keys)),
                        Some(65) => out.extend_from_slice(arrow_down(app_cursor_keys)),
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
                    self.step(b, app_cursor_keys, out);
                }
            }
        }
    }

    /// Emit the buffered candidate verbatim and reset to Ground (the bytes
    /// weren't a mouse sequence after all).
    fn flush(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.pending);
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
fn process_user_input(st: &mut ClientState, buf: &[u8], raw: &RawMode) -> bool {
    let now = now_ms();

    // When grabbing the wheel, run input through the mouse filter (translating
    // wheel events to arrows, dropping other mouse events) before the byte
    // loop, so the rest of the path is unchanged. The filter's persistent
    // state reassembles sequences split across reads (posh#52).
    let grabbed;
    let buf: &[u8] = if grab_active(st) {
        let app_cursor_keys = st.server_term.app_cursor_keys();
        grabbed = st.mouse_filter.feed(buf, app_cursor_keys);
        &grabbed
    } else {
        // Not grabbing. If the filter holds a partial from when grab was last
        // active (the app enabled its own mouse mode mid-sequence, flipping
        // grab off between reads), hand those bytes back and prepend them so
        // the app — which now wants mouse events — receives the complete
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

    // Don't predict for bulk pastes.
    let paste = buf.len() > 100;
    if paste {
        st.predict.reset();
    }

    let mut dirty = false;
    let push = |st: &mut ClientState, byte: u8| {
        if !paste {
            st.predict.set_local_frame_sent(st.outbox.end_offset());
            st.predict.new_user_byte(byte, &st.last_drawn, now);
        }
        st.outbox.push(&[byte]);
    };

    for &byte in buf {
        if st.quit_pending {
            st.quit_pending = false;
            match byte {
                b'.' => {
                    request_shutdown(st);
                    dirty = true;
                    continue;
                }
                0x1a => {
                    // Ctrl-^ Ctrl-Z: suspend the client (mosh suspend
                    // sequence), not the remote foreground job.
                    suspend(st, raw);
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
            if st.notify.message() == ESCAPE_KEY_HELP {
                st.notify.set_message("", false, now);
            }
            dirty = true;
            continue;
        }

        if byte == ESCAPE_KEY {
            st.quit_pending = true;
            st.notify.set_message(ESCAPE_KEY_HELP, true, now);
            continue;
        }

        if byte == 0x0c {
            // Ctrl-L: ask for a full repaint of the outer terminal.
            st.initialized = false;
        }

        push(st, byte);
        dirty = true;
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
        FrameBody::Diff { .. } => st.stats.record_frame_diff(),
        FrameBody::Empty => st.stats.record_frame_empty(),
        // Scrollback bodies carry no visible-screen change; they are not
        // part of the Full/Diff/Empty economics the stats track.
        FrameBody::Scrollback { .. } => {}
    }
    st.notify.server_heard(now);
    st.outbox.ack(frame.input_ack);
    st.predict.set_local_frame_acked(frame.input_ack);
    st.predict.set_local_frame_late_acked(frame.echo_ack);
    st.predict.set_send_interval(st.conn.send_interval());
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
fn apply_frame(st: &mut ClientState, frame: &ServerFrame) -> bool {
    if frame.frame_num < st.applied_num {
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
            return true; // duplicate retransmission: re-ack, don't reapply
        }
        if *base != st.applied_num {
            return true; // growth against a state we are not at; re-ack
        }
        st.scrollback.append(rows);
        st.applied_num = frame.frame_num;
        return true;
    }
    let bytes: Vec<u8> = match &frame.body {
        FrameBody::Empty => return false,
        FrameBody::Full(bytes) => bytes.clone(),
        FrameBody::Diff { base, diff } => {
            if *base != st.applied_num {
                // Diff against a state we do not hold; the server will fall
                // back to a full dump once it sees our (stale) ack.
                return true;
            }
            match sync::apply_diff(&st.applied_data, diff) {
                Some(bytes) => bytes,
                None => return true,
            }
        }
        // Handled above (returns early); listed so the match stays total.
        FrameBody::Scrollback { .. } => unreachable!("scrollback handled above"),
    };
    if frame.frame_num == st.applied_num {
        return true; // duplicate retransmission: re-ack, don't reapply
    }
    let mut term = Terminal::with_scrollback(st.rows, st.cols, 0);
    // Time the full-dump re-parse — the client-side mirror of the server's
    // dump_vt_us, and the suspected hot spot (it grows with the dump's size).
    let apply_timer = st.stats.enabled().then(Instant::now);
    term.process(&bytes);
    if let Some(t) = apply_timer {
        st.stats.record_apply_us(t.elapsed().as_micros() as u64);
    }
    // A DECCOLM replayed from the server dump resizes the model to 132/80
    // columns regardless of the real tty: clamp back so renders never paint
    // a wider image than the tty can show (the server-side mode is the
    // server model's concern, not the local render width).
    if term.rows() != st.rows || term.cols() != st.cols {
        term.resize(st.rows, st.cols);
    }
    st.server_term = term;
    st.applied_num = frame.frame_num;
    st.applied_data = bytes;
    true
}

/// mosh's output_new_frame: server state + prediction overlay + status
/// banner, diffed against what the tty currently shows.
fn render(st: &mut ClientState, now: u64) {
    let bytes = compose_frame(st, now);
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
    let overlays_live =
        st.predict.active() || !st.notify.message().is_empty() || st.notify.server_late(now);
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
    let base = Snapshot::from_term(&st.server_term);
    st.predict.cull(&base, now);
    let mut next = base;
    st.predict.apply(&mut next);
    st.notify.adjust(now);
    st.notify.apply(&mut next, now);

    let grab = grab_active(st);
    let bytes = display::new_frame(st.initialized, &st.last_drawn, &next, grab);
    st.initialized = true;
    st.last_drawn = next;
    if let Some(t) = compose_timer {
        st.stats.record_compose_us(t.elapsed().as_micros() as u64);
    }
    bytes
}

/// Snapshots the prediction engine's display gauges for the stats log.
fn predict_sample(predict: &PredictionEngine) -> PredictSample {
    let (correct, nocredit, incorrect) = predict.prediction_outcomes();
    PredictSample {
        active: predict.active(),
        shown: predict.shown_cells(),
        epoch_lag: predict.epoch_lag(),
        resets: predict.mispredict_resets(),
        correct,
        nocredit,
        incorrect,
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

    /// Feed a whole batch through a fresh filter (no split across reads).
    fn filter_once(buf: &[u8], app_cursor_keys: bool) -> Vec<u8> {
        MouseFilter::default().feed(buf, app_cursor_keys)
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
        assert_eq!(f.feed(b"\x1b", false), b"", "lone ESC held pending next byte");
        assert_eq!(f.feed(b"a", false), b"\x1ba", "next byte flushes the held ESC");
    }

    #[test]
    fn grabbed_split_sequence_reassembles_at_any_boundary() {
        // posh#52: the persistent state machine reassembles a wheel sequence
        // split across reads at EVERY byte boundary, with no raw leak — the
        // case the old buffer-scan could only partly handle.
        for split in 1..b"\x1b[<64;10;5M".len() {
            let seq = b"\x1b[<64;10;5M";
            let mut f = MouseFilter::default();
            let mut out = f.feed(&seq[..split], false);
            out.extend(f.feed(&seq[split..], false));
            assert_eq!(out, b"\x1b[A", "split at {split} must reassemble to one arrow");
        }
    }

    #[test]
    fn grab_flip_mid_sequence_hands_back_the_held_partial() {
        // posh#52 / review candidate 1: if grab disengages (app took the
        // mouse) while a wheel sequence is half-read, the held prefix must be
        // handed back, not dropped — so the app receives the complete event.
        let mut f = MouseFilter::default();
        assert_eq!(f.feed(b"\x1b[<64", false), b"", "front half held while grabbed");
        // Grab flips off; the caller drains the partial and prepends the tail.
        let pending = f.take_pending();
        assert_eq!(pending, b"\x1b[<64", "held prefix returned, not lost");
        let mut delivered = pending;
        delivered.extend_from_slice(b";1;1M");
        assert_eq!(delivered, b"\x1b[<64;1;1M", "app gets the whole sequence");
        // And the filter is back at Ground for whatever comes next.
        assert_eq!(f.feed(b"x", false), b"x");
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
    fn grab_active_requires_policy_on_and_app_without_mouse() {
        let mut st = test_state(5, 20);
        // Default policy is Off → never grabbing.
        assert!(!grab_active(&st));
        st.grab_mouse = GrabMouse::On;
        // Policy on, app has no mouse mode → grabbing.
        assert!(grab_active(&st));
        // App enables mouse tracking → posh steps back, passes events through.
        st.server_term.process(b"\x1b[?1000h");
        assert!(!grab_active(&st));
    }

    #[test]
    fn resolve_ipv6_literal_with_brackets_in_port_form() {
        let addr = resolve("::1", 60001, Family::Auto).unwrap();
        match addr {
            SocketAddr::V6(a) => assert_eq!(a.ip().to_string(), "::1"),
            SocketAddr::V4(_) => panic!("expected v6"),
        }
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
            predict: PredictionEngine::new(DisplayPreference::Never, false),
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
            stats: Stats::new(),
        }
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
