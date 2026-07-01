//! Attach client: raw-mode tty bridged to a session daemon over the Unix
//! socket (zmx clientLoop port). Detach key: Ctrl-\.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use posh_term::Terminal;

use crate::pty::{self, RawMode};
use crate::remote::caps;
use crate::remote::display::{self, Snapshot};
use crate::remote::framesync::{ApplyOutcome, FrameApplier, FrameSync};
use crate::remote::sync::ServerFrame;
use crate::session::ipc::{self, FrameBuffer, Tag};
use crate::session::{daemon, Config};
use crate::util::{self, Error, Result};

const STDIN: i32 = libc::STDIN_FILENO;
const STDOUT: i32 = libc::STDOUT_FILENO;

/// Mode resets written on detach before leaving the alternate screen:
/// mouse reporting (1000/1002/1003/1006), alternate scroll (1007),
/// bracketed paste (2004), focus events (1004), and the pen.
const MODES_OFF_SEQ: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1007l\x1b[?2004l\x1b[?1004l\x1b[0m";

/// Takeover sequence written on attach (and SIGCONT resume): terminfo
/// smcup for $TERM puts the whole attach on the outer terminal's
/// alternate screen, so detach can put the user's shell back exactly as
/// it was; the explicit clear covers terminals whose alt buffer isn't
/// cleared on entry and gives the replay its clean slate either way. The
/// daemon virtualizes the inner application's own switches so the outer
/// terminal never leaves this screen mid-attach. Under --no-init (or a
/// terminfo entry with no alternate screen) the bracket is empty and this
/// degrades to the historical clear-in-place behavior.
fn enter_seq(bracket: &Option<(Vec<u8>, Vec<u8>)>) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some((smcup, _)) = bracket {
        out.extend_from_slice(smcup);
    }
    out.extend_from_slice(b"\x1b[2J\x1b[H");
    out
}

/// Restore sequence written on the way out: mode resets, terminfo rmcup
/// (restoring the user's pre-attach screen and cursor), re-show the
/// cursor.
fn restore_seq(bracket: &Option<(Vec<u8>, Vec<u8>)>) -> Vec<u8> {
    let mut out = Vec::from(MODES_OFF_SEQ);
    if let Some((_, rmcup)) = bracket {
        out.extend_from_slice(rmcup);
    }
    out.extend_from_slice(b"\x1b[?25h");
    out
}

pub fn cmd_attach(
    cfg: &Config,
    name: &str,
    command: Option<Vec<String>>,
    detach_flag: bool,
) -> Result<()> {
    if !detach_flag && std::env::var_os("POSH_SESSION").is_some() {
        return Err(Error::from(
            "cannot attach to a session from within a session",
        ));
    }

    let created = daemon::ensure_session(cfg, name, command)?;
    if detach_flag {
        if created {
            println!("session \"{name}\" created");
        } else {
            println!("session \"{name}\" already exists");
        }
        return Ok(());
    }

    let path = cfg.socket_path(name)?;
    let stream = UnixStream::connect(&path)
        .map_err(|e| Error(format!("connect {}: {e}", path.display())))?;

    // Handlers go in before raw mode and the takeover write: the first
    // byte on the tty is the outside world's readiness signal, and a
    // SIGTERM racing it must find the handler installed, not the default
    // disposition (github #49, the attach sibling of #48).
    util::install_client_signal_handlers();
    let raw = RawMode::enable(STDIN)?;
    // Take over the alternate screen before the daemon replays the
    // session state; the user's shell screen waits underneath.
    let bracket = crate::terminfo::ca_mode_bracket();
    let enter = enter_seq(&bracket);
    let _ = util::write_fd(STDOUT, &enter);
    let result = client_loop(stream, &enter);
    let _ = util::write_fd(STDOUT, &restore_seq(&bracket));
    drop(raw);
    // When the session ended (rather than detached), carry the shell's
    // exit status out as our own. github #18.
    match result {
        Ok(code) if code != 0 => std::process::exit(code),
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}

/// The detach key Ctrl-\ as kitty keyboard CSI-u encodings (92 = backslash,
/// 5 = ctrl modifier; with and without the explicit `:1` press-event suffix).
const KITTY_DETACH_SEQS: [&[u8]; 2] = [b"\x1b[92;5u", b"\x1b[92;5:1u"];

enum KittyMatch {
    /// The slice begins with a full detach sequence.
    Full,
    /// The slice is a proper prefix of a detach sequence (need more bytes).
    Partial,
    /// No detach sequence starts here.
    No,
}

fn match_kitty_detach(s: &[u8]) -> KittyMatch {
    let mut partial = false;
    for seq in KITTY_DETACH_SEQS {
        if s.len() >= seq.len() {
            if &s[..seq.len()] == seq {
                return KittyMatch::Full;
            }
        } else if seq.starts_with(s) {
            partial = true;
        }
    }
    if partial {
        KittyMatch::Partial
    } else {
        KittyMatch::No
    }
}

/// Scans the stdin byte stream for the detach key — raw Ctrl-\ (0x1c) at any
/// offset, or its kitty CSI-u encodings — surviving splits across reads by
/// holding back a trailing partial that could still complete the sequence.
#[derive(Default)]
struct DetachMatcher {
    carry: Vec<u8>,
}

impl DetachMatcher {
    /// Returns the bytes to forward to the daemon as input, and whether the
    /// detach key was seen (in which case bytes after it are discarded).
    fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(input);
        let mut forward = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let b = data[i];
            if b == 0x1c {
                return (forward, true);
            }
            if b == 0x1b {
                match match_kitty_detach(&data[i..]) {
                    KittyMatch::Full => return (forward, true),
                    KittyMatch::Partial => {
                        // Hold back; the rest may arrive on the next read.
                        self.carry = data[i..].to_vec();
                        return (forward, false);
                    }
                    KittyMatch::No => {}
                }
            }
            forward.push(b);
            i += 1;
        }
        (forward, false)
    }
}

/// Client-side consumer of the daemon's posh-proto `ServerFrame` stream
/// (RFC 0008 / FDR 0011): a minimal, reliable-socket mirror of the remote
/// client's apply+compose core. It holds a client-side terminal model, applies
/// each received `FrameBody` through the DumpDiff applier — the only codec the
/// local client negotiates in Phase 1 — and renders the resulting screen as a
/// display diff against what the tty last showed. No resync/base-sum,
/// prediction, palette, or scrollback: those are later unification tasks.
///
/// This path runs only when the daemon sends `Tag::Frame`
/// (`POSH_SESSION_FRAMES=on`, negotiated by Task 1.4). A default gate-off
/// session serves raw `Tag::Output` and never constructs a `FrameRenderer`, so
/// its byte stream is unchanged.
struct FrameRenderer {
    /// The client's mirror of the daemon terminal. DumpDiff rebuilds it from a
    /// fresh model on every apply, so it is effectively write-then-read here;
    /// a persistent model is nonetheless the seam a later in-place codec
    /// (Morph/scrollback) would advance, so it is held rather than local.
    server_term: Terminal,
    applier: Box<dyn FrameApplier>,
    /// The last applied frame's `dump_vt` bytes — the byte-diff base a `Diff`
    /// reconstructs against.
    applied_data: Vec<u8>,
    /// The frame number the model currently reflects.
    applied_num: u64,
    /// What the tty last showed, so the renderer emits only the delta.
    last_drawn: Snapshot,
    /// False until the first render (and after an external clear), so the next
    /// frame clears + fully repaints — this is what lets a leading raw
    /// `Tag::Output` be overwritten by the first `Full` keyframe, and a `Diff`
    /// land cleanly after a SIGCONT re-enter.
    initialized: bool,
    rows: u16,
    cols: u16,
}

impl FrameRenderer {
    fn new(rows: u16, cols: u16) -> FrameRenderer {
        let rows = rows.max(1);
        let cols = cols.max(1);
        FrameRenderer {
            server_term: Terminal::with_scrollback(rows, cols, 0),
            // DumpDiff: the daemon encodes visible frames with DumpDiff in
            // Phase 1 (Task 1.4) and the client advertises only
            // PROTOCOL_VERSION over the socket (never CAP_MORPH), so the
            // matching applier is DumpDiff. Selected through `FrameSync` so the
            // codec choice lives in one place.
            applier: FrameSync::DumpDiff.applier(),
            applied_data: Vec::new(),
            applied_num: 0,
            last_drawn: Snapshot::blank(rows, cols),
            initialized: false,
            rows,
            cols,
        }
    }

    /// Track a tty resize (SIGWINCH): the daemon re-encodes at the new size, so
    /// the model must apply subsequent frames at the same dimensions (DumpDiff's
    /// reparse clamps to these). Forces the next frame to fully repaint at the
    /// new size.
    fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows.max(1);
        self.cols = cols.max(1);
        self.initialized = false;
    }

    /// Force the next rendered frame to clear + fully repaint — used after the
    /// tty was externally cleared (SIGCONT re-enter) so a subsequent `Diff`
    /// (which carries only the delta) still lands on a known-blank screen.
    fn invalidate(&mut self) {
        self.initialized = false;
    }

    /// Decode, apply, and render one `Tag::Frame` payload, returning the escape
    /// stream to write to the tty (empty when the frame produced no visible
    /// change). Over the reliable Unix socket a frame always applies against the
    /// last one, so an inability to apply (`ReackAndWait`) is a genuine protocol
    /// bug — surfaced as an error rather than silently retried, since the local
    /// client has no resync/keyframe-request path yet (a later task).
    fn render_frame(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let frame = ServerFrame::decode(payload)?;
        match self.applier.apply(
            self.rows,
            self.cols,
            &self.applied_data,
            &mut self.server_term,
            &frame.body,
        ) {
            ApplyOutcome::Advanced { dump } => {
                self.applied_data = dump;
                self.applied_num = frame.frame_num;
            }
            ApplyOutcome::AdvancedNoDump => {
                // The applier advanced the model in place without re-dumping it
                // (MorphDelta): `applied_data` intentionally stays at the last
                // Full/Diff dump, since a Morph session emits no Diff body that
                // would read it. DumpDiff never returns this in Phase 1, but
                // handle it generically so a later codec swap is inert here.
                self.applied_num = frame.frame_num;
            }
            ApplyOutcome::NoChange => return Ok(Vec::new()),
            ApplyOutcome::ReackAndWait => {
                return Err(Error(format!(
                    "session frame {} could not be applied on the reliable socket",
                    frame.frame_num
                )));
            }
        }
        // The minimal compose: non-scroll, non-prediction, non-palette. `false`
        // = no outer wheel reporting; `true` = the scroll-shortcut optimization
        // on (new_frame's default). The remote client's compose_frame adds the
        // prediction/notification/palette overlays on top of exactly this.
        let next = Snapshot::from_term(&self.server_term);
        let bytes = display::new_frame_opt(self.initialized, &self.last_drawn, &next, false, true);
        self.last_drawn = next;
        self.initialized = true;
        Ok(bytes)
    }
}

