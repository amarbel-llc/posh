//! Per-session daemon: owns the PTY and broadcasts output to attached
//! clients over a Unix socket (zmx daemonLoop port).

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

use posh_term::{ScreenSwitch, Terminal};

use crate::pty::{self, PtyChild};
use crate::remote::caps;
use crate::remote::display::Snapshot;
use crate::remote::framesync::FrameProducer;
use crate::remote::sync::ServerFrame;
use crate::session::ipc::{self, FrameBuffer, SessionInfo, Tag};
use crate::session::{self, Config};
use crate::util::{self, Error, Result};

const SCROLLBACK: usize = 10_000;

/// A `.castx` recorder writing to a boxed sink (a file, in practice). Built
/// when `$POSH_RECORD_FILE` is set (`posh --record FILE`); tees the session's
/// raw PTY output so `poshterity replay` can reproduce the screen deterministically.
type SessionRecorder = poshterity::castx::Recorder<Box<dyn Write>>;

/// Open the recording named by `$POSH_RECORD_FILE` (if any) and write its
/// header. A failure to open/write only logs and disables recording — it must
/// never stop the session from starting.
fn open_recorder(rows: u16, cols: u16) -> Option<SessionRecorder> {
    let path = std::env::var_os("POSH_RECORD_FILE")?;
    let file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            util::log_write("warn", &format!("--record: cannot open {path:?}: {e}"));
            return None;
        }
    };
    let writer: Box<dyn Write> = Box::new(std::io::BufWriter::new(file));
    let mut rec = poshterity::castx::Recorder::new(writer);
    let header = poshterity::castx::Header {
        version: 2,
        width: cols,
        height: rows,
        poshterity: Some(poshterity::castx::Poshterity {
            v: 1,
            emu_rev: posh_term::emu_rev(),
        }),
    };
    if let Err(e) = rec.write_header(&header) {
        util::log_write("warn", &format!("--record: cannot write header: {e}"));
        return None;
    }
    Some(rec)
}

/// A client whose unsent backlog grows past this is treated as a stuck
/// reader and dropped, so one wedged terminal can't OOM the daemon and take
/// down every other attached client. github #11.
const MAX_CLIENT_BACKLOG: usize = 16 * 1024 * 1024;

/// Ensures the session exists, forking off a daemon when needed. Returns
/// true when a new session was created. The daemon is a double-forked
/// grandchild that never returns from this function (it exits the process).
pub fn ensure_session(cfg: &Config, name: &str, command: Option<Vec<String>>) -> Result<bool> {
    let path = cfg.socket_path(name)?;
    if session::session_socket_exists(&path) {
        match session::probe_session(&path) {
            Ok(_) => {
                if command.is_some() {
                    util::log_write(
                        "warn",
                        &format!("session already exists, ignoring command session={name}"),
                    );
                }
                return Ok(false);
            }
            Err(_) => {
                // Only reclaim the socket if the daemon is genuinely gone; a
                // slow-but-live daemon means the session already exists, so
                // don't remove its socket and spawn a duplicate. github #15.
                if !session::cleanup_stale_socket(&path) {
                    return Ok(false);
                }
            }
        }
    } else if std::fs::symlink_metadata(&path).is_ok() {
        return Err(Error(format!(
            "{} exists and is not a socket",
            path.display()
        )));
    }

    // Bind before forking so a racing client can connect (and queue) as soon
    // as the parent returns.
    let listener =
        UnixListener::bind(&path).map_err(|e| Error(format!("bind {}: {e}", path.display())))?;
    if util::double_fork()? {
        drop(listener);
        std::thread::sleep(std::time::Duration::from_millis(10));
        return Ok(true);
    }
    daemon_main(cfg, name, listener, command);
}

struct ClientConn {
    stream: UnixStream,
    read_buf: FrameBuffer,
    write_buf: Vec<u8>,
    // Zero means "size not yet reported"; ignored for the shared minimum.
    rows: u16,
    cols: u16,
    // Capabilities the client advertised on its `Tag::Init` (RFC 0001 table,
    // github #100). Read by `is_frame_capable` to decide whether this client
    // gets a `FrameProducer` (and thus `Tag::Frame` output) when the session
    // frame-emission gate is on.
    caps: Vec<caps::Cap>,
    // Per-client visible-frame producer (RFC 0008), `Some` exactly when this
    // client advertised frame support AND `$POSH_SESSION_FRAMES` is on. While
    // `Some`, the daemon emits posh-proto `ServerFrame`s (`Tag::Frame`) to this
    // client instead of raw `Tag::Output`; each client diffs against its OWN
    // acked base, so a freshly attached client's first frame is a `Full` while
    // an established one gets a `Diff`. Default `None` ⇒ today's `Tag::Output`.
    producer: Option<FrameProducer>,
}

impl ClientConn {
    fn queue(&mut self, tag: Tag, payload: &[u8]) {
        ipc::append_frame(&mut self.write_buf, tag, payload);
    }

    /// Applies a `Tag::Init` payload: a 4-byte resize prefix that sizes the
    /// PTY, optionally followed by an RFC 0001 capability table (the
    /// framesync handshake, github #100). Returns whether the reported size
    /// was updated. The trailing table is parsed and recorded but NOT acted
    /// on here — the daemon's output path is unchanged this task.
    ///
    /// The resize is decoded from the first 4 bytes only, because `posh`'s
    /// `decode_resize` rejects any non-4-byte payload; a cap-extended Init
    /// must still size the PTY. An absent or malformed trailing table leaves
    /// any previously negotiated caps in place (a bare re-`Init` on SIGCONT
    /// resume does not wipe them).
    fn apply_init(&mut self, payload: &[u8]) -> bool {
        let resized = match payload.get(..4).and_then(ipc::decode_resize) {
            Some((r, w)) => {
                self.rows = r;
                self.cols = w;
                true
            }
            None => false,
        };
        if payload.len() > 4 {
            match caps::decode_table(&payload[4..]) {
                Ok((advertised, _)) => self.caps = advertised,
                Err(e) => util::log_write(
                    "warn",
                    &format!("malformed Init cap table, treating peer as baseline: {e}"),
                ),
            }
        }
        resized
    }

    /// Whether this client advertised the posh-proto frame protocol — i.e. its
    /// `Tag::Init` carried a capability table with `CAP_PROTOCOL_VERSION`. A
    /// baseline (no-table) peer is never frame-capable, so it always receives
    /// raw `Tag::Output`.
    fn is_frame_capable(&self) -> bool {
        caps::find(&self.caps, caps::CAP_PROTOCOL_VERSION).is_some()
    }

    /// Construct this client's `FrameProducer` when the session frame-emission
    /// gate is on AND the client is frame-capable. Idempotent: a bare re-`Init`
    /// (SIGCONT resume) keeps the existing producer (and its acked base) rather
    /// than resetting it. With `gate` off, NEVER constructs a producer, so the
    /// client stays on `Tag::Output` — the Phase 1 safety invariant.
    fn maybe_enable_frames(&mut self, gate: bool) {
        if gate && self.producer.is_none() && self.is_frame_capable() {
            self.producer = Some(FrameProducer::new(self.rows.max(1), self.cols.max(1)));
        }
    }

