//! Roaming remote client (mosh-client/stmclient port): raw-mode tty, a
//! reliable input stream upload, a local terminal model rebuilt from
//! server frames, speculative local echo (predict.rs), and a minimal-diff
//! renderer (display.rs) so frames morph the screen without flicker.

use std::net::{SocketAddr, ToSocketAddrs};

use posh_term::Terminal;

use crate::pty::{self, RawMode};
use crate::remote::crypto::Key;
use crate::remote::datagram::{Connection, Family};
use crate::remote::display::{self, NotificationEngine, Snapshot};
use crate::remote::predict::{DisplayPreference, PredictionEngine};
use crate::remote::sync::{
    self, ClientMessage, FragmentAssembly, Fragmenter, FrameBody, InputOutbox, ServerFrame,
    HEARTBEAT_INTERVAL,
};
use crate::util::{self, now_ms, Error, Result};

const STDIN: i32 = libc::STDIN_FILENO;
const STDOUT: i32 = libc::STDOUT_FILENO;
const SHUTDOWN_GRACE: u64 = 5000; // ms to wait for the shutdown ack

/// The escape (quit-sequence) key: Ctrl-^ (0x1E), as in mosh.
const ESCAPE_KEY: u8 = 0x1e;
const ESCAPE_PASS_KEY: u8 = b'^';
const ESCAPE_KEY_HELP: &str = "Commands: Ctrl-Z suspends, \".\" quits, \"^\" gives literal Ctrl-^";

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

    let addr = resolve(host, port, family)?;
    let conn = Connection::client(addr, &key)?;

    let raw = RawMode::enable(STDIN)?;
    let result = client_loop(conn, prediction, predict_overwrite, &raw, addr.port());
    let _ = util::write_all_retry(STDOUT, display::close(), 1000);
    drop(raw);
    eprintln!("\nposh: [client exited]");
    result
}

fn resolve(host: &str, port: u16, family: Family) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| Error(format!("could not resolve {host}: {e}")))?
        .collect();
    let pick = match family {
        Family::Inet => addrs.iter().find(|a| a.is_ipv4()),
        Family::Inet6 => addrs.iter().find(|a| a.is_ipv6()),
        // Prefer IPv4 (the common path for roaming UDP), fall back to v6.
        Family::Auto => addrs.iter().find(|a| a.is_ipv4()).or_else(|| addrs.first()),
    };
    pick.copied()
        .ok_or_else(|| Error(format!("no suitable addresses for {host}")))
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
    /// What the physical tty currently shows.
    last_drawn: Snapshot,
    /// False when the outer terminal state is unknown (startup, resize,
    /// Ctrl-L): the next frame repaints from scratch.
    initialized: bool,
    predict: PredictionEngine,
    notify: NotificationEngine,
    quit_pending: bool,
    shutdown_requested: bool,
    shutdown_requested_at: u64,
    shutdown_seen: bool,
    /// (applied_num, server_term generation) at the last compose, plus
    /// whether any overlay was live then — the idle fast-path key. github #35.
    last_render_state: (u64, u64),
    last_render_overlays: bool,
}

