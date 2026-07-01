//! Attach client: raw-mode tty bridged to a session daemon over the Unix
//! socket (zmx clientLoop port). Detach key: Ctrl-\.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use posh_term::Terminal;
use serde_json::{json, Value};

use crate::pty::{self, RawMode};
use crate::remote::caps;
use crate::remote::display::{self, Snapshot};
use crate::remote::framesync::{ApplyOutcome, FrameApplier, FrameSync};
use crate::remote::palette::{composite_palette, Palette, PaletteEvent};
use crate::remote::scrollview;
use crate::remote::sync::{FrameBody, ScrollbackRing, ServerFrame};
use crate::session::ipc::{self, FrameBuffer, Tag};
use crate::session::{daemon, Config};
use crate::util::{self, Error, Result};

const STDIN: i32 = libc::STDIN_FILENO;
const STDOUT: i32 = libc::STDOUT_FILENO;

/// The command-palette summon key, Ctrl-^ (mirrors `remote::client::ESCAPE_KEY`).
/// On the frames-on local path a lone `0x1e` opens the palette; gate-off it is an
/// ordinary byte forwarded to the shell.
const ESCAPE_KEY: u8 = 0x1e;

/// Depth of the local client's scrollback ring (RFC 0002 §3), in rows. Matches
/// the session daemon's primary ring (`daemon::SCROLLBACK`, 10_000) — the client
/// advertises `CAP_SCROLLBACK` with a `0` payload (server-default depth), so it
/// mirrors that here — and bounds client memory. The captured rows are rendered
/// into a scrollable local viewport by the shared `remote::scrollview` machinery
/// when the wheel scrolls up (Task 2.5b).
const SCROLLBACK_RING_DEPTH: usize = 10_000;

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
    let result = client_loop(stream, &enter, &raw);
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
/// display diff against what the tty last showed. It also captures scrolled-off
/// rows into a local ring and renders a frozen scroll-view when the wheel
/// scrolls up (Task 2.5b), sharing the `remote::scrollview` machinery with the
/// roaming client. No resync/base-sum, prediction, or palette: those remain
/// later unification tasks.
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
    /// Local, partial, monotonically-growing view of the daemon's scrolled-off
    /// primary rows (RFC 0002 §3). `FrameBody::Scrollback` frames append here
    /// without touching the visible model; a width resize clears it (§4). The
    /// scroll-view (below) renders a window of it when `scroll_offset > 0`.
    scrollback: ScrollbackRing,
    /// How far up the captured history the view sits (FDR 0005), in logical
    /// rows; 0 = the live bottom (normal render). Driven by the wheel through
    /// `mouse_filter`; any keystroke returns it to 0.
    scroll_offset: usize,
    /// Scroll-view render memo (`remote::scrollview`): skips a repaint while the
    /// offset, ring length, and server generation are all unchanged.
    last_scroll_state: scrollview::ScrollMemo,
    /// Intercepts the outer terminal's wheel (SGR mouse) so it drives the local
    /// scroll-view instead of reaching the daemon. Persists across reads so a
    /// sequence split across `read()`s reassembles (posh#52). Shared with the
    /// roaming client via `remote::scrollview`.
    mouse_filter: scrollview::MouseFilter,
    /// What the tty last showed, so the renderer emits only the delta.
    last_drawn: Snapshot,
    /// False until the first render (and after an external clear), so the next
    /// frame clears + fully repaints — this is what lets a leading raw
    /// `Tag::Output` be overwritten by the first `Full` keyframe, and a `Diff`
    /// land cleanly after a SIGCONT re-enter.
    initialized: bool,
    /// The wheel intent of the last live compose, so the shared renderer can
    /// tear the wheel-grab down (or re-arm it) on a want_wheel transition that
    /// isn't also a mouse_mode change — e.g. an app entering the alt-screen
    /// without a mouse mode (github #106).
    last_wheel: bool,
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
            scrollback: ScrollbackRing::new(SCROLLBACK_RING_DEPTH),
            scroll_offset: 0,
            last_scroll_state: None,
            mouse_filter: scrollview::MouseFilter::default(),
            last_drawn: Snapshot::blank(rows, cols),
            initialized: false,
            last_wheel: false,
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
        // A resize returns to the live view (FDR 0005): the frozen viewport was
        // measured against the old geometry and the ring is about to be cleared.
        self.scroll_offset = 0;
        self.last_scroll_state = None;
        // A width change reflows the old rows (RFC 0002 §4): drop the ring and
        // re-accumulate at the new width. The daemon restarts its per-client
        // appended-row counting when it PROCESSES the matching `Tag::Resize`, so
        // both sides go forward-only from that point — no mixed-width rows, no
        // re-ship of old ones.
        //
        // Note: a brief window exists between this ring-clear and the daemon
        // processing the matching `Tag::Resize`, during which in-flight
        // scrollback frames produced at the OLD width may arrive and append to
        // the cleared ring. The window is bounded by socket latency (≈0 on a
        // local socket) and the rows scrolled in that interval. DECIDED to
        // accept for the POSH_SESSION_FRAMES gate flip (github #107): the
        // artifact is a few boundary rows that render slightly off only when
        // scrolled into view, and self-heals as they scroll out of history. The
        // robust fix — a daemon-stamped resize epoch on `FrameBody::Scrollback`
        // so the client drops pre-resize rows deterministically — is deferred
        // there; remote's per-message cap-suppression lever does not port to the
        // once-negotiated local socket.
        self.scrollback.clear();
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
    fn render_frame(&mut self, payload: &[u8], palette: Option<&Terminal>) -> Result<Vec<u8>> {
        let frame = ServerFrame::decode(payload)?;
        // Scrollback growth (RFC 0002 §3): intercept BEFORE the applier — a
        // scrollback frame appends scrolled-off rows to the local ring and
        // leaves the visible model (and thus the tty) untouched, so it produces
        // no render bytes. Mirrors the remote client's pre-applier intercept.
        // The dup (`frame_num == applied_num`) and base (`base != applied_num`)
        // guards are degenerate on the reliable socket — frames arrive once, in
        // order — but kept for parity so a retransmit or superseding body never
        // double-appends.
        if let FrameBody::Scrollback { base, rows } = &frame.body {
            if frame.frame_num == self.applied_num {
                return Ok(Vec::new());
            }
            if *base != self.applied_num {
                return Ok(Vec::new());
            }
            let grew = rows.len();
            self.scrollback.append(rows);
            self.applied_num = frame.frame_num;
            // While scrolled up, keep the frozen viewport anchored on the same
            // content as new rows arrive (FDR 0005: output accumulates but does
            // not yank to the bottom), then repaint the scroll-view — the local
            // client has no periodic render tick, so the repaint happens here.
            if self.scroll_offset > 0 {
                self.set_scroll(self.scroll_offset + grew);
                return Ok(self.compose_scroll());
            }
            return Ok(Vec::new());
        }
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
        // While scrolled up, the live model advanced underneath but the viewport
        // stays frozen: repaint the scroll-view (memoized on the server
        // generation, so an out-of-window change diffs to nothing) rather than
        // yanking to the live bottom.
        if self.scroll_offset > 0 {
            return Ok(self.compose_scroll());
        }
        Ok(self.compose_live(palette))
    }

    /// The live visible compose: non-prediction, palette-aware. When `palette` is
    /// `Some`, the renderer's screen is composited over the greyed session before
    /// the diff (mirror remote client.rs render() ordering) so the overlay box
    /// paints in the same frame. `wheel` enables outer-terminal mouse reporting at
    /// a bare prompt so the wheel arrives as SGR events the scroll-view can
    /// intercept (mirrors the remote client); `true` = the scroll-shortcut
    /// optimization on (new_frame's default).
    fn compose_live(&mut self, palette: Option<&Terminal>) -> Vec<u8> {
        let mut next = Snapshot::from_term(&self.server_term);
        if let Some(rterm) = palette {
            composite_palette(&mut next, rterm, self.rows, self.cols);
        }
        let wheel = scrollview::wheel_active(&self.server_term);
        let bytes = display::new_frame_opt(
            self.initialized,
            &self.last_drawn,
            &next,
            wheel,
            self.last_wheel,
            true,
        );
        self.last_drawn = next;
        self.last_wheel = wheel;
        self.initialized = true;
        bytes
    }

    /// Re-run the live compose for a palette-activity repaint (the renderer
    /// pumped, or the palette opened/closed) that isn't driven by an incoming
    /// `Tag::Frame`. Threads the palette overlay, or `None` to clear it. The local
    /// client has no periodic render tick, so these repaints happen on demand.
    fn recompose(&mut self, palette: Option<&Terminal>) -> Vec<u8> {
        self.compose_live(palette)
    }

    /// Repaints the frozen history window at the current offset via the shared
    /// `remote::scrollview` compose. `scroll_opt = true`: the local client has no
    /// palette toggle for the scroll-shortcut, so it stays at new_frame's default.
    fn compose_scroll(&mut self) -> Vec<u8> {
        scrollview::compose_scroll_frame(
            self.scroll_offset,
            &self.scrollback,
            &self.server_term,
            self.rows,
            self.cols,
            &mut self.last_scroll_state,
            &mut self.initialized,
            &mut self.last_drawn,
            true,
        )
    }

    /// Sets the scroll offset (clamped to the ring) via the shared helper. The
    /// local client keeps no separate live-render memo, so the shared helper's
    /// scroll-memo invalidation is all that is needed.
    fn set_scroll(&mut self, offset: usize) {
        let ring_len = self.scrollback.len();
        scrollview::set_scroll(
            &mut self.scroll_offset,
            &mut self.last_scroll_state,
            ring_len,
            offset,
        );
    }

    /// Applies wheel ticks to the scroll offset (+ = up into history).
    fn scroll_by(&mut self, ticks: i32) {
        let ring_len = self.scrollback.len();
        scrollview::scroll_by(
            &mut self.scroll_offset,
            &mut self.last_scroll_state,
            ring_len,
            ticks,
        );
    }

    /// Processes one batch of stdin bytes on the frame path: intercepts the wheel
    /// for the local scroll-view, returning `(to_daemon, repaint)` — the bytes to
    /// forward to the daemon as `Tag::Input`, and any tty repaint to emit now.
    ///
    /// When not intercepting (the inner app set its own mouse mode, or is on the
    /// alt screen) bytes pass straight through and no repaint is produced, so a
    /// full-screen app receives raw wheel events. A wheel tick is a purely local
    /// view change and is never forwarded; any keystroke while scrolled returns
    /// the view to the live bottom and then forwards normally (FDR 0005).
    fn handle_input(&mut self, buf: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut repaint = Vec::new();
        let forward: Vec<u8> = if scrollview::wheel_active(&self.server_term) {
            let app_cursor_keys = self.server_term.app_cursor_keys();
            // The local client has no POSH_GRAB_MOUSE arrows mode: always scroll.
            let out = self.mouse_filter.feed(buf, app_cursor_keys, true);
            if out.wheel != 0 {
                self.scroll_by(out.wheel);
                repaint = self.compose_scroll();
            }
            out.bytes
        } else {
            // Not intercepting: hand back any partial held from when the wheel
            // was last active (the app enabled its own mouse mode mid-sequence)
            // so the app receives the complete sequence rather than a torn tail.
            let pending = self.mouse_filter.take_pending();
            if pending.is_empty() {
                buf.to_vec()
            } else {
                let mut joined = pending;
                joined.extend_from_slice(buf);
                joined
            }
        };
        // Any keystroke while scrolled returns to the live view, then forwards
        // normally below — you are about to type at the prompt.
        if !forward.is_empty() && self.scroll_offset > 0 {
            self.set_scroll(0);
            // `handle_input` runs only while the palette is closed (the loop
            // forwards keystrokes straight to the renderer when it is open), so
            // the return-to-live repaint never carries an overlay.
            repaint = self.compose_live(None);
        }
        (forward, repaint)
    }
}