/// Bridges the tty to the daemon until detach or session end. Returns the
/// session shell's exit status (0 on detach or connection loss).
/// `enter` is re-written on SIGCONT, when the outer terminal may have
/// left our alternate screen while we were stopped.
fn client_loop(stream: UnixStream, enter: &[u8]) -> Result<i32> {
    stream.set_nonblocking(true)?;
    let sock_fd = stream.as_raw_fd();
    util::set_nonblocking(STDIN)?;

    let mut sock_write_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut stdout_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut read_buf = FrameBuffer::new();
    let mut detach = DetachMatcher::default();
    let mut stream_writer = &stream;

    // Announce our terminal size so the daemon can size the PTY, and append
    // our capability table (RFC 0001) so a frame-capable daemon can negotiate
    // the framesync transport (github #100). The Init payload is the 4-byte
    // resize prefix followed by the encoded table.
    let (rows, cols) = pty::term_size(STDOUT);
    let mut init_payload = ipc::encode_resize(rows, cols).to_vec();
    init_payload.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
    ipc::append_frame(&mut sock_write_buf, Tag::Init, &init_payload);
    // Re-assert the size via Tag::Resize: a pre-#100 daemon runs the strict
    // decode_resize over the whole Init payload, so the cap-extended Init's
    // length != 4 makes it drop the initial size. Every daemon version
    // handles Tag::Resize, so this re-assertion guarantees the size lands; on
    // a new daemon it merely re-sets the value Init already carried.
    ipc::append_frame(
        &mut sock_write_buf,
        Tag::Resize,
        &ipc::encode_resize(rows, cols),
    );

    // Consumer of the daemon's `Tag::Frame` stream (RFC 0008): stays None —
    // fully inert — until the first frame arrives, so a default gate-off session
    // (`Tag::Output` only) constructs nothing and behaves exactly as today.
    // `frame_size` seeds a lazily-built renderer at the current tty size and
    // tracks SIGWINCH so it stays correct if frames begin after a resize.
    let mut frame_renderer: Option<FrameRenderer> = None;
    let mut frame_size = (rows, cols);

    let err_events = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
    loop {
        if util::take_flag(&util::SIGWINCH_RECEIVED) {
            let (rows, cols) = pty::term_size(STDOUT);
            frame_size = (rows, cols);
            if let Some(fr) = frame_renderer.as_mut() {
                fr.resize(rows, cols);
            }
            ipc::append_frame(
                &mut sock_write_buf,
                Tag::Resize,
                &ipc::encode_resize(rows, cols),
            );
        }

        if util::take_flag(&util::SIGTERM_RECEIVED) {
            // SIGTERM/SIGINT/SIGHUP: best-effort detach notice, then leave;
            // cmd_attach restores the tty on the way out either way.
            ipc::append_frame(&mut sock_write_buf, Tag::Detach, b"");
            let _ = util::write_all_retry(sock_fd, &sock_write_buf, 100);
            return Ok(0);
        }

        if util::take_flag(&util::SIGCONT_RECEIVED) {
            // Resumed after SIGSTOP/fg: the outer terminal may have left
            // our alternate screen while we were stopped, so re-enter it,
            // then re-Init so the daemon replays the screen (and picks up
            // any size change while stopped).
            let _ = util::write_fd(STDOUT, enter);
            // `enter` just cleared the tty; force the next frame to fully
            // repaint over the blank screen so a replay `Diff` isn't lost.
            if let Some(fr) = frame_renderer.as_mut() {
                fr.invalidate();
            }
            let (rows, cols) = pty::term_size(STDOUT);
            // Bare 4-byte Init: caps are session-persistent (the daemon
            // preserves them across bare re-Inits), and an exact-4-byte resize
            // is accepted by every daemon version, so no cap table or follow-up
            // Resize is needed here.
            ipc::append_frame(
                &mut sock_write_buf,
                Tag::Init,
                &ipc::encode_resize(rows, cols),
            );
        }

        let mut fds = vec![util::pollfd(STDIN, libc::POLLIN)];
        let mut sock_events = libc::POLLIN;
        if !sock_write_buf.is_empty() {
            sock_events |= libc::POLLOUT;
        }
        fds.push(util::pollfd(sock_fd, sock_events));
        if !stdout_buf.is_empty() {
            fds.push(util::pollfd(STDOUT, libc::POLLOUT));
        }

        // Bounded timeout: a signal landing between the flag checks above
        // and this poll sets the flag without an EINTR; an infinite poll
        // would then sit raw-mode until unrelated activity. One wakeup per
        // second bounds that race (the remote loop does the same).
        match util::poll(&mut fds, 1000) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }

        // stdin -> daemon
        if fds[0].revents & (libc::POLLIN | err_events) != 0 {
            let mut buf = [0u8; 4096];
            match util::read_fd(STDIN, &mut buf) {
                Ok(0) => return Ok(0),
                Ok(n) => {
                    let (forward, detached) = detach.feed(&buf[..n]);
                    if !forward.is_empty() {
                        ipc::append_frame(&mut sock_write_buf, Tag::Input, &forward);
                    }
                    if detached {
                        ipc::append_frame(&mut sock_write_buf, Tag::Detach, b"");
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }

        // daemon -> stdout
        if fds[1].revents & libc::POLLIN != 0 {
            match read_buf.read_from(sock_fd) {
                Ok(0) => return Ok(0),
                Ok(_) => loop {
                    match read_buf.next() {
                        Ok(Some(frame)) => match frame.tag {
                            Tag::Output if !frame.payload.is_empty() => {
                                stdout_buf.extend_from_slice(&frame.payload);
                            }
                            // Frame transport (RFC 0008), only when the daemon
                            // negotiated it (`POSH_SESSION_FRAMES=on`): apply the
                            // ServerFrame into the client model and append the
                            // rendered delta. Build the consumer lazily on the
                            // first frame so the gate-off path stays inert.
                            Tag::Frame => {
                                let fr = frame_renderer.get_or_insert_with(|| {
                                    FrameRenderer::new(frame_size.0, frame_size.1)
                                });
                                let bytes = fr.render_frame(&frame.payload)?;
                                stdout_buf.extend_from_slice(&bytes);
                            }
                            Tag::Exit => {
                                // Session over: flush the final output and
                                // carry the shell's status out.
                                if !stdout_buf.is_empty() {
                                    let _ = util::write_all_retry(STDOUT, &stdout_buf, 1000);
                                }
                                return Ok(ipc::decode_exit(&frame.payload).unwrap_or(0));
                            }
                            _ => {}
                        },
                        Ok(None) => break,
                        Err(e) => return Err(e),
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionReset
                        || e.kind() == std::io::ErrorKind::BrokenPipe =>
                {
                    return Ok(0);
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Flush buffered writes toward the daemon.
        if fds[1].revents & libc::POLLOUT != 0 && !sock_write_buf.is_empty() {
            match stream_writer.write(&sock_write_buf) {
                Ok(n) => {
                    sock_write_buf.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionReset
                        || e.kind() == std::io::ErrorKind::BrokenPipe =>
                {
                    return Ok(0);
                }
                Err(e) => return Err(e.into()),
            }
        }

        if !stdout_buf.is_empty() {
            match util::write_fd(STDOUT, &stdout_buf) {
                Ok(n) => {
                    stdout_buf.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }

        if fds[1].revents & err_events != 0 {
            return Ok(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takeover_sequences_wrap_the_bracket() {
        let bracket = Some((b"\x1b[?1049h".to_vec(), b"\x1b[?1049l".to_vec()));
        assert_eq!(enter_seq(&bracket), b"\x1b[?1049h\x1b[2J\x1b[H");
        let restore = restore_seq(&bracket);
        assert!(restore.starts_with(MODES_OFF_SEQ));
        assert!(restore.ends_with(b"\x1b[?1049l\x1b[?25h"));
        // --no-init / no-alt-screen terminal: historical clear-in-place,
        // mode resets still run.
        assert_eq!(enter_seq(&None), b"\x1b[2J\x1b[H");
        let restore = restore_seq(&None);
        assert!(restore.starts_with(MODES_OFF_SEQ));
        assert!(restore.ends_with(b"\x1b[?25h"));
        assert!(!restore.windows(4).any(|w| w == b"1049"));
    }

    #[test]
    fn raw_ctrl_backslash_detaches() {
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"\x1c"), (vec![], true));
    }

    #[test]
    fn bytes_before_detach_are_forwarded() {
        // Ctrl-\ mid-buffer: the preceding keystrokes must still reach the app
        // (the old matcher dropped the whole buffer). github #17.
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"abc\x1c"), (b"abc".to_vec(), true));
    }

    #[test]
    fn plain_input_passes_through() {
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"hello"), (b"hello".to_vec(), false));
    }

    #[test]
    fn kitty_detach_in_one_read() {
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"\x1b[92;5u"), (vec![], true));
        let mut m2 = DetachMatcher::default();
        assert_eq!(m2.feed(b"\x1b[92;5:1u"), (vec![], true));
    }

    #[test]
    fn kitty_detach_split_across_reads() {
        // The 7-byte CSI-u sequence arriving in two reads must still detach
        // (the old substring scan missed this). github #17.
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"\x1b[92"), (vec![], false)); // held back as partial
        assert_eq!(m.feed(b";5u"), (vec![], true));
    }

    #[test]
    fn non_detach_kitty_key_is_forwarded_after_split() {
        // A different CSI-u key sharing the `\x1b[9` prefix must be delivered,
        // not swallowed, once disambiguated on the next read.
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"\x1b[9"), (vec![], false));
        let (fwd, detached) = m.feed(b"7;5u");
        assert!(!detached);
        assert_eq!(fwd, b"\x1b[97;5u");
    }

    // ---- Task 2.1: local client renders posh-proto ServerFrames (RFC 0008) ----

    use crate::remote::framesync::FrameProducer;
    use crate::remote::sync::FrameBody;

    /// Encode one visible frame the way the session daemon's `queue_frame` does
    /// (DumpDiff, reliable-socket immediate self-ack), returning the `Tag::Frame`
    /// `ServerFrame` payload bytes the client would receive off the socket.
    fn daemon_frame(producer: &mut FrameProducer, term: &Terminal) -> Vec<u8> {
        producer.advance_visible(
            term.dump_vt(),
            Snapshot::from_term(term),
            term.is_alt_screen(),
            (term.rows(), term.cols()),
            0,
        );
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

    /// Fill a screen so a later small edit diffs as a clear win (a `Diff`, not a
    /// `Full`) — mirrors the daemon-side frame fixture. SGR is varied per line
    /// (bold+colour on even rows) so the per-cell style check in
    /// `assert_screens_match` has real signal: a scrambled-attribute regression
    /// would leave the text identical but the pens wrong.
    fn fill_screen(term: &mut Terminal) {
        term.process(b"\x1b[2J\x1b[H");
        for i in 0..20u8 {
            let line = if i % 2 == 0 {
                format!("\x1b[1;32mline {i:02} bold green session content\x1b[0m\r\n")
            } else {
                format!("line {i:02} plain session content\r\n")
            };
            term.process(line.as_bytes());
        }
    }

    /// Assert two terminals show the same visible grid: per-row text, per-cell
    /// SGR pen for every glyph-bearing cell, and cursor position. The style
    /// check is what catches an apply→render regression that scrambled
    /// colours/attributes while leaving the text intact. Blank cells' pens are
    /// deliberately skipped — the escape stream does not round-trip the pen of
    /// empty trailing cells — as are the Snapshot fields (bell/clipboard) that
    /// never travel through the rendered escapes.
    fn assert_screens_match(rendered: &Terminal, expected: &Terminal) {
        let (rs, es) = (rendered.screen(), expected.screen());
        for r in 0..expected.rows() {
            let (rrow, erow) = (rs.row(r).unwrap(), es.row(r).unwrap());
            assert_eq!(rrow.text(true), erow.text(true), "row {r} text diverged");
            for (c, (rc, ec)) in rrow.cells().iter().zip(erow.cells()).enumerate() {
                if ec.ch != ' ' && ec.ch != '\0' {
                    assert_eq!(rc.style, ec.style, "row {r} col {c} style diverged");
                }
            }
        }
        assert_eq!(rendered.cursor().row, expected.cursor().row, "cursor row");
        assert_eq!(rendered.cursor().col, expected.cursor().col, "cursor col");
    }

    /// The core Task 2.1 property: the local client's `FrameRenderer` consumes
    /// the daemon's `Tag::Frame` stream and reproduces the daemon screen — i.e.
    /// the same screen the raw `Tag::Output` path (a `dump_vt` replay of that
    /// daemon screen) would have produced, since `Snapshot::from_term(&daemon)`
    /// IS what raw output renders. Checked at two levels: the applier's model
    /// equals the daemon screen (apply correctness), and an outer terminal fed
    /// the RENDERED escape stream shows the same grid + cursor (render
    /// correctness), across a `Full` keyframe then a `Diff`.
    #[test]
    fn frame_renderer_reproduces_the_daemon_screen() {
        let (rows, cols) = (24u16, 80u16);
        let mut daemon = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut daemon);

        let mut producer = FrameProducer::new(rows, cols);
        let mut renderer = FrameRenderer::new(rows, cols);
        // The outer tty: a fresh model fed ONLY the client's rendered bytes,
        // i.e. exactly what the user's terminal would display.
        let mut outer = Terminal::with_scrollback(rows, cols, 0);

        let play = |renderer: &mut FrameRenderer, outer: &mut Terminal, payload: &[u8]| {
            let bytes = renderer
                .render_frame(payload)
                .expect("a frame must apply on the reliable socket");
            outer.process(&bytes);
            if outer.rows() != rows || outer.cols() != cols {
                outer.resize(rows, cols);
            }
        };

        // Frame 1: the replay-on-attach Full keyframe (nothing acked but the
        // empty frame-0 base, so a DumpDiff against it is never a win).
        let f1 = daemon_frame(&mut producer, &daemon);
        assert!(
            matches!(ServerFrame::decode(&f1).unwrap().body, FrameBody::Full(_)),
            "a fresh attach's first frame must be a Full keyframe"
        );
        play(&mut renderer, &mut outer, &f1);
        assert_eq!(renderer.applied_num, 1, "the keyframe advances applied_num to 1");
        assert_eq!(
            Snapshot::from_term(&renderer.server_term),
            Snapshot::from_term(&daemon),
            "the applier model must equal the daemon screen after the keyframe"
        );
        assert_screens_match(&outer, &daemon);

        // A visible edit at the cursor => a Diff against the acked base (frame 1).
        daemon.process(b"appended output");
        let f2 = daemon_frame(&mut producer, &daemon);
        assert!(
            matches!(
                ServerFrame::decode(&f2).unwrap().body,
                FrameBody::Diff { base: 1, .. }
            ),
            "an established base must yield a Diff against frame 1"
        );
        play(&mut renderer, &mut outer, &f2);
        assert_eq!(renderer.applied_num, 2, "the Diff advances applied_num to 2");
        assert_eq!(
            Snapshot::from_term(&renderer.server_term),
            Snapshot::from_term(&daemon),
            "the applier model must track the daemon screen across a Diff"
        );
        assert_screens_match(&outer, &daemon);
    }

    /// The first `Full` keyframe overwrites any leading raw `Tag::Output` (#17):
    /// before frames begin the daemon may emit a little raw output, which the
    /// client writes verbatim; the first frame is a `Full` applied with
    /// `initialized == false`, so the render clears + fully repaints, erasing the
    /// stale raw bytes. Here the outer terminal is pre-seeded with leftover
    /// output; after the keyframe it must match the daemon exactly.
    #[test]
    fn first_full_keyframe_overwrites_leading_raw_output() {
        let (rows, cols) = (24u16, 80u16);
        let mut daemon = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut daemon);

        let mut producer = FrameProducer::new(rows, cols);
        let mut renderer = FrameRenderer::new(rows, cols);

        // Stale leading raw output on the tty (the pre-frame `Tag::Output`).
        let mut outer = Terminal::with_scrollback(rows, cols, 0);
        outer.process(b"stale leading raw output line\r\nand another\r\n");

        let f1 = daemon_frame(&mut producer, &daemon);
        let bytes = renderer.render_frame(&f1).expect("keyframe applies");
        outer.process(&bytes);
        if outer.rows() != rows || outer.cols() != cols {
            outer.resize(rows, cols);
        }
        assert_screens_match(&outer, &daemon);
    }
}