    /// Produce a visible frame from the supplied screen state and queue it as
    /// `Tag::Frame`. Returns `false` (queuing nothing) when this client has no
    /// producer, so the caller falls back to `Tag::Output`.
    ///
    /// Reliable-as-degenerate (RFC 0008 §3): the socket delivers in order with
    /// no loss, so after queuing the frame we immediately `ack` it — the acked
    /// base is always the last frame, the next frame is a `Diff` against it, and
    /// the producer's retransmit machinery idles. `input_ack`/`echo_ack` are
    /// inert (the socket input stream is itself reliable; Task 1.5).
    fn queue_frame(&mut self, dump: Vec<u8>, snapshot: Snapshot, alt: bool, dims: (u16, u16)) -> bool {
        let encoded = match self.producer.as_mut() {
            None => return false,
            Some(producer) => {
                producer.advance_visible(dump, snapshot, alt, dims, 0);
                // DumpDiff for Phase 1: the local client cannot negotiate
                // CAP_MORPH over the socket yet, so never select MorphDelta.
                let body = producer.encode_visible(false);
                let frame_num = producer.current_num();
                let bytes = ServerFrame {
                    flags: 0,
                    caps: caps::own_table(&[]),
                    frame_num,
                    input_ack: 0,
                    echo_ack: 0,
                    body,
                }
                .encode();
                producer.ack(frame_num);
                bytes
            }
        };
        self.queue(Tag::Frame, &encoded);
        true
    }
}

/// Parses the `$POSH_SESSION_FRAMES` daemon frame-emission gate (RFC 0008 §6):
/// `1`/`true`/`on`/`yes` (case-insensitive, trimmed) turn it ON; anything else
/// — including unset/empty — leaves it OFF. Kept distinct from `$POSH_FRAMESYNC`
/// (the *remote* MorphDelta codec opt-in) so the two are never conflated: this
/// gate decides whether the session daemon emits frames at all, not which codec.
fn parse_frames_gate(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "on" | "yes")
    )
}

/// Whether this daemon emits posh-proto `ServerFrame`s (`Tag::Frame`) to
/// frame-capable clients. DEFAULT OFF: with the gate off no producer is ever
/// constructed, so every client — including a frame-capable one — receives raw
/// `Tag::Output`, byte-for-byte today's behavior. Phase 1 must stay off because
/// the local client cannot consume frames until Phase 2 (RFC 0008 / FDR 0011).
fn session_frames_enabled() -> bool {
    parse_frames_gate(std::env::var("POSH_SESSION_FRAMES").ok().as_deref())
}

/// Broadcasts a PTY-output chunk to every attached client: a posh-proto
/// `ServerFrame` (`Tag::Frame`) for each frame-capable client, the raw `bcast`
/// bytes (`Tag::Output`) for the rest. The dump/snapshot frame inputs are
/// derived once from `term` and cloned per producer — each client diffs against
/// its OWN acked base — and ONLY when at least one client is frame-capable, so a
/// session with none pays exactly today's cost and emits exactly today's
/// `Tag::Output` bytes (the gate-off invariant).
fn broadcast_output(clients: &mut [ClientConn], term: &Terminal, bcast: &[u8]) {
    let frame_inputs = clients.iter().any(|c| c.producer.is_some()).then(|| {
        (
            term.dump_vt(),
            Snapshot::from_term(term),
            term.is_alt_screen(),
            (term.rows(), term.cols()),
        )
    });
    for c in clients.iter_mut() {
        let produced = match &frame_inputs {
            Some((dump, snap, alt, dims)) => c.queue_frame(dump.clone(), snap.clone(), *alt, *dims),
            None => false,
        };
        if !produced {
            c.queue(Tag::Output, bcast);
        }
    }
}

/// Substituted for RIS in the broadcast: the model performed a full reset,
/// so push the outer terminal's shared modes back to defaults without
/// letting it leave the alternate screen the client pinned it to (a raw
/// RIS would switch the outer terminal to its primary buffer — the user's
/// shell — and clear it). DECSTR covers cursor/charsets/SGR/region/keypad
/// and the kitty key stack; the explicit resets cover what DECSTR leaves
/// (mouse, paste, focus, alternate scroll, cursor blink/visibility,
/// DECCKM/reverse-video/autorepeat/LNM/insert, a pending synchronized
/// update, dynamic colors). A repaint of the (now empty) model screen
/// follows from the caller.
const RIS_SUBSTITUTE: &[u8] = b"\x1b[!p\
    \x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?9l\x1b[?1005l\x1b[?1006l\x1b[?1016l\
    \x1b[?2004l\x1b[?1004l\x1b[?1007l\x1b[?12l\x1b[?25h\x1b[?1l\x1b[?5l\x1b[?8h\
    \x1b[?2026l\x1b>\x1b[20l\x1b[4l\x1b]104\x07\x1b]110\x07\x1b]111\x07\x1b]112\x07";

/// Rebuilds a DECSET/DECRST sequence with the alt-screen modes (47/1047/
/// 1049) stripped, so co-set modes still reach the outer terminal (e.g.
/// `CSI ? 1049 ; 2004 h` forwards as `CSI ? 2004 h`). Returns None when
/// nothing remains or the held bytes aren't the plain `ESC [ ? params h/l`
/// shape (interleaved C0s, C1 CSI restarts); dropping the sequence whole
/// is safe because the model-faithful repaint follows either way.
fn strip_alt_screen_params(seq: &[u8]) -> Option<Vec<u8>> {
    let body = seq.strip_prefix(b"\x1b[?")?;
    let (&final_byte, params) = body.split_last()?;
    if !matches!(final_byte, b'h' | b'l') {
        return None;
    }
    let mut kept: Vec<&[u8]> = Vec::new();
    for part in params.split(|&b| b == b';') {
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        // Match numerically so leading zeros ("0047") can't sneak through.
        let n: u32 = std::str::from_utf8(part).ok()?.parse().unwrap_or(0);
        if !matches!(n, 47 | 1047 | 1049) {
            kept.push(part);
        }
    }
    if kept.is_empty() {
        return None;
    }
    let mut out = b"\x1b[?".to_vec();
    out.extend_from_slice(&kept.join(&b';'));
    out.push(final_byte);
    Some(out)
}

/// Virtualizes the application's screen switches in the raw output
/// broadcast.
///
/// Attached clients hold the outer terminal on ITS alternate screen for
/// the whole attach, so detach can restore the user's shell exactly as it
/// was. The inner application's own switch sequences (DECSET/DECRST
/// 47/1047/1049) and RIS must therefore never reach the outer terminal
/// raw: each is excised from the stream and replaced with a repaint of the
/// newly active screen generated from the daemon's terminal model.
///
/// Bytes are held back while the parser is mid-escape/CSI (the only states
/// that can complete into a switch), which also keeps sequences split
/// across PTY reads from being forwarded in halves.
#[derive(Default)]
struct ScreenSwitchFilter {
    held: Vec<u8>,
}