/// The local session's palette command set (FDR 0011 Phase 2.4a/b). The
/// transport-aware entries that make sense on a reliable local socket: Suspend
/// (background the client), Shell out (a daemon-spawned escape-to-shell overlay,
/// FDR 0008), and Detach (leave the session running). The remote client's
/// echo/prediction/resync/forensics/agent/server-log commands are
/// UDP/prediction-only and intentionally omitted.
fn palette_commands() -> Value {
    json!([
        { "name": "Suspend client", "action": { "method": "client.suspend" } },
        { "name": "Shell out", "action": { "method": "shell.open" } },
        { "name": "Detach", "action": { "method": "app.detach" } },
    ])
}

/// Split a forwarded stdin batch at the first Ctrl-^ (the frames-on palette-open
/// key). Returns the bytes before the escape (ordinary input) and, when an
/// escape was present, the bytes after it (routed to the palette; the `0x1e`
/// itself is dropped). The caller only consults this when a `FrameRenderer` is
/// live — gate-off leaves the `0x1e` in the ordinary-input slice.
fn split_escape(forward: &[u8]) -> (&[u8], Option<&[u8]>) {
    match forward.iter().position(|&b| b == ESCAPE_KEY) {
        Some(pos) => (&forward[..pos], Some(&forward[pos + 1..])),
        None => (forward, None),
    }
}