fn client_loop(
    conn: Connection,
    prediction: DisplayPreference,
    predict_overwrite: bool,
    raw: &RawMode,
    port: u16,
) -> Result<()> {
    util::install_client_signal_handlers();
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
        last_drawn: Snapshot::blank(rows, cols),
        initialized: false,
        predict: PredictionEngine::new(prediction, predict_overwrite),
        notify: NotificationEngine::new(now),
        quit_pending: false,
        shutdown_requested: false,
        shutdown_requested_at: 0,
        shutdown_seen: false,
        last_render_state: (u64::MAX, u64::MAX),
        last_render_overlays: false,
    };
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
    send_message(&mut st);

    loop {
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
            Err(e) => return Err(e.into()),
        }

        if util::take_flag(&util::SIGWINCH_RECEIVED) {
            let size = pty::term_size(STDOUT);
            st.rows = size.0;
            st.cols = size.1;
            st.predict.reset();
            st.initialized = false; // full repaint at the new size
            send_now = true;
        }

        if util::take_flag(&util::SIGTERM_RECEIVED) {
            // SIGTERM/SIGINT/SIGHUP: wind down through the normal shutdown
            // handshake so run() restores the tty and the server hangs up
            // the shell instead of lingering until the network timeout.
            request_shutdown(&mut st);
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
                    request_shutdown(&mut st);
                    send_now = true;
                }
                Ok(n) => {
                    if process_user_input(&mut st, &buf[..n], raw) {
                        send_now = true;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
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
                        if process_frame(&mut st, &frame) {
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
                return Err(Error(format!(
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
        render(&mut st, now);

        if send_now
            || ((!st.outbox.is_empty() || st.flags != 0)
                && now.saturating_sub(st.last_send) >= st.conn.rto())
            || now.saturating_sub(st.last_send) >= HEARTBEAT_INTERVAL
        {
            send_message(&mut st);
        }

        if st.shutdown_seen {
            // Shell exited (or our quit was acknowledged); the final-state
            // ack went out just above.
            return Ok(());
        }
        if st.shutdown_requested && now.saturating_sub(st.shutdown_requested_at) >= SHUTDOWN_GRACE {
            return Ok(()); // server unreachable; leave anyway
        }
    }
}

/// mosh stmclient.cc suspend sequence: restore the outer terminal and the
/// tty driver, stop our process group, and on SIGCONT re-enter raw mode and
/// force a full repaint.
fn suspend(st: &mut ClientState, raw: &RawMode) {
    let _ = util::write_all_retry(STDOUT, display::close(), 1000);
    raw.restore();
    let _ = util::write_all_retry(STDOUT, b"\r\n\x1b[37;44m[posh is suspended.]\x1b[m\r\n", 1000);
    util::stop_own_pgroup();
    // Execution resumes here after SIGCONT (fg).
    raw.reapply();
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

/// Feeds user bytes through the Ctrl-^ quit-sequence state machine, the
/// prediction engine, and into the reliable input stream. Returns true when
/// anything needs sending.
fn process_user_input(st: &mut ClientState, buf: &[u8], raw: &RawMode) -> bool {
    let now = now_ms();
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
    st.notify.server_heard(now);
    st.outbox.ack(frame.input_ack);
    st.predict.set_local_frame_acked(frame.input_ack);
    st.predict.set_local_frame_late_acked(frame.echo_ack);
    st.predict.set_send_interval(st.conn.send_interval());
    if frame.flags & sync::FLAG_SHUTDOWN != 0 {
        st.shutdown_seen = true;
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
    };
    if frame.frame_num == st.applied_num {
        return true; // duplicate retransmission: re-ack, don't reapply
    }
    let mut term = Terminal::with_scrollback(st.rows, st.cols, 0);
    term.process(&bytes);
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
    if !bytes.is_empty() {
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

    let base = Snapshot::from_term(&st.server_term);
    st.predict.cull(&base, now);
    let mut next = base;
    st.predict.apply(&mut next);
    st.notify.adjust(now);
    st.notify.apply(&mut next, now);

    let bytes = display::new_frame(st.initialized, &st.last_drawn, &next);
    st.initialized = true;
    st.last_drawn = next;
    bytes
}

fn send_message(st: &mut ClientState) {
    let msg = ClientMessage {
        flags: st.flags,
        caps: vec![], // populated when EXIT_STATUS lands (plan task 6)
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
            last_drawn: Snapshot::blank(rows, cols),
            initialized: false,
            predict: PredictionEngine::new(DisplayPreference::Never, false),
            notify: NotificationEngine::new(0),
            quit_pending: false,
            shutdown_requested: false,
            shutdown_requested_at: 0,
            shutdown_seen: false,
            last_render_state: (u64::MAX, u64::MAX),
            last_render_overlays: false,
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