/// Cap on bytes held back mid-sequence; see the flush in `feed`.
const MAX_HELD: usize = 4096;

impl ScreenSwitchFilter {
    /// Feeds one PTY chunk through the model and appends the broadcast
    /// bytes (raw passthrough with switches substituted) to `out`.
    fn feed(&mut self, term: &mut Terminal, chunk: &[u8], out: &mut Vec<u8>) {
        // Fast path: nothing held, parser at rest, and no byte that could
        // begin an escape sequence (0x1b, or 0x9b as a raw C1 CSI).
        if self.held.is_empty()
            && !term.mid_escape()
            && !chunk.iter().any(|&b| b == 0x1b || b == 0x9b)
        {
            term.process(chunk);
            out.extend_from_slice(chunk);
            return;
        }
        for &b in chunk {
            self.held.push(b);
            term.process(&[b]);
            if let Some(kind) = term.take_screen_switch() {
                let seq = std::mem::take(&mut self.held);
                match kind {
                    ScreenSwitch::Reset => out.extend_from_slice(RIS_SUBSTITUTE),
                    ScreenSwitch::Alt => {
                        if let Some(rest) = strip_alt_screen_params(&seq) {
                            out.extend_from_slice(&rest);
                        }
                    }
                }
                out.extend_from_slice(&term.dump_screen_switch());
            } else if !term.mid_escape() {
                out.append(&mut self.held);
            } else if self.held.len() > MAX_HELD {
                // A real switch sequence is ~10 bytes; an escape this long
                // is garbage that can't be excised later anyway. Flush it
                // so a malicious stream can't grow the hold buffer.
                out.append(&mut self.held);
            }
        }
    }
}

/// Elementwise minimum size across all clients that have reported one
/// (tmux `window-size smallest`).
fn min_client_size(clients: &[ClientConn]) -> Option<(u16, u16)> {
    let mut acc: Option<(u16, u16)> = None;
    for c in clients {
        if c.rows == 0 || c.cols == 0 {
            continue;
        }
        acc = Some(match acc {
            None => (c.rows, c.cols),
            Some((r, w)) => (r.min(c.rows), w.min(c.cols)),
        });
    }
    acc
}

fn apply_client_size(clients: &[ClientConn], pty_fd: RawFd, term: &mut Terminal) {
    if let Some((rows, cols)) = min_client_size(clients) {
        pty::set_term_size(pty_fd, rows, cols);
        term.resize(rows, cols);
    }
}

fn daemon_main(
    cfg: &Config,
    name: &str,
    listener: UnixListener,
    command: Option<Vec<String>>,
) -> ! {
    util::redirect_stdio_devnull();
    let _ = util::log_init(&cfg.log_path(name));
    util::install_sigterm_handler();
    let socket_path = cfg.socket_path(name).expect("socket path");
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    // stdio is detached, so the PTY starts at the 24x80 default; the first
    // client Init resizes it.
    let (rows, cols) = (24u16, 80u16);
    let envs = vec![
        ("POSH_SESSION".to_string(), name.to_string()),
        ("POSH_GROUP".to_string(), cfg.group.clone()),
    ];
    let child = match pty::spawn_shell(command.as_deref(), rows, cols, &envs, None) {
        Ok(c) => c,
        Err(e) => {
            util::log_write("error", &format!("failed to spawn pty: {e}"));
            let _ = std::fs::remove_file(&socket_path);
            std::process::exit(1);
        }
    };
    util::log_write(
        "info",
        &format!("daemon started session={name} pid={}", child.pid),
    );

    let _ = listener.set_nonblocking(true);
    let _ = util::set_nonblocking(child.master);

    let mut term = Terminal::with_scrollback(rows, cols, SCROLLBACK);
    let mut clients: Vec<ClientConn> = Vec::new();
    // Join argv with NUL (not spaces) so `posh fork` can recover arguments
    // that contain spaces losslessly. github #18.
    let info_cmd = command.as_ref().map(|c| c.join("\0")).unwrap_or_default();

    // Optional `.castx` recording (posh --record FILE). Best-effort: a failure
    // to open never blocks the session.
    let recorder = open_recorder(rows, cols);

    daemon_loop(
        &listener, &child, &mut term, &mut clients, &info_cmd, &cwd, recorder,
    );

    // Teardown. Reap the shell first: when it already exited (the pty-EIO
    // path) WNOHANG captures its real status before the group kills below.
    // The SIGHUP -> grace -> SIGKILL sequence always runs against the whole
    // process group regardless — background jobs survive the shell's own
    // exit and must not outlive the session.
    util::log_write("info", &format!("shutting down daemon session={name}"));
    let reaped = util::try_reap(child.pid);
    util::kill_pgroup(child.pid, libc::SIGHUP);
    std::thread::sleep(std::time::Duration::from_millis(500));
    util::kill_pgroup(child.pid, libc::SIGKILL);
    let status = reaped.unwrap_or_else(|| util::reap(child.pid));
    util::close_fd(child.master);
    let code = util::exit_code(status);
    // Tell attached clients the real status before hanging up (their EOF
    // is the detach notice). Best-effort: a stuck client cannot block
    // teardown. github #18.
    for c in clients.iter_mut() {
        ipc::append_frame(&mut c.write_buf, Tag::Exit, &ipc::encode_exit(code));
        let _ = util::write_all_retry(c.stream.as_raw_fd(), &c.write_buf, 100);
    }
    clients.clear();
    let _ = std::fs::remove_file(&socket_path);
    std::process::exit(code);
}

