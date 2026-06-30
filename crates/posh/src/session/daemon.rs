//! Per-session daemon: owns the PTY and broadcasts output to attached
//! clients over a Unix socket (zmx daemonLoop port).

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

use posh_term::{ScreenSwitch, Terminal};

use crate::pty::{self, PtyChild};
use crate::remote::caps;
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
    // github #100). Recorded here for the framesync negotiation; this task
    // only stores them — output stays `Tag::Output` for every client until
    // frame emission is gated on caps in a later task, which is when this
    // field gains its first production reader (hence the allow).
    #[allow(dead_code)]
    caps: Vec<caps::Cap>,
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
                        for c in clients.iter_mut() {
                            c.queue(Tag::Output, &bcast);
                        }
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
                let dump = term.dump_vt_flat();
                clients[i].queue(Tag::Output, &dump);
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
}