/// Open the command palette over the live session (frames-on only): spawn the
/// renderer on first use, summon it, drop any scroll-view back to the live bottom
/// (the palette overlays the live session, not history), force a repaint, and
/// paint the greyed overlay now. Returns false if the renderer can't be spawned,
/// leaving the caller to forward the `Ctrl-^` to the shell as an ordinary byte.
fn open_local_palette(
    palette: &mut Option<Palette>,
    fr: &mut FrameRenderer,
    stdout_buf: &mut Vec<u8>,
    rows: u16,
    cols: u16,
) -> bool {
    if palette.is_none() {
        *palette = Palette::spawn(rows, cols);
    }
    let Some(p) = palette.as_mut() else {
        return false;
    };
    p.open("Commands", palette_commands());
    fr.set_scroll(0);
    fr.invalidate();
    stdout_buf.extend_from_slice(&fr.recompose(p.screen()));
    true
}

/// The tty-side effect a dispatched palette action still needs applied after it
/// returns. `client.suspend` can't run inside [`dispatch_local_action`] (it needs
/// the loop's `RawMode` and blocks in `SIGSTOP`), so it is surfaced here and run
/// at the call site; `Detach` is a pure wire effect handled inline.
enum LocalAction {
    /// The action's effect is complete (its wire frame, if any, is queued).
    None,
    /// Run [`suspend`] with the loop's `RawMode`.
    Suspend,
}

/// Dispatch a palette selection on the local client. Wire-only effects (Detach)
/// are applied here against `sock_write_buf`; the tty-side Suspend is returned
/// for the caller to run with its `RawMode`. Splitting it this way keeps the
/// dispatch unit-testable without a real tty.
fn dispatch_local_action(method: &str, sock_write_buf: &mut Vec<u8>) -> LocalAction {
    match method {
        "app.detach" => {
            // Same wire effect as the DetachMatcher path: ask the daemon to
            // detach this client, leaving the session running.
            ipc::append_frame(sock_write_buf, Tag::Detach, b"");
            LocalAction::None
        }
        "shell.open" => {
            // Escape-to-shell (FDR 0008): ask the daemon to open a transient
            // shell overlay in the session cwd. Pure wire effect — the client
            // stays a passive frame consumer; the overlay's screen arrives over
            // the existing Tag::Frame path, and Ctrl-D closes it daemon-side.
            ipc::append_frame(sock_write_buf, Tag::Shell, b"");
            LocalAction::None
        }
        "client.suspend" => LocalAction::Suspend,
        _ => LocalAction::None, // unknown method: the renderer already closed
    }
}