#[allow(clippy::too_many_arguments)]
fn daemon_loop(
    listener: &UnixListener,
    child: &PtyChild,
    term: &mut Terminal,
    clients: &mut Vec<ClientConn>,
    info_cmd: &str,
    cwd: &str,
    mut recorder: Option<SessionRecorder>,
) {
    let listener_fd = listener.as_raw_fd();
    let pty_fd = child.master;
    let mut has_pty_output = false;
    // Frame-emission gate (RFC 0008 §6), read once at startup: when off, no
    // client ever gets a `FrameProducer`, so every client stays on `Tag::Output`
    // (today's behavior, byte-for-byte). Default off.
    let frames_gate = session_frames_enabled();
    let mut filter = ScreenSwitchFilter::default();
    let err_events = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
    // t=0 for recording timestamps (only used when recorder.is_some()).
    let rec_start = std::time::Instant::now();

    'daemon: loop {
        if util::take_flag(&util::SIGTERM_RECEIVED) {
            util::log_write("info", "SIGTERM received, shutting down gracefully");
            break;
        }

        // Drop stuck readers before building the pollfd set (so the fd<->client
        // index mapping stays consistent for this iteration). github #11.
        clients.retain(|c| {
            if c.write_buf.len() > MAX_CLIENT_BACKLOG {
                util::log_write(
                    "warn",
                    &format!(
                        "dropping slow client fd={} backlog={}",
                        c.stream.as_raw_fd(),
                        c.write_buf.len()
                    ),
                );
                false
            } else {
                true
            }
        });

        let mut fds = Vec::with_capacity(2 + clients.len());
        fds.push(util::pollfd(listener_fd, libc::POLLIN));
        fds.push(util::pollfd(pty_fd, libc::POLLIN));
        for c in clients.iter() {
            let mut events = libc::POLLIN;
            if !c.write_buf.is_empty() {
                events |= libc::POLLOUT;
            }
            fds.push(util::pollfd(c.stream.as_raw_fd(), events));
        }

        match util::poll(&mut fds, -1) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                util::log_write("error", &format!("poll failed: {e}"));
                break;
            }
        }

        // New client connections.
        if fds[0].revents & err_events != 0 {
            util::log_write("error", "server socket error");
            break;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            if let Ok((stream, _)) = listener.accept() {
                let _ = stream.set_nonblocking(true);
                util::log_write(
                    "info",
                    &format!("client connected fd={}", stream.as_raw_fd()),
                );
                clients.push(ClientConn {
                    stream,
                    read_buf: FrameBuffer::new(),
                    write_buf: Vec::new(),
                    rows: 0,
                    cols: 0,
                    caps: Vec::new(),
                    producer: None,
                });
            }
        }

        // PTY output: feed the terminal model, return any query replies to
        // the application, and broadcast the bytes to all clients — raw,
        // except that screen switches are virtualized (clients pin the
        // outer terminal to its alternate screen for the whole attach).
        if fds[1].revents & (libc::POLLIN | err_events) != 0 {
            let mut buf = [0u8; 4096];
            match util::read_fd(pty_fd, &mut buf) {
                Ok(0) => {
                    util::log_write("info", "shell exited");
                    break;
                }
                Ok(n) => {
                    let mut bcast = Vec::with_capacity(n);
                    filter.feed(term, &buf[..n], &mut bcast);
                    // Record the RAW chunk (what the emulator processed), not
                    // the screen-switch-filtered broadcast — that's what makes
                    // a poshterity replay reproduce this session's screen.
                    if let Some(rec) = recorder.as_mut() {
                        if rec.output(rec_start.elapsed().as_secs_f64(), &buf[..n]).is_err() {
                            recorder = None; // disable on write error; never kill the session
                        }
                    }
                    // The model answers the app's queries (DA/DSR/kitty/...)
                    // only when no real terminal is attached. When clients are
                    // present, their terminals answer and the answers return
                    // as Tag::Input, so the model staying silent avoids a
                    // duplicate (and lets the real terminal's capabilities
                    // win). github #13.
                    let responses = term.take_responses();
                    if !responses.is_empty() && clients.is_empty() {
                        let _ = util::write_all_retry(pty_fd, &responses, 100);
                    }
                    has_pty_output = true;
                    if !bcast.is_empty() {
                        broadcast_output(clients, term, &bcast);
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    // EIO on Linux when the slave side is gone.
                    util::log_write("info", "pty closed");
                    break;
                }
            }
        }

        // Client traffic. Iterate only over the clients present when the
        // pollfd set was built; walk backwards so removal is safe.
        let polled = fds.len() - 2;
        let mut i = clients.len().min(polled);
        while i > 0 {
            i -= 1;
            let revents = fds[i + 2].revents;
            if revents == 0 {
                continue;
            }
            let mut remove = false;
            let mut resized = false;
            let mut needs_replay = false;
            let mut detach_all = false;
            let total_clients = clients.len();
            {
                let c = &mut clients[i];
                if revents & libc::POLLIN != 0 {
                    match c.read_buf.read_from(c.stream.as_raw_fd()) {
                        Ok(0) => remove = true,
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => remove = true,
                    }
                    if !remove {
                        loop {
                            let frame = match c.read_buf.next() {
                                Ok(Some(frame)) => frame,
                                Ok(None) => break,
                                // Oversize/corrupt framing from this peer: drop it.
                                Err(_) => {
                                    remove = true;
                                    break;
                                }
                            };
                            match frame.tag {
                                Tag::Input => {
                                    let _ = util::write_all_retry(pty_fd, &frame.payload, 100);
                                }
                                Tag::Init => {
                                    if c.apply_init(&frame.payload) {
                                        resized = true;
                                    }
                                    // Enable per-client frame production for a
                                    // frame-capable client when the gate is on;
                                    // a no-op otherwise (the replay/broadcast
                                    // then stay on Tag::Output). RFC 0008.
                                    c.maybe_enable_frames(frames_gate);
                                    // Replay the current screen so the client
                                    // sees state it missed (including the first
                                    // attach to a detached-created session). The
                                    // dump is queued after the resize below so
                                    // it reflects the new client size. github #16.
                                    needs_replay = has_pty_output;
                                }
                                Tag::Resize => {
                                    if let Some((r, w)) = ipc::decode_resize(&frame.payload) {
                                        c.rows = r;
                                        c.cols = w;
                                        resized = true;
                                    }
                                }
                                Tag::Detach => {
                                    remove = true;
                                    break;
                                }
                                Tag::DetachAll => {
                                    detach_all = true;
                                    break;
                                }
                                Tag::Kill => break 'daemon,
                                Tag::Info => {
                                    let info = SessionInfo {
                                        clients: (total_clients - 1) as u64,
                                        pid: child.pid,
                                        cmd: info_cmd.to_string(),
                                        cwd: cwd.to_string(),
                                    };
                                    c.queue(Tag::Info, &info.encode());
                                }
                                Tag::History => {
                                    let out = if ipc::decode_history_format(&frame.payload) {
                                        term.dump_vt()
                                    } else {
                                        term.dump_text().into_bytes()
                                    };
                                    c.queue(Tag::History, &out);
                                }
                                Tag::Run => {
                                    let _ = util::write_all_retry(pty_fd, &frame.payload, 1000);
                                    c.queue(Tag::Ack, b"");
                                }
                                // Output, Ack, Exit, and Frame are all
                                // daemon->client only; ignore if received from
                                // a client.
                                Tag::Output | Tag::Ack | Tag::Exit | Tag::Frame => {}
                            }
                        }
                    }
                }
                if !remove && revents & libc::POLLOUT != 0 && !c.write_buf.is_empty() {
                    match c.stream.write(&c.write_buf) {
                        Ok(n) => {
                            c.write_buf.drain(..n);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => remove = true,
                    }
                }
                if revents & err_events != 0 {
                    remove = true;
                }
            }
            if detach_all {
                util::log_write("info", &format!("detach all clients={}", clients.len()));
                clients.clear();
                break;
            }
            if remove {
                let fd = clients[i].stream.as_raw_fd();
                clients.remove(i);
                util::log_write(
                    "info",
                    &format!("client disconnected fd={fd} remaining={}", clients.len()),
                );
                // The smallest client may have left; grow back (zmx issue #8).
                resized = true;
            }
            if resized {
                apply_client_size(clients, pty_fd, term);
                // Record the new effective size (asciinema "COLSxROWS").
                if let Some(rec) = recorder.as_mut() {
                    let t = rec_start.elapsed().as_secs_f64();
                    if rec.resize(t, term.cols(), term.rows()).is_err() {
                        recorder = None;
                    }
                }
            }
            // Replay after the resize so the dump reflects the client's size.
            // Skip if the client was removed this iteration. github #16.
            // Flat dump: the client pinned the outer terminal to its alt
            // screen, so the replay must never switch the outer's buffers
            // (the outer primary belongs to the user's shell). Session
            // scrollback stays reachable via `posh history`.
            if needs_replay && !remove && i < clients.len() {
                // For a frame-capable client the replay IS the producer's first
                // frame: a fresh producer holds only the empty frame-0 base, so
                // `encode_visible` yields a `Full` keyframe — the equivalent of
                // the dump replay. A baseline client keeps the flat `dump_vt`
                // (it pinned the outer terminal to its alt screen, so the replay
                // must never switch buffers). RFC 0008.
                let c = &mut clients[i];
                // Derive the dump/snapshot frame inputs ONLY when a producer
                // exists — exactly the lazy guard `broadcast_output` uses — so a
                // gate-off or non-capable client (the Phase 1 default, hit on
                // every attach) pays only the single `dump_vt_flat` it always did.
                let frame_inputs = c.producer.is_some().then(|| {
                    (
                        term.dump_vt(),
                        Snapshot::from_term(term),
                        term.is_alt_screen(),
                        (term.rows(), term.cols()),
                    )
                });
                let produced = match frame_inputs {
                    Some((dump, snap, alt, dims)) => c.queue_frame(dump, snap, alt, dims),
                    None => false,
                };
                if !produced {
                    c.queue(Tag::Output, &term.dump_vt_flat());
                }
            }
        }
    }

    // Flush the recording's held UTF-8 tail + buffered writer on the way out
    // (shell exit / SIGTERM / kill).
    if let Some(mut rec) = recorder {
        let _ = rec.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_term() -> Terminal {
        Terminal::with_scrollback(5, 20, 100)
    }

    /// Feeds chunks through a fresh filter+model, returning the broadcast.
    fn run_filter(term: &mut Terminal, chunks: &[&[u8]]) -> Vec<u8> {
        let mut filter = ScreenSwitchFilter::default();
        let mut out = Vec::new();
        for chunk in chunks {
            filter.feed(term, chunk, &mut out);
        }
        out
    }

    fn row_text(t: &Terminal, r: u16) -> String {
        t.screen().row(r).unwrap().text(true)
    }

    fn assert_mirrors(session: &Terminal, outer: &Terminal) {
        for r in 0..session.rows() {
            assert_eq!(
                row_text(session, r),
                row_text(outer, r),
                "row {r} diverged"
            );
        }
        assert_eq!(session.cursor().row, outer.cursor().row, "cursor row");
        assert_eq!(session.cursor().col, outer.cursor().col, "cursor col");
    }

    #[test]
    fn passthrough_without_switches_is_byte_identical() {
        let mut term = new_term();
        let input: &[u8] = b"hello \x1b[31mred\x1b[0m\r\n\x1b]2;title\x07done";
        let out = run_filter(&mut term, &[input]);
        assert_eq!(out, input);
    }

    #[test]
    fn fast_path_plain_text_is_byte_identical() {
        let mut term = new_term();
        let input: &[u8] = b"no escapes at all, just text\r\n";
        let out = run_filter(&mut term, &[input]);
        assert_eq!(out, input);
    }

    #[test]
    fn alt_switch_is_excised_and_substituted() {
        let mut term = new_term();
        let out = run_filter(&mut term, &[b"abc\x1b[?1049hdef"]);
        let s = String::from_utf8_lossy(&out);
        assert!(s.starts_with("abc"), "{s:?}");
        assert!(s.ends_with("def"), "{s:?}");
        assert!(!s.contains("\x1b[?1049"), "raw switch leaked: {s:?}");
        assert!(s.contains("\x1b[2J"), "no repaint substitute: {s:?}");
    }

    #[test]
    fn switch_split_across_reads_is_still_excised() {
        let mut term = new_term();
        let out = run_filter(&mut term, &[b"x\x1b[?10", b"49h", b"y"]);
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("\x1b[?1049"), "raw switch leaked: {s:?}");
        assert!(s.starts_with('x') && s.ends_with('y'), "{s:?}");
    }

    #[test]
    fn co_set_modes_survive_the_strip() {
        let mut term = new_term();
        let out = run_filter(&mut term, &[b"\x1b[?1049;2004h"]);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("\x1b[?2004h"), "co-set mode lost: {s:?}");
        assert!(!s.contains("1049"), "{s:?}");
    }

    #[test]
    fn non_switch_private_modes_pass_raw() {
        let mut term = new_term();
        let out = run_filter(&mut term, &[b"\x1b[?2004h\x1b[?1000h\x1b[?1049$p"]);
        assert_eq!(out, b"\x1b[?2004h\x1b[?1000h\x1b[?1049$p");
    }

    #[test]
    fn outer_terminal_mirrors_session_through_a_vim_cycle() {
        // `outer` is the attached client's real terminal: it receives the
        // filtered broadcast and must show the same screen as the session
        // model at every step, without ever switching its own buffers.
        let mut session = new_term();
        let mut outer = new_term();
        let mut filter = ScreenSwitchFilter::default();
        let mut play = |session: &mut Terminal, outer: &mut Terminal, bytes: &[u8]| {
            let mut filter_out = Vec::new();
            filter.feed(session, bytes, &mut filter_out);
            outer.process(&filter_out);
        };
        play(&mut session, &mut outer, b"$ ls\r\nfile.txt\r\n$ vim\x1b[1;7H");
        assert_mirrors(&session, &outer);
        play(
            &mut session,
            &mut outer,
            b"\x1b[?1049h\x1b[2J\x1b[H~ VIM ~\x1b[2;1H\x1b[?2004h",
        );
        assert_mirrors(&session, &outer);
        assert!(session.is_alt_screen());
        assert!(!outer.is_alt_screen(), "outer must never switch buffers");
        play(&mut session, &mut outer, b"\x1b[?2004l\x1b[?1049l");
        assert_mirrors(&session, &outer);
        assert!(!outer.is_alt_screen());
        assert_eq!(row_text(&outer, 0), "$ ls");
        assert_eq!(row_text(&outer, 1), "file.txt");
    }

    #[test]
    fn ris_is_substituted_with_reset_preamble() {
        let mut term = new_term();
        let out = run_filter(&mut term, &[b"junk\x1bcafter"]);
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("\x1bc"), "raw RIS leaked: {s:?}");
        assert!(s.contains("\x1b[!p"), "no soft reset in substitute: {s:?}");
        assert!(s.contains("\x1b[2J"), "no repaint after reset: {s:?}");
        assert!(s.ends_with("after"), "{s:?}");
    }

    fn test_client_conn() -> ClientConn {
        // A connected pair gives the struct a real fd without a daemon; only
        // the parse-side fields (rows/cols/caps) are exercised here.
        let (stream, _peer) = UnixStream::pair().unwrap();
        ClientConn {
            stream,
            read_buf: FrameBuffer::new(),
            write_buf: Vec::new(),
            rows: 0,
            cols: 0,
            caps: Vec::new(),
            producer: None,
        }
    }

    #[test]
    fn init_with_cap_table_records_protocol_version_and_resizes() {
        let mut c = test_client_conn();
        let mut payload = ipc::encode_resize(24, 80).to_vec();
        payload.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));

        let resized = c.apply_init(&payload);

        assert!(resized, "resize prefix must still size the PTY");
        assert_eq!((c.rows, c.cols), (24, 80), "size decoded from the 4-byte prefix");
        assert!(
            caps::find(&c.caps, caps::CAP_PROTOCOL_VERSION).is_some(),
            "PROTOCOL_VERSION must be recorded from the trailing table: {:?}",
            c.caps
        );
    }

    #[test]
    fn bare_init_records_empty_caps_and_resizes() {
        let mut c = test_client_conn();

        let resized = c.apply_init(&ipc::encode_resize(10, 40));

        assert!(resized, "a baseline 4-byte Init still resizes");
        assert_eq!((c.rows, c.cols), (10, 40));
        assert!(c.caps.is_empty(), "no trailing table => no caps");
    }

    #[test]
    fn bare_reinit_preserves_already_negotiated_caps() {
        // SIGCONT resume re-Inits with a bare 4-byte payload; that must not
        // wipe the caps a cap-extended Init negotiated earlier.
        let mut c = test_client_conn();
        let mut first = ipc::encode_resize(24, 80).to_vec();
        first.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        c.apply_init(&first);

        c.apply_init(&ipc::encode_resize(30, 100));

        assert_eq!((c.rows, c.cols), (30, 100), "the re-Init still resizes");
        assert!(
            caps::find(&c.caps, caps::CAP_PROTOCOL_VERSION).is_some(),
            "caps survive a bare re-Init"
        );
    }

    #[test]
    fn strict_decode_resize_rejects_cap_extended_payload() {
        // Why the client re-asserts its size via Tag::Resize after a
        // cap-extended Init: a pre-#100 daemon ran decode_resize on the whole
        // payload, which rejects anything but exactly 4 bytes and would drop
        // the initial size.
        let mut payload = ipc::encode_resize(24, 80).to_vec();
        payload.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        assert!(ipc::decode_resize(&payload).is_none());
    }

    #[test]
    fn strip_alt_screen_params_shapes() {
        assert_eq!(strip_alt_screen_params(b"\x1b[?1049h"), None);
        assert_eq!(strip_alt_screen_params(b"\x1b[?47l"), None);
        // Leading zeros still match numerically.
        assert_eq!(strip_alt_screen_params(b"\x1b[?0047h"), None);
        assert_eq!(
            strip_alt_screen_params(b"\x1b[?1049;2004h").as_deref(),
            Some(b"\x1b[?2004h".as_slice())
        );
        assert_eq!(
            strip_alt_screen_params(b"\x1b[?2004;1049;1000l").as_deref(),
            Some(b"\x1b[?2004;1000l".as_slice())
        );
        // Unexpected shapes are dropped whole (the repaint follows anyway).
        assert_eq!(strip_alt_screen_params(b"\x1b[?10\x0749h"), None);
        assert_eq!(strip_alt_screen_params(b"\x1bc"), None);
    }

    // ---- Task 1.4: per-client frame production (RFC 0008) ----

    use crate::remote::framesync::{ApplyOutcome, DumpDiff, FrameApplier};
    use crate::remote::sync::FrameBody;

    /// A frame-capable client: its `Tag::Init` carries an RFC 0001 cap table, so
    /// with the gate on `maybe_enable_frames` constructs its `FrameProducer`.
    /// The peer end is returned so the socket stays open for the test's lifetime.
    fn frame_capable_conn(rows: u16, cols: u16) -> (ClientConn, UnixStream) {
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut c = ClientConn {
            stream,
            read_buf: FrameBuffer::new(),
            write_buf: Vec::new(),
            rows: 0,
            cols: 0,
            caps: Vec::new(),
            producer: None,
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        c.apply_init(&init);
        c.maybe_enable_frames(true);
        (c, peer)
    }

    /// Fills the screen so a later one-character edit is a clear diff win (a
    /// `Diff`, not a `Full`) — the diff-economics fixture the producer needs.
    fn fill_screen(term: &mut Terminal) {
        term.process(b"\x1b[2J\x1b[H");
        for i in 0..20u8 {
            term.process(format!("line {i:02} of representative session content\r\n").as_bytes());
        }
    }

    /// Decode the `Tag::Frame` `ServerFrame` bodies queued in a client's write
    /// buffer, asserting every queued record is a `Tag::Frame` (no `Tag::Output`
    /// leaked in for a frame-capable client).
    fn decode_frame_bodies(write_buf: &[u8]) -> Vec<FrameBody> {
        let mut fb = FrameBuffer::new();
        fb.feed(write_buf);
        let mut bodies = Vec::new();
        while let Some(frame) = fb.next().unwrap() {
            assert_eq!(frame.tag, Tag::Frame, "frame-capable client must receive Tag::Frame");
            bodies.push(ServerFrame::decode(&frame.payload).unwrap().body);
        }
        bodies
    }

    /// Reconstruct a frame-capable client's view: apply its queued `Tag::Frame`
    /// stream through the `DumpDiff` applier into a scratch `Terminal` and return
    /// the rendered `Snapshot`. This is the real client-side codec, so a passing
    /// equality against the daemon's own `Snapshot` is a genuine round-trip, not
    /// a tautology.
    fn reconstruct(write_buf: &[u8], rows: u16, cols: u16) -> Snapshot {
        let mut fb = FrameBuffer::new();
        fb.feed(write_buf);
        let mut term = Terminal::with_scrollback(rows, cols, 0);
        let mut applier = DumpDiff;
        let mut applied: Vec<u8> = Vec::new();
        while let Some(frame) = fb.next().unwrap() {
            let body = ServerFrame::decode(&frame.payload).unwrap().body;
            match applier.apply(rows, cols, &applied, &mut term, &body) {
                ApplyOutcome::Advanced { dump } => applied = dump,
                ApplyOutcome::AdvancedNoDump | ApplyOutcome::NoChange => {}
                ApplyOutcome::ReackAndWait => panic!("DumpDiff could not apply a queued body"),
            }
        }
        Snapshot::from_term(&term)
    }

    #[test]
    fn frames_gate_defaults_off_and_parses_truthy() {
        // Default OFF: unset/empty/falsey never turn it on.
        assert!(!parse_frames_gate(None));
        assert!(!parse_frames_gate(Some("")));
        assert!(!parse_frames_gate(Some("0")));
        assert!(!parse_frames_gate(Some("off")));
        assert!(!parse_frames_gate(Some("false")));
        // `morph` is the POSH_FRAMESYNC value, NOT this gate — must stay off.
        assert!(!parse_frames_gate(Some("morph")));
        // Truthy spellings (case-insensitive, trimmed) turn it on.
        for on in ["1", "true", "on", "yes", "  TRUE  ", "On"] {
            assert!(parse_frames_gate(Some(on)), "{on:?} should enable the gate");
        }
    }

    #[test]
    fn producer_constructed_only_when_gated_and_capable() {
        // Gate on + capable => producer.
        let (capable, _p) = frame_capable_conn(24, 80);
        assert!(capable.producer.is_some(), "gate on + cap table => producer");

        // Gate off + capable => none (the Phase 1 safety invariant).
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut gate_off = ClientConn {
            stream,
            read_buf: FrameBuffer::new(),
            write_buf: Vec::new(),
            rows: 0,
            cols: 0,
            caps: Vec::new(),
            producer: None,
        };
        let mut init = ipc::encode_resize(24, 80).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        gate_off.apply_init(&init);
        gate_off.maybe_enable_frames(false);
        assert!(gate_off.producer.is_none(), "gate off must not construct a producer");

        // Gate on + NOT capable (bare Init) => none.
        let mut baseline = test_client_conn();
        baseline.apply_init(&ipc::encode_resize(24, 80));
        baseline.maybe_enable_frames(true);
        assert!(baseline.producer.is_none(), "a non-capable client never gets a producer");
    }

    #[test]
    fn maybe_enable_frames_is_idempotent_across_reinit() {
        // A bare re-Init (SIGCONT resume) must NOT rebuild an established
        // producer — that would reset frame numbering to 0 and stale the
        // consumer's acked base. Mirrors the cap-idempotency test.
        let (mut c, _peer) = frame_capable_conn(24, 80);
        // Advance the producer past frame 0 so a reset would be observable.
        assert!(c.queue_frame(b"dump".to_vec(), Snapshot::blank(24, 80), false, (24, 80)));
        let num_before = c.producer.as_ref().unwrap().current_num();
        assert_eq!(num_before, 1, "producing one frame must advance current_num to 1");

        c.maybe_enable_frames(true);

        assert!(c.producer.is_some(), "the producer survives a re-Init");
        assert_eq!(
            c.producer.as_ref().unwrap().current_num(),
            num_before,
            "a re-Init must preserve frame numbering, not reset to a fresh producer"
        );
    }

    #[test]
    fn frame_capable_client_receives_reconstructable_frames() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);

        let (mut c, _peer) = frame_capable_conn(rows, cols);
        assert!(c.producer.is_some());

        // Replay on attach: the producer's first frame is a Full keyframe.
        assert!(c.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            term.is_alt_screen(),
            (rows, cols),
        ));

        // A later visible change broadcasts a frame against the acked base.
        // Append at the cursor (screen bottom) so the long shared prefix makes
        // the prefix/suffix diff a clear win — i.e. a Diff, not a Full.
        term.process(b"appended output");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw bytes ignored>");

        let bodies = decode_frame_bodies(&c.write_buf);
        assert_eq!(bodies.len(), 2, "one replay keyframe + one broadcast frame");
        assert!(
            matches!(bodies[0], FrameBody::Full(_)),
            "fresh attach => Full keyframe, got {:?}",
            bodies[0]
        );
        assert!(
            matches!(bodies[1], FrameBody::Diff { base: 1, .. }),
            "established base => Diff against frame 1, got {:?}",
            bodies[1]
        );

        // The applied frames reconstruct the daemon's screen exactly.
        assert_eq!(
            reconstruct(&c.write_buf, rows, cols),
            Snapshot::from_term(&term),
            "client-applied frames must reproduce the daemon screen"
        );
    }

    #[test]
    fn per_client_producers_diff_against_independent_bases() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);

        // Client A attaches first and gets its Full keyframe (frame 1).
        let (mut a, _pa) = frame_capable_conn(rows, cols);
        assert!(a.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            term.is_alt_screen(),
            (rows, cols),
        ));

        // A visible change (appended at the cursor so A's diff is a clear win);
        // then client B attaches AFTER it. B's first-ever frame is a Full of the
        // NEW screen, while A — in the same broadcast — gets a Diff against its
        // own acked base.
        term.process(b"appended output");
        let (mut b, _pb) = frame_capable_conn(rows, cols);
        assert!(b.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            term.is_alt_screen(),
            (rows, cols),
        ));
        broadcast_output(std::slice::from_mut(&mut a), &term, b"x");

        let a_bodies = decode_frame_bodies(&a.write_buf);
        let b_bodies = decode_frame_bodies(&b.write_buf);
        assert!(matches!(a_bodies[0], FrameBody::Full(_)));
        assert!(
            matches!(a_bodies[1], FrameBody::Diff { base: 1, .. }),
            "A's established producer diffs, got {:?}",
            a_bodies[1]
        );
        assert_eq!(b_bodies.len(), 1, "B has only its replay keyframe");
        assert!(
            matches!(b_bodies[0], FrameBody::Full(_)),
            "B's first-ever frame is a Full regardless of A's state, got {:?}",
            b_bodies[0]
        );

        // Both clients reconstruct the same final screen.
        assert_eq!(reconstruct(&a.write_buf, rows, cols), Snapshot::from_term(&term));
        assert_eq!(reconstruct(&b.write_buf, rows, cols), Snapshot::from_term(&term));
    }

    #[test]
    fn gate_off_emits_output_for_every_client() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 100);
        term.process(b"content");

        // A cap-advertising client, but the gate is OFF => no producer.
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut c = ClientConn {
            stream,
            read_buf: FrameBuffer::new(),
            write_buf: Vec::new(),
            rows: 0,
            cols: 0,
            caps: Vec::new(),
            producer: None,
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        c.apply_init(&init);
        c.maybe_enable_frames(false);
        assert!(c.producer.is_none());

        let raw = b"raw broadcast bytes";
        broadcast_output(std::slice::from_mut(&mut c), &term, raw);

        let mut fb = FrameBuffer::new();
        fb.feed(&c.write_buf);
        let frame = fb.next().unwrap().expect("one queued record");
        assert_eq!(frame.tag, Tag::Output, "gate off => Tag::Output");
        assert_eq!(frame.payload, raw, "gate off => the raw broadcast bytes, unchanged");
        assert!(fb.next().unwrap().is_none(), "exactly one Output record");
    }

    #[test]
    fn non_capable_client_gets_output_even_with_gate_on() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 100);
        term.process(b"content");

        // No cap table in the Init => baseline peer; gate ON.
        let mut c = test_client_conn();
        c.apply_init(&ipc::encode_resize(rows, cols));
        c.maybe_enable_frames(true);
        assert!(c.producer.is_none(), "a non-capable client never gets a producer");

        let raw = b"raw broadcast bytes";
        broadcast_output(std::slice::from_mut(&mut c), &term, raw);

        let mut fb = FrameBuffer::new();
        fb.feed(&c.write_buf);
        let frame = fb.next().unwrap().expect("one queued record");
        assert_eq!(frame.tag, Tag::Output);
        assert_eq!(frame.payload, raw);
    }

    #[test]
    fn mixed_clients_each_get_their_own_transport() {
        // One frame-capable + one baseline client in the same broadcast: the
        // capable one gets Tag::Frame, the baseline one gets the raw Tag::Output
        // — neither regresses the other.
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);

        let (capable, _pc) = frame_capable_conn(rows, cols);
        let mut baseline = test_client_conn();
        baseline.apply_init(&ipc::encode_resize(rows, cols));
        baseline.maybe_enable_frames(true);
        assert!(baseline.producer.is_none());

        let mut clients = vec![capable, baseline];
        let raw = b"raw delta";
        broadcast_output(&mut clients, &term, raw);

        // Capable client => a single Tag::Frame (a Full, since fresh).
        let cap_bodies = decode_frame_bodies(&clients[0].write_buf);
        assert_eq!(cap_bodies.len(), 1);
        assert!(matches!(cap_bodies[0], FrameBody::Full(_)));

        // Baseline client => Tag::Output with the raw bytes.
        let mut fb = FrameBuffer::new();
        fb.feed(&clients[1].write_buf);
        let frame = fb.next().unwrap().expect("one queued record");
        assert_eq!(frame.tag, Tag::Output);
        assert_eq!(frame.payload, raw);
    }

    // ---- Task 1.6: 4-way session-socket version-skew matrix (RFC 0008 §6) ----

    /// Assert a client's whole queued backlog is a single `Tag::Output` record
    /// carrying `expected` verbatim — the baseline (`Tag::Output`) outcome for
    /// every skew cell except new×new.
    fn assert_single_output(write_buf: &[u8], expected: &[u8]) {
        let mut fb = FrameBuffer::new();
        fb.feed(write_buf);
        let frame = fb.next().unwrap().expect("one queued record");
        assert_eq!(frame.tag, Tag::Output, "expected the baseline Tag::Output");
        assert_eq!(frame.payload, expected, "Tag::Output must carry the raw broadcast bytes unchanged");
        assert!(fb.next().unwrap().is_none(), "exactly one queued record");
    }

    /// The four-way socket version-skew matrix of RFC 0008 §6: the daemon's
    /// negotiation degrades cleanly across daemon/client versions without a flag
    /// day. "old daemon" is modelled by the `$POSH_SESSION_FRAMES` gate being
    /// OFF (the daemon's newness — gate off ⇒ it never constructs a producer, so
    /// every client gets raw `Tag::Output`); "old client" by a bare 4-byte Init
    /// with no capability table.
    ///
    /// | daemon (gate) | client (Init)        | screen output |
    /// |---------------|----------------------|---------------|
    /// | new (on)      | new (caps)           | `Tag::Frame`  |
    /// | new (on)      | old (bare)           | `Tag::Output` |
    /// | old (off)     | new (caps + Resize)  | `Tag::Output` |
    /// | old (off)     | old (bare)           | `Tag::Output` |
    #[test]
    fn four_way_socket_version_skew_matrix() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let raw = b"raw screen-output bytes";

        // Cell 1 — new daemon (gate ON) × new client (cap table) ⇒ Tag::Frame.
        // The frame cap is observed, so the daemon negotiates frames and serves
        // the screen as a posh-proto ServerFrame (a Full keyframe on first paint).
        {
            let (mut c, _peer) = frame_capable_conn(rows, cols);
            assert!(c.producer.is_some(), "cell 1: gate on + cap table ⇒ producer");
            broadcast_output(std::slice::from_mut(&mut c), &term, raw);
            let bodies = decode_frame_bodies(&c.write_buf); // also asserts every record is Tag::Frame
            assert_eq!(bodies.len(), 1, "cell 1: one screen-output frame");
            assert!(
                matches!(bodies[0], FrameBody::Full(_)),
                "cell 1: a fresh frame-capable attach ⇒ Full keyframe, got {:?}",
                bodies[0]
            );
        }

        // Cell 2 — new daemon (gate ON) × old client (bare Init) ⇒ Tag::Output.
        // The daemon never observes a frame cap, so even with the gate on it
        // builds no producer and serves the baseline raw dump.
        {
            let mut c = test_client_conn();
            c.apply_init(&ipc::encode_resize(rows, cols));
            c.maybe_enable_frames(true);
            assert!(c.producer.is_none(), "cell 2: no cap table ⇒ no producer even with gate on");
            broadcast_output(std::slice::from_mut(&mut c), &term, raw);
            assert_single_output(&c.write_buf, raw);
        }

        // Cell 3 (the critical cross-version cell) — old daemon (gate OFF) × new
        // client (cap-extended Init + the Tag::Resize re-assertion) ⇒ Tag::Output,
        // AND the size the new client conveys is recoverable on an old daemon.
        {
            let mut c = test_client_conn();
            let cap_extended_init = {
                let mut init = ipc::encode_resize(rows, cols).to_vec();
                init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
                init
            };
            c.apply_init(&cap_extended_init);
            c.maybe_enable_frames(false); // gate OFF ⇒ "old daemon" ⇒ no frames
            assert!(c.producer.is_none(), "cell 3: gate off ⇒ no producer regardless of caps");

            broadcast_output(std::slice::from_mut(&mut c), &term, raw);
            assert_single_output(&c.write_buf, raw);

            // The cross-version size property, pinned on the REAL decoder applied
            // to the GENUINE payloads (not a field write-then-read tautology):
            //
            // (1) An OLD daemon decodes resize from the WHOLE Init payload and
            // rejects any non-4-byte length, so the cap-extended Init's size is
            // dropped on its floor — which is precisely why the new client must
            // re-assert via Tag::Resize.
            assert!(
                ipc::decode_resize(&cap_extended_init).is_none(),
                "cell 3: an old daemon's strict whole-payload decode must drop the cap-extended Init's size"
            );
            // (2) The 4-byte Tag::Resize the new client re-asserts after the Init
            // decodes to the right dims — every daemon version honors Tag::Resize,
            // so even an old daemon that dropped the Init size recovers it here.
            let resize_payload = ipc::encode_resize(rows, cols);
            assert_eq!(
                ipc::decode_resize(&resize_payload),
                Some((rows, cols)),
                "cell 3: the client's Tag::Resize re-assertion must carry the recoverable size"
            );
        }

        // Cell 4 — old daemon (gate OFF) × old client (bare Init) ⇒ Tag::Output.
        // The unchanged baseline: neither side negotiates anything new.
        {
            let mut c = test_client_conn();
            c.apply_init(&ipc::encode_resize(rows, cols));
            c.maybe_enable_frames(false);
            assert!(c.producer.is_none(), "cell 4: gate off + no caps ⇒ no producer");
            broadcast_output(std::slice::from_mut(&mut c), &term, raw);
            assert_single_output(&c.write_buf, raw);
        }
    }
}