/// Suspend the local client (mirror remote client.rs `suspend`): leave the
/// alternate screen and restore the tty driver so the parent shell is visible,
/// print the suspend banner, stop our own process group, and — on `SIGCONT`
/// (fg) — re-enter raw mode and the alternate screen. The forced repaint of the
/// resumed session is the caller's `fr.invalidate()` + recompose (and the loop's
/// `SIGCONT` handler), matching the remote client's `st.initialized = false`.
fn suspend(raw: &RawMode) {
    let _ = util::write_all_retry(STDOUT, &display::close(), 1000);
    raw.restore();
    let _ = util::write_all_retry(STDOUT, b"\r\n\x1b[37;44m[posh is suspended.]\x1b[m\r\n", 1000);
    util::stop_own_pgroup();
    // Execution resumes here after SIGCONT (fg): back onto the alternate screen
    // before the caller's forced repaint redraws it.
    raw.reapply();
    let _ = util::write_all_retry(STDOUT, &display::open(), 1000);
}

/// Bridges the tty to the daemon until detach or session end. Returns the
/// session shell's exit status (0 on detach or connection loss).
/// `enter` is re-written on SIGCONT, when the outer terminal may have
/// left our alternate screen while we were stopped. `raw` is the tty's raw-mode
/// guard, borrowed for the palette's Suspend command (temporary restore + re-arm
/// around `SIGSTOP`).
fn client_loop(stream: UnixStream, enter: &[u8], raw: &RawMode) -> Result<i32> {
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
    // Advertise CAP_SCROLLBACK (RFC 0002 §1) alongside the base table so a
    // frame-emitting daemon syncs scrolled-off rows to our local ring. The `0`
    // payload requests the server-default ring depth. Harmless when the daemon
    // isn't producing frames (gate off): the cap is parsed and ignored.
    init_payload.extend_from_slice(&caps::encode_table(&caps::own_table(&[caps::Cap {
        id: caps::CAP_SCROLLBACK,
        payload: vec![0],
    }])));
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

    // The command-palette overlay (FDR 0011 Phase 2.4a): stays None until the
    // user first opens it with Ctrl-^ on a frames-on session, then persists
    // (spawned renderer reused across opens) until loop teardown. A gate-off
    // session never builds a FrameRenderer, so Ctrl-^ is never intercepted and
    // this stays None — the palette is a frames-on feature.
    let mut palette: Option<Palette> = None;

    let err_events = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
    let result: Result<i32> = 'client: loop {
        if util::take_flag(&util::SIGWINCH_RECEIVED) {
            let (rows, cols) = pty::term_size(STDOUT);
            frame_size = (rows, cols);
            if let Some(fr) = frame_renderer.as_mut() {
                fr.resize(rows, cols);
            }
            // Keep the palette renderer sized to the tty while it is up (mirror
            // remote client.rs); a closed renderer is resized when next opened.
            if let Some(p) = palette.as_mut().filter(|p| p.is_open()) {
                p.resize(rows, cols);
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
            break 'client Ok(0);
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
        // Poll the palette renderer's fds (its PTY + control socket) while it is
        // resident, so its output drains and selections are seen promptly (mirror
        // remote client.rs `palette_base`). Recorded after the fixed fds so the
        // base index maps `revents` back to master (base) and ctrl (base + 1).
        let palette_base = palette.as_ref().map(|p| {
            let base = fds.len();
            fds.push(util::pollfd(p.master_fd(), libc::POLLIN));
            fds.push(util::pollfd(p.ctrl_fd(), libc::POLLIN));
            base
        });

        // Bounded timeout: a signal landing between the flag checks above
        // and this poll sets the flag without an EINTR; an infinite poll
        // would then sit raw-mode until unrelated activity. One wakeup per
        // second bounds that race (the remote loop does the same).
        match util::poll(&mut fds, 1000) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => break 'client Err(e.into()),
        }

        // stdin -> daemon
        if fds[0].revents & (libc::POLLIN | err_events) != 0 {
            let mut buf = [0u8; 4096];
            match util::read_fd(STDIN, &mut buf) {
                Ok(0) => break 'client Ok(0),
                Ok(n) => {
                    let input = &buf[..n];
                    // While the palette overlay is up the renderer owns the
                    // keyboard: forward raw keystrokes to it and send nothing to
                    // the daemon (mirror remote client.rs). Its selection/cancel
                    // arrives on the control channel, serviced below.
                    if let Some(p) = palette.as_ref().filter(|p| p.is_open()) {
                        p.forward_input(input);
                    } else {
                        let (forward, detached) = detach.feed(input);
                        // Frames-on only: a lone Ctrl-^ opens the palette. The
                        // bytes before it are ordinary input; the bytes after it
                        // route to the palette (the 0x1e is dropped). Gate-off (no
                        // renderer) never looks for the escape, so 0x1e stays in
                        // the ordinary-input slice and reaches the shell.
                        let (normal, after_escape) = if frame_renderer.is_some() {
                            split_escape(&forward)
                        } else {
                            (&forward[..], None)
                        };
                        // Ordinary input: when a FrameRenderer is live it
                        // intercepts the wheel for the local scroll-view (bare
                        // prompt only) and repaints the tty; the rest forwards as
                        // input. Gate-off forwards `normal` verbatim — the raw
                        // wheel reaches the daemon byte for byte, exactly as today.
                        if !normal.is_empty() {
                            let to_daemon = if let Some(fr) = frame_renderer.as_mut() {
                                let (fwd, repaint) = fr.handle_input(normal);
                                if !repaint.is_empty() {
                                    stdout_buf.extend_from_slice(&repaint);
                                }
                                fwd
                            } else {
                                normal.to_vec()
                            };
                            if !to_daemon.is_empty() {
                                ipc::append_frame(&mut sock_write_buf, Tag::Input, &to_daemon);
                            }
                        }
                        // Open the palette on the escape byte (frames-on). If the
                        // renderer can't be spawned, forward the Ctrl-^ and its
                        // trailing bytes to the shell as ordinary input.
                        if let Some(after) = after_escape {
                            let fr = frame_renderer
                                .as_mut()
                                .expect("split_escape only fires with a live renderer");
                            let (rows, cols) = frame_size;
                            if open_local_palette(&mut palette, fr, &mut stdout_buf, rows, cols) {
                                if !after.is_empty() {
                                    if let Some(p) = palette.as_ref() {
                                        p.forward_input(after);
                                    }
                                }
                            } else {
                                let mut literal = vec![ESCAPE_KEY];
                                literal.extend_from_slice(after);
                                ipc::append_frame(&mut sock_write_buf, Tag::Input, &literal);
                            }
                        }
                        if detached {
                            ipc::append_frame(&mut sock_write_buf, Tag::Detach, b"");
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => break 'client Err(e.into()),
            }
        }

        // daemon -> stdout
        if fds[1].revents & libc::POLLIN != 0 {
            match read_buf.read_from(sock_fd) {
                Ok(0) => break 'client Ok(0),
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
                            // first frame so the gate-off path stays inert. The
                            // palette overlay (when open) composites on top of the
                            // freshly-applied frame in the same render.
                            Tag::Frame => {
                                let screen = palette
                                    .as_ref()
                                    .filter(|p| p.is_open())
                                    .and_then(Palette::screen);
                                let fr = frame_renderer.get_or_insert_with(|| {
                                    FrameRenderer::new(frame_size.0, frame_size.1)
                                });
                                let bytes = match fr.render_frame(&frame.payload, screen) {
                                    Ok(bytes) => bytes,
                                    Err(e) => break 'client Err(e),
                                };
                                stdout_buf.extend_from_slice(&bytes);
                            }
                            Tag::Exit => {
                                // Session over: flush the final output and
                                // carry the shell's status out.
                                if !stdout_buf.is_empty() {
                                    let _ = util::write_all_retry(STDOUT, &stdout_buf, 1000);
                                }
                                break 'client Ok(ipc::decode_exit(&frame.payload).unwrap_or(0));
                            }
                            _ => {}
                        },
                        Ok(None) => break,
                        Err(e) => break 'client Err(e),
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionReset
                        || e.kind() == std::io::ErrorKind::BrokenPipe =>
                {
                    break 'client Ok(0);
                }
                Err(e) => break 'client Err(e.into()),
            }
        }

        // Palette overlay servicing (mirror remote client.rs): drain the
        // renderer's screen so the overlay repaints, and act on any
        // selection/cancel it reported. The local client has no periodic render
        // tick, so each palette activity recomposites here on demand.
        if let Some(base) = palette_base {
            if fds[base].revents & libc::POLLIN != 0 {
                let changed = palette.as_mut().map(Palette::pump).unwrap_or(false);
                if changed {
                    if let Some(fr) = frame_renderer.as_mut() {
                        let screen = palette
                            .as_ref()
                            .filter(|p| p.is_open())
                            .and_then(Palette::screen);
                        stdout_buf.extend_from_slice(&fr.recompose(screen));
                    }
                }
            }
            if fds[base + 1].revents & libc::POLLIN != 0 {
                match palette.as_mut().map(Palette::poll_events) {
                    Some(PaletteEvent::Action { method, .. }) => {
                        if let LocalAction::Suspend =
                            dispatch_local_action(&method, &mut sock_write_buf)
                        {
                            suspend(raw);
                        }
                        // The palette closed and (for Suspend) the tty was cleared
                        // and restored: force a full repaint of the plain session.
                        if let Some(fr) = frame_renderer.as_mut() {
                            fr.invalidate();
                            stdout_buf.extend_from_slice(&fr.recompose(None));
                        }
                    }
                    Some(PaletteEvent::Cancelled) => {
                        // Dismissed without a selection: repaint the plain session.
                        if let Some(fr) = frame_renderer.as_mut() {
                            fr.invalidate();
                            stdout_buf.extend_from_slice(&fr.recompose(None));
                        }
                    }
                    // The local 2.4a menu has no dialog commands, so Copy is
                    // unreachable; a later dialog command would emit OSC 52 here.
                    Some(PaletteEvent::Copy) | Some(PaletteEvent::None) | None => {}
                }
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
                    break 'client Ok(0);
                }
                Err(e) => break 'client Err(e.into()),
            }
        }

        if !stdout_buf.is_empty() {
            match util::write_fd(STDOUT, &stdout_buf) {
                Ok(n) => {
                    stdout_buf.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => break 'client Err(e.into()),
            }
        }

        if fds[1].revents & err_events != 0 {
            break 'client Ok(0);
        }
    };

    // Tear down the palette renderer (if any) on every loop exit — detach,
    // session end, signal, or error.
    if let Some(p) = palette.take() {
        p.shutdown();
    }
    result
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
                .render_frame(payload, None)
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
        let bytes = renderer.render_frame(&f1, None).expect("keyframe applies");
        outer.process(&bytes);
        if outer.rows() != rows || outer.cols() != cols {
            outer.resize(rows, cols);
        }
        assert_screens_match(&outer, &daemon);
    }

    // ---- Task 2.5a: local client rings scrollback frames (RFC 0002) ----

    /// Encode a `FrameBody::Scrollback` the way the daemon's `maybe_queue_scrollback`
    /// does: advance a scrollback slot (carrying the row count forward), anchor
    /// the body to the CONFIRMED visible base (`acked_num`), and self-ack — the
    /// reliable-socket production the local client consumes.
    fn daemon_scrollback_frame(producer: &mut FrameProducer, rows: Vec<Vec<u8>>) -> Vec<u8> {
        let sb_total = producer.current_sb_total() + rows.len() as u64;
        producer.advance_scrollback(sb_total);
        let frame_num = producer.current_num();
        let bytes = ServerFrame {
            flags: 0,
            caps: caps::own_table(&[]),
            frame_num,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Scrollback {
                base: producer.acked_num(),
                rows,
            },
        }
        .encode();
        producer.ack(frame_num);
        bytes
    }

    /// The core Task 2.5a client property: a `FrameBody::Scrollback` frame is
    /// intercepted before the applier — it appends the carried rows to the
    /// renderer's local ring, advances `applied_num`, and produces NO render
    /// bytes while leaving the visible model byte-identical (scrollback never
    /// touches the visible screen).
    #[test]
    fn frame_renderer_rings_scrollback_without_touching_the_screen() {
        let (rows, cols) = (24u16, 80u16);
        let mut daemon = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut daemon);

        let mut producer = FrameProducer::new(rows, cols);
        let mut renderer = FrameRenderer::new(rows, cols);
        let mut outer = Terminal::with_scrollback(rows, cols, 0);

        // Frame 1: the Full keyframe establishes the visible base (applied_num=1).
        let f1 = daemon_frame(&mut producer, &daemon);
        outer.process(&renderer.render_frame(&f1, None).expect("keyframe applies"));
        assert_eq!(renderer.applied_num, 1);
        let visible_after_keyframe = Snapshot::from_term(&renderer.server_term);
        assert!(renderer.scrollback.is_empty(), "no scrollback rung yet");

        // Frame 2: a Scrollback body carrying two rows, based on the keyframe.
        let sb_rows = vec![
            b"scrolled row A\r\n".to_vec(),
            b"scrolled row B\r\n".to_vec(),
        ];
        let f2 = daemon_scrollback_frame(&mut producer, sb_rows.clone());
        let bytes = renderer.render_frame(&f2, None).expect("scrollback applies");

        // No render bytes, visible model unchanged, applied_num advanced, ring
        // holds exactly the carried rows.
        assert!(bytes.is_empty(), "a scrollback frame must produce no render bytes");
        assert_eq!(
            Snapshot::from_term(&renderer.server_term),
            visible_after_keyframe,
            "the visible model must be untouched by a scrollback frame"
        );
        assert_eq!(renderer.applied_num, 2, "the scrollback frame advances applied_num");
        assert_eq!(renderer.scrollback.len(), 2);
        assert_eq!(renderer.scrollback.row(0), Some(sb_rows[0].as_slice()));
        assert_eq!(renderer.scrollback.row(1), Some(sb_rows[1].as_slice()));

        // A subsequent visible Diff still applies cleanly against the scrollback
        // frame's number — the diff-base chain threads through the scrollback frame.
        daemon.process(b"more output");
        let f3 = daemon_frame(&mut producer, &daemon);
        assert!(
            matches!(
                ServerFrame::decode(&f3).unwrap().body,
                FrameBody::Diff { base: 2, .. }
            ),
            "the visible diff must anchor on the scrollback frame (base 2)"
        );
        renderer.render_frame(&f3, None).expect("the diff applies after scrollback");
        assert_eq!(renderer.applied_num, 3);
        assert_eq!(
            Snapshot::from_term(&renderer.server_term),
            Snapshot::from_term(&daemon),
            "the visible model tracks the daemon across the interleaved scrollback"
        );
        // The ring is undisturbed by the later visible frame.
        assert_eq!(renderer.scrollback.len(), 2, "a visible frame must not touch the ring");
    }

    /// A width resize drops the local ring (RFC 0002 §4): the old rows were at a
    /// different width, so the renderer re-accumulates forward at the new width.
    #[test]
    fn resize_clears_the_scrollback_ring() {
        let (rows, cols) = (24u16, 80u16);
        let mut renderer = FrameRenderer::new(rows, cols);
        renderer
            .scrollback
            .append(&[b"old width row\r\n".to_vec()]);
        assert_eq!(renderer.scrollback.len(), 1);

        renderer.resize(rows, 100);
        assert!(
            renderer.scrollback.is_empty(),
            "a resize must clear the ring so the new width re-accumulates cleanly"
        );
    }

    // ---- Task 2.5b: the local scroll-view is scrollable (wheel + view) -------

    /// Read a rendered outer terminal's rows as trimmed strings, for content
    /// assertions on the scroll-view.
    fn rows_text(term: &Terminal) -> Vec<String> {
        let snap = Snapshot::from_term(term);
        (0..term.rows())
            .map(|r| {
                (0..term.cols())
                    .filter_map(|c| snap.cell(r, c))
                    .map(|cell| if cell.ch == '\0' { ' ' } else { cell.ch })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The core Task 2.5b property: with a populated ring, a wheel-up scrolls the
    /// local view into captured history (SCROLLBACK indicator + history rows),
    /// forwarding nothing to the daemon; a subsequent keystroke returns to the
    /// live screen and forwards the key. Driven end-to-end through the real input
    /// path (`handle_input` fed synthetic SGR wheel bytes), with the rendered
    /// escape stream replayed onto an outer terminal to check what the user sees.
    #[test]
    fn frame_renderer_scrolls_captured_history_and_returns_to_live() {
        let (rows, cols) = (5u16, 20u16);
        let mut producer = FrameProducer::new(rows, cols);
        let mut renderer = FrameRenderer::new(rows, cols);
        // The outer tty: a fresh model fed ONLY the client's rendered bytes.
        let mut outer = Terminal::with_scrollback(rows, cols, 0);

        // Visible keyframe: a bare prompt on the primary screen (wheel-active).
        let mut daemon = Terminal::with_scrollback(rows, cols, 1000);
        daemon.process(b"\x1b[2J\x1b[Hlive prompt line");
        let f1 = daemon_frame(&mut producer, &daemon);
        outer.process(&renderer.render_frame(&f1, None).expect("keyframe applies"));
        assert_eq!(renderer.scroll_offset, 0, "starts at the live bottom");

        // Ring up eight history rows via a scrollback frame (no repaint at
        // offset 0).
        let hist: Vec<Vec<u8>> = (0..8)
            .map(|i| format!("history line {i}\r\n").into_bytes())
            .collect();
        let f2 = daemon_scrollback_frame(&mut producer, hist.clone());
        let sb_bytes = renderer.render_frame(&f2, None).expect("scrollback applies");
        assert!(sb_bytes.is_empty(), "ringing history while live paints nothing");
        assert_eq!(renderer.scrollback.len(), 8);

        // Wheel up two ticks (2 * WHEEL_STEP = 6 lines) → scroll into history.
        let wheel_up = b"\x1b[<64;1;1M\x1b[<64;1;1M";
        let (to_daemon, repaint) = renderer.handle_input(wheel_up);
        assert!(to_daemon.is_empty(), "a wheel tick is a local view change, not input");
        assert!(renderer.scroll_offset > 0, "the wheel scrolled the view up");
        assert!(!repaint.is_empty(), "the scroll-view painted");
        outer.process(&repaint);

        let view = rows_text(&outer);
        assert!(view[0].contains("SCROLLBACK"), "indicator on the top row: {view:?}");
        assert!(
            view.iter().any(|r| r.contains("history line")),
            "captured history is visible in the scroll-view: {view:?}"
        );
        assert!(
            !view.iter().any(|r| r.contains("live prompt line")),
            "the live prompt is scrolled out of the frozen window: {view:?}"
        );

        // A keystroke returns to the live view and forwards to the daemon.
        let (fwd, repaint) = renderer.handle_input(b"x");
        assert_eq!(fwd, b"x", "the keystroke forwards to the daemon");
        assert_eq!(renderer.scroll_offset, 0, "a keystroke returns to the live bottom");
        outer.process(&repaint);
        let live = rows_text(&outer);
        assert!(live[0].contains("live prompt line"), "the live screen is restored: {live:?}");
        assert!(
            !live.iter().any(|r| r.contains("SCROLLBACK")),
            "the scroll indicator is gone on return to live: {live:?}"
        );
    }

    /// While an app has mouse mode on (or is on the alt screen) the wheel is NOT
    /// intercepted: raw SGR wheel bytes pass straight through `handle_input` to
    /// the daemon, so full-screen apps (vim/htop) get real wheel events.
    #[test]
    fn wheel_passes_through_when_app_holds_mouse_mode() {
        let (rows, cols) = (5u16, 20u16);
        let mut producer = FrameProducer::new(rows, cols);
        let mut renderer = FrameRenderer::new(rows, cols);

        // Keyframe where the inner app has enabled SGR mouse tracking.
        let mut daemon = Terminal::with_scrollback(rows, cols, 1000);
        daemon.process(b"\x1b[?1000h\x1b[?1006h");
        let f1 = daemon_frame(&mut producer, &daemon);
        renderer.render_frame(&f1, None).expect("keyframe applies");

        let wheel = b"\x1b[<64;10;5M";
        let (to_daemon, repaint) = renderer.handle_input(wheel);
        assert_eq!(to_daemon, wheel, "raw wheel forwards unchanged to the app");
        assert!(repaint.is_empty(), "no local scroll-view while the app owns the mouse");
        assert_eq!(renderer.scroll_offset, 0, "the wheel did not scroll the local view");
    }

    /// Gate-off invariant: with `POSH_SESSION_FRAMES` off there is no
    /// FrameRenderer, so the client_loop stdin path forwards `forward` verbatim.
    /// The only stage between raw stdin and `Tag::Input` is the detach matcher,
    /// which passes wheel SGR bytes through untouched — the daemon receives the
    /// raw wheel exactly as before this task.
    #[test]
    fn gate_off_forwards_wheel_bytes_to_daemon_unchanged() {
        let mut m = DetachMatcher::default();
        let wheel = b"\x1b[<64;10;5M";
        assert_eq!(m.feed(wheel), (wheel.to_vec(), false));
    }

    // ---- Task 2.4a: command palette on the local session client ------------

    /// `split_escape` is the open decision the loop makes when a FrameRenderer is
    /// live: a lone Ctrl-^ (0x1e) partitions the batch into the ordinary input
    /// before it and the palette-bound bytes after it, dropping the escape byte.
    #[test]
    fn split_escape_partitions_input_at_ctrl_caret() {
        assert_eq!(
            split_escape(b"ab\x1ecd"),
            (b"ab".as_slice(), Some(b"cd".as_slice())),
            "bytes before the escape are input; bytes after route to the palette"
        );
        assert_eq!(
            split_escape(b"\x1e"),
            (b"".as_slice(), Some(b"".as_slice())),
            "a lone Ctrl-^ opens the palette with no leading input and an empty tail"
        );
        assert_eq!(
            split_escape(b"abc"),
            (b"abc".as_slice(), None),
            "no escape: the whole batch is ordinary input"
        );
    }

    /// Gate-off invariant: with `POSH_SESSION_FRAMES` off there is no
    /// FrameRenderer, so the loop never calls `split_escape` and Ctrl-^ is not
    /// intercepted. The only stage before `Tag::Input` is the detach matcher,
    /// which forwards the `0x1e` byte untouched — the shell receives it, exactly
    /// as before this task (mirror `gate_off_forwards_wheel_bytes_...`).
    #[test]
    fn gate_off_leaves_ctrl_caret_in_the_forwarded_stream() {
        let mut m = DetachMatcher::default();
        assert_eq!(m.feed(b"\x1e"), (vec![0x1e], false));
    }

    /// `app.detach` dispatch queues the same `Tag::Detach` wire frame the
    /// `DetachMatcher` path emits, and reports no further tty-side action.
    #[test]
    fn dispatch_detach_appends_a_detach_frame() {
        let mut buf = Vec::new();
        assert!(matches!(
            dispatch_local_action("app.detach", &mut buf),
            LocalAction::None
        ));
        assert_eq!(buf, ipc::encode_frame(Tag::Detach, b""));
    }

    /// `shell.open` dispatch queues a `Tag::Shell` wire frame (empty payload) —
    /// the daemon spawns the escape-to-shell overlay (FDR 0008) on receipt — and
    /// reports no tty-side action; the client stays a passive frame consumer.
    #[test]
    fn dispatch_shell_open_appends_a_shell_frame() {
        let mut buf = Vec::new();
        assert!(matches!(
            dispatch_local_action("shell.open", &mut buf),
            LocalAction::None
        ));
        assert_eq!(buf, ipc::encode_frame(Tag::Shell, b""));
    }

    /// `client.suspend` is routed as a tty-side action (run with the loop's
    /// RawMode at the call site) and queues no wire frame. The actual SIGSTOP is
    /// not unit-testable, matching the remote client.
    #[test]
    fn dispatch_suspend_routes_to_the_suspend_action() {
        let mut buf = Vec::new();
        assert!(matches!(
            dispatch_local_action("client.suspend", &mut buf),
            LocalAction::Suspend
        ));
        assert!(buf.is_empty(), "suspend queues no wire frame");
    }

    /// An unknown method (the renderer already closed the palette) is inert.
    #[test]
    fn dispatch_unknown_method_is_inert() {
        let mut buf = Vec::new();
        assert!(matches!(
            dispatch_local_action("no.such.method", &mut buf),
            LocalAction::None
        ));
        assert!(buf.is_empty());
    }

    /// The core 2.4a render property: the palette renderer's screen composites
    /// onto the live local session — the session greys behind it and the
    /// renderer's box is anchored a third of the way down — and clears cleanly on
    /// close. Driven through the real `FrameRenderer::recompose`, with the
    /// rendered escapes replayed onto an outer terminal to check what the user
    /// sees (mirrors the moved `composite_palette_*` tests, but end-to-end).
    #[test]
    fn palette_screen_composites_onto_the_local_snapshot() {
        let (rows, cols) = (24u16, 80u16);
        let mut daemon = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut daemon);
        let mut producer = FrameProducer::new(rows, cols);
        let mut renderer = FrameRenderer::new(rows, cols);
        let mut outer = Terminal::with_scrollback(rows, cols, 0);

        // Establish the live session on the outer tty (bright, undimmed).
        let f1 = daemon_frame(&mut producer, &daemon);
        outer.process(&renderer.render_frame(&f1, None).expect("keyframe applies"));
        assert!(
            !outer.screen().cell(0, 0).unwrap().style.dim,
            "the session starts bright, before any overlay"
        );

        // A palette renderer screen showing "HI" at row 0.
        let mut pterm = Terminal::new(rows, cols);
        pterm.process(b"HI");

        // Recompose with the overlay (as the loop does on palette open / pump).
        renderer.invalidate();
        outer.process(&renderer.recompose(Some(&pterm)));

        let has_hi = |t: &Terminal| {
            let snap = Snapshot::from_term(t);
            (0..rows).any(|r| {
                (0..cols.saturating_sub(1)).any(|c| {
                    snap.cell(r, c).map(|x| x.ch) == Some('H')
                        && snap.cell(r, c + 1).map(|x| x.ch) == Some('I')
                })
            })
        };
        assert!(
            has_hi(&outer),
            "the palette 'HI' box is composited onto the session"
        );
        assert!(
            outer.screen().cell(0, 0).unwrap().style.dim,
            "the session is greyed behind the palette overlay"
        );

        // Closing the overlay (recompose with None) repaints the plain session:
        // the box is gone and the session is bright again.
        renderer.invalidate();
        outer.process(&renderer.recompose(None));
        assert!(!has_hi(&outer), "the overlay is cleared on close");
        assert!(
            !outer.screen().cell(0, 0).unwrap().style.dim,
            "the session brightens back on close"
        );
    }
}
