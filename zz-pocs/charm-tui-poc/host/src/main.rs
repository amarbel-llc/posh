//! charm-tui-host: host the bubbletea POC command bar (`tui/tui-bin`) as a
//! centered popup composited over a retained session screen — proving posh's
//! client-side emulator can run an arbitrary charmbracelet TUI as a mux-style
//! popup. Throwaway POC: hardcoded constants, no flags. All libc/PTY FFI is
//! confined here so posh-term stays 100% safe.
//!
//! Rendering uses posh-term as a compositor rather than a separate-PTY blit:
//!   * `session`: a posh_term::Terminal holding the background ("live session").
//!   * `bar`: a posh_term::Terminal fed the command bar's PTY output.
//!   * compose(): copy the session cells, then overlay the bar's drawn region
//!     (its non-blank bounding box) centered on top — like `tmux display-popup`.
//!   * a per-cell diff renderer writes only the cells that changed since the
//!     last frame (reusing posh_term::sgr_params for styling), wrapped in
//!     synchronized-output. So the popup is centered, and when it shrinks or
//!     closes the vacated cells revert to the session underneath — no
//!     full-screen clear, no stale rectangle.
//!
//! Two behaviours chosen by whether stdout is a tty (an OS fact, not a flag):
//!   * stdout IS a tty  -> interactive: session screen + chord/"/" -> popup.
//!   * stdout is NOT a tty -> self-test: assert the hosted bar renders, filters,
//!     and runs/quits; that compose() centers the popup over the background; and
//!     that the chord state machine maps correctly. Print PASS/FAIL, exit 0/1.

use std::ffi::CString;
use std::time::Duration;

use posh_term::{sgr_params, Cell, Screen, Style, Terminal};

/// Path to the bubbletea binary, anchored at compile time to this crate's
/// manifest dir so it resolves regardless of the runtime CWD.
const TUI_BIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tui/tui-bin");
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
const STDIN: libc::c_int = 0;
const STDOUT: libc::c_int = 1;
/// Quiet period with no PTY output that counts as "the TUI finished drawing".
const IDLE: Duration = Duration::from_millis(400);
/// Exit status the command bar uses to request that the whole driver quit (vs.
/// exit 0 = "overlay closed, return to base"). Mirrors quitExitCode in
/// tui/main.go.
const QUIT_SENTINEL: i32 = 42;

// Chord: Ctrl-^ (0x1e) prefix + a key, matching posh's existing escape chord
// (remote/client.rs ESCAPE_KEY). `Ctrl-^ .` is the reachable stand-in for the
// eventual `Ctrl-.` (a bare Ctrl-. is not a control byte and needs the kitty /
// CSI-u keyboard protocol to report — deferred).
const CHORD_PREFIX: u8 = 0x1e; // Ctrl-^
const CHORD_OPEN: u8 = b'.'; // Ctrl-^ .  -> summon the command bar
const CHORD_QUIT: u8 = b'q'; // Ctrl-^ q  -> quit the driver
const SLASH: u8 = b'/'; // bare "/" also summons (trapeze's native trigger)

/// The retained "live session" background screen the popup composites over.
const BASE_SCREEN: &[u8] = b"\x1b[2J\x1b[H  posh client \xe2\x80\x94 live session (POC base screen)\r\n\r\n  \x1b[1m/\x1b[0m  or  \x1b[1mCtrl-^ .\x1b[0m   command palette\r\n  \x1b[1mCtrl-^ q\x1b[0m            quit\r\n";

fn main() {
    // Make the child's color/term detection deterministic and non-blocking.
    std::env::set_var("TERM", "xterm-256color");
    std::env::set_var("COLORTERM", "truecolor");

    let bin = std::fs::canonicalize(TUI_BIN).unwrap_or_else(|e| panic!("cannot find {TUI_BIN}: {e}"));
    let bin = CString::new(bin.into_os_string().into_encoded_bytes()).unwrap();

    if isatty(STDOUT) {
        interactive(&bin);
    } else {
        std::process::exit(selftest(&bin));
    }
}

// ---------------------------------------------------------------------------
// Chord state machine — the input-intercept logic, unit-testable in isolation.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Pass these bytes through to whatever runs underneath (the session).
    Forward(Vec<u8>),
    /// `Ctrl-^ .` — summon the overlay.
    OpenOverlay,
    /// `Ctrl-^ q` — quit the driver.
    Quit,
    /// Consumed the prefix; waiting for the next byte.
    Pending,
}

struct Chord {
    armed: bool,
}

impl Chord {
    fn new() -> Chord {
        Chord { armed: false }
    }

    fn feed(&mut self, b: u8) -> Action {
        if self.armed {
            self.armed = false;
            match b {
                CHORD_OPEN => Action::OpenOverlay,
                CHORD_QUIT => Action::Quit,
                // `Ctrl-^ Ctrl-^` emits a literal Ctrl-^ to the session.
                CHORD_PREFIX => Action::Forward(vec![CHORD_PREFIX]),
                other => Action::Forward(vec![other]),
            }
        } else if b == CHORD_PREFIX {
            self.armed = true;
            Action::Pending
        } else {
            Action::Forward(vec![b])
        }
    }
}

// ---------------------------------------------------------------------------
// Compositor: session background + centered popup -> presentation grid, then a
// per-cell diff to the real terminal.
// ---------------------------------------------------------------------------

/// Tracks the cells currently on the real screen so each frame only writes the
/// cells that changed (a minimal diff, like the live client's display path).
struct Presenter {
    rows: u16,
    cols: u16,
    prev: Vec<Cell>,
}

impl Presenter {
    /// Starts assuming a freshly cleared (all-blank) screen.
    fn new(rows: u16, cols: u16) -> Presenter {
        let blank = Cell::blank(Style::default());
        Presenter {
            rows,
            cols,
            prev: vec![blank; rows as usize * cols as usize],
        }
    }

    fn flush(&mut self, session: &Terminal, overlay: Option<&Terminal>, armed: bool) {
        let cur = compose(self.rows, self.cols, session, overlay, armed);
        let mut body = Vec::new();
        diff(&self.prev, &cur, self.rows, self.cols, &mut body);
        if !body.is_empty() {
            let mut frame = Vec::with_capacity(body.len() + 16);
            frame.extend_from_slice(b"\x1b[?2026h"); // begin synchronized update
            frame.extend_from_slice(&body);
            frame.extend_from_slice(b"\x1b[?2026l"); // end synchronized update
            write_all(STDOUT, &frame);
        }
        self.prev = cur;
    }
}

/// Build the presentation grid: the session background with `overlay` (if any)
/// composited as a centered popup, or the armed status line on the bottom row.
fn compose(rows: u16, cols: u16, session: &Terminal, overlay: Option<&Terminal>, armed: bool) -> Vec<Cell> {
    let w = cols as usize;
    let mut cur = vec![Cell::blank(Style::default()); rows as usize * w];

    let sscr = session.screen();
    for r in 0..rows {
        for c in 0..cols {
            if let Some(cell) = sscr.cell(r, c) {
                cur[r as usize * w + c as usize] = cell.clone();
            }
        }
    }

    if let Some(bar) = overlay {
        if let Some((r0, c0, r1, c1)) = bbox(bar.screen()) {
            let h = r1 - r0 + 1;
            let bw = c1 - c0 + 1;
            // Anchor the popup's top a third of the way down and center it
            // horizontally. The top stays put as the list grows/shrinks, so it
            // expands downward and collapses upward (a long list in a short
            // terminal clips at the bottom — list scrolling is a follow-up).
            let dr = rows / 3;
            let dc = cols.saturating_sub(bw) / 2;
            let bscr = bar.screen();
            for r in 0..h {
                for c in 0..bw {
                    let (pr, pc) = (dr + r, dc + c);
                    if pr < rows && pc < cols {
                        if let Some(cell) = bscr.cell(r0 + r, c0 + c) {
                            cur[pr as usize * w + pc as usize] = cell.clone();
                        }
                    }
                }
            }
        }
    } else if armed {
        overlay_status_line(&mut cur, rows, cols);
    }

    cur
}

/// The non-blank bounding box of a screen: (top, left, bottom, right), or None
/// if the screen is entirely blank. This is the popup's drawn region.
fn bbox(scr: &Screen) -> Option<(u16, u16, u16, u16)> {
    let mut found = false;
    let (mut r0, mut c0, mut r1, mut c1) = (0u16, 0u16, 0u16, 0u16);
    for r in 0..scr.rows() {
        for c in 0..scr.cols() {
            if scr.cell(r, c).map(|cell| !cell.is_blank()).unwrap_or(false) {
                if !found {
                    found = true;
                    (r0, c0, r1, c1) = (r, c, r, c);
                } else {
                    r0 = r0.min(r);
                    c0 = c0.min(c);
                    r1 = r1.max(r);
                    c1 = c1.max(c);
                }
            }
        }
    }
    found.then_some((r0, c0, r1, c1))
}

/// Paint a reverse-video chord-armed hint onto the bottom row of the grid.
fn overlay_status_line(cur: &mut [Cell], rows: u16, cols: u16) {
    let text = " PREFIX  Ctrl-^  —  .  palette   ·   q  quit   (any other key cancels) ";
    let mut style = Style::default();
    style.inverse = true;
    let w = cols as usize;
    let base = (rows - 1) as usize * w;
    for (i, ch) in text.chars().enumerate() {
        if i >= w {
            break;
        }
        cur[base + i] = Cell {
            ch,
            style,
            width: 1,
            extra: Vec::new(),
            hyperlink: 0,
        };
    }
}

/// Emit the minimal escape stream to turn `prev` into `cur`: per row, repaint
/// from the first changed column to the last, reusing posh_term::sgr_params for
/// styling. Cursor-positions absolutely, so it does not disturb other rows.
fn diff(prev: &[Cell], cur: &[Cell], rows: u16, cols: u16, out: &mut Vec<u8>) {
    let w = cols as usize;
    for r in 0..rows as usize {
        let base = r * w;
        let mut first = None;
        let mut last = 0;
        for c in 0..w {
            if !cells_eq(&prev[base + c], &cur[base + c]) {
                if first.is_none() {
                    first = Some(c);
                }
                last = c;
            }
        }
        let Some(first) = first else { continue };

        out.extend_from_slice(format!("\x1b[{};{}H\x1b[0m", r + 1, first + 1).as_bytes());
        let mut style = Style::default();
        for c in first..=last {
            let cell = &cur[base + c];
            if cell.width == 0 {
                continue; // trailing column of a wide char
            }
            if cell.style != style {
                out.extend_from_slice(format!("\x1b[{}m", sgr_params(&cell.style)).as_bytes());
                style = cell.style;
            }
            let mut b = [0u8; 4];
            let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
            out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
            for &e in &cell.extra {
                out.extend_from_slice(e.encode_utf8(&mut b).as_bytes());
            }
        }
        out.extend_from_slice(b"\x1b[0m");
    }
}

fn cells_eq(a: &Cell, b: &Cell) -> bool {
    a.ch == b.ch && a.style == b.style && a.width == b.width && a.extra == b.extra && a.hyperlink == b.hyperlink
}

// ---------------------------------------------------------------------------
// Self-test: the deterministic, headless PASS/FAIL path.
// ---------------------------------------------------------------------------

fn selftest(bin: &CString) -> i32 {
    let hosting_ok = test_command_bar(bin);
    let quit_ok = test_quit_command(bin);
    let composite_ok = test_composite(bin);
    let chord_ok = test_chord();
    if hosting_ok && quit_ok && composite_ok && chord_ok {
        println!("PASS: posh_term hosted the command bar, Quit exits the driver, the popup composites centered over the session, and the chord state machine maps correctly");
        0
    } else {
        println!("FAIL: hosting_ok={hosting_ok} quit_ok={quit_ok} composite_ok={composite_ok} chord_ok={chord_ok}");
        1
    }
}

/// Spawn the command bar on a PTY, then assert through the emulated screen: it
/// renders the palette, typing filters it, and Enter runs a (non-Quit)
/// selection, which echoes `ran: <name>` and closes the overlay.
fn test_command_bar(bin: &CString) -> bool {
    let (master, pid) = spawn_on_pty(bin, DEFAULT_ROWS, DEFAULT_COLS);
    let mut term = Terminal::new(DEFAULT_ROWS, DEFAULT_COLS);

    drain_until_idle(master, &mut term, IDLE);
    let initial = term.dump_text();
    let shows_bar = initial.contains("Commands") && initial.contains("New Session");
    eprintln!("--- command bar (initial) ---\n{initial}\n-----------------------------");

    // Type "model" to filter the list down to "Switch Model".
    write_all(master, b"model");
    drain_until_idle(master, &mut term, IDLE);
    let filtered = term.dump_text();
    let filtered_ok = filtered.contains("Switch Model") && !filtered.contains("New Session");
    eprintln!("--- after filter \"model\" ---\n{filtered}\n----------------------------");

    // Enter runs the selection; the program echoes "ran: Switch Model".
    write_all(master, b"\r");
    drain_until_idle(master, &mut term, IDLE);
    let ran = term.dump_text();
    let ran_ok = ran.contains("ran: Switch Model");
    eprintln!("--- after Enter ---\n{ran}\n-------------------");

    wait_for(pid, true);
    shows_bar && filtered_ok && ran_ok
}

/// Spawn the command bar, filter to "Quit", press Enter, and assert the bar
/// exits with the sentinel status that tells the host to quit the driver.
fn test_quit_command(bin: &CString) -> bool {
    let (master, pid) = spawn_on_pty(bin, DEFAULT_ROWS, DEFAULT_COLS);
    let mut term = Terminal::new(DEFAULT_ROWS, DEFAULT_COLS);

    drain_until_idle(master, &mut term, IDLE);
    write_all(master, b"quit");
    drain_until_idle(master, &mut term, IDLE);
    write_all(master, b"\r");
    drain_until_idle(master, &mut term, IDLE);

    let code = wait_for(pid, false);
    let ok = code == Some(QUIT_SENTINEL);
    eprintln!("--- quit command ---\nexit code = {code:?} (want {QUIT_SENTINEL})\n--------------------");
    ok
}

/// Compose the bar over the session background and assert the popup is centered
/// (doesn't start at column 0) and the background shows above it.
fn test_composite(bin: &CString) -> bool {
    let (master, pid) = spawn_on_pty(bin, DEFAULT_ROWS, DEFAULT_COLS);
    let mut bar = Terminal::new(DEFAULT_ROWS, DEFAULT_COLS);
    drain_until_idle(master, &mut bar, IDLE);
    wait_for(pid, true);

    let mut session = Terminal::new(DEFAULT_ROWS, DEFAULT_COLS);
    session.process(BASE_SCREEN);

    let cur = compose(DEFAULT_ROWS, DEFAULT_COLS, &session, Some(&bar), false);
    let joined: Vec<String> = (0..DEFAULT_ROWS).map(|r| row_text(&cur, r, DEFAULT_COLS)).collect();

    // Background preserved above the popup: the top row is the session.
    let bg_ok = joined[0].contains("posh client");
    // The composed grid carries the palette.
    let popup_ok = joined.join("\n").contains("Commands") && joined.join("\n").contains("New Session");
    // Horizontally centered: the popup's bbox does not start at column 0.
    let centered = match bbox(bar.screen()) {
        Some((_, c0, _, c1)) => DEFAULT_COLS.saturating_sub(c1 - c0 + 1) / 2 > 0,
        None => false,
    };
    // Top anchored a third of the way down: the popup's top border sits on that
    // row and the row above it is still background.
    let anchor = (DEFAULT_ROWS / 3) as usize;
    let anchored = joined[anchor].contains('╭') && !joined[anchor - 1].contains('╭');
    eprintln!(
        "--- composite ---\nbg_ok={bg_ok} popup_ok={popup_ok} centered={centered} anchored={anchored}\ntop=\"{}\"  anchor[{anchor}]=\"{}\"\n-----------------",
        joined[0], joined[anchor]
    );
    bg_ok && popup_ok && centered && anchored
}

/// Assert the chord parser: ordinary bytes pass through, `Ctrl-^ .` opens,
/// `Ctrl-^ q` quits, `Ctrl-^ Ctrl-^` forwards a literal Ctrl-^.
fn test_chord() -> bool {
    let ordinary = Chord::new().feed(b'a');

    let mut c = Chord::new();
    let open = (c.feed(CHORD_PREFIX), c.feed(CHORD_OPEN));

    let mut c = Chord::new();
    let quit = (c.feed(CHORD_PREFIX), c.feed(CHORD_QUIT));

    let mut c = Chord::new();
    let literal = (c.feed(CHORD_PREFIX), c.feed(CHORD_PREFIX));

    let pass = ordinary == Action::Forward(vec![b'a'])
        && open == (Action::Pending, Action::OpenOverlay)
        && quit == (Action::Pending, Action::Quit)
        && literal == (Action::Pending, Action::Forward(vec![CHORD_PREFIX]));
    eprintln!("--- chord ---\nordinary={ordinary:?}\nopen={open:?}\nquit={quit:?}\nliteral={literal:?}\n-------------");
    pass
}

fn row_text(cur: &[Cell], r: u16, cols: u16) -> String {
    let w = cols as usize;
    let base = r as usize * w;
    let mut s = String::new();
    for c in 0..w {
        let ch = cur[base + c].ch;
        s.push(if ch == '\0' { ' ' } else { ch });
    }
    s.trim_end().to_string()
}

/// Read from `master` into `term` until no bytes arrive for `idle`, or the
/// child closes the PTY. Query replies the emulator generates are written back
/// so the child does not stall waiting for them.
fn drain_until_idle(master: libc::c_int, term: &mut Terminal, idle: Duration) {
    let mut buf = [0u8; 8192];
    loop {
        if !poll_readable(master, idle) {
            break;
        }
        let n = read_fd(master, &mut buf);
        if n <= 0 {
            break;
        }
        term.process(&buf[..n as usize]);
        let replies = term.take_responses();
        if !replies.is_empty() {
            write_all(master, &replies);
        }
    }
}

// ---------------------------------------------------------------------------
// Interactive: session screen + chord-summoned popup (for a human to drive).
// ---------------------------------------------------------------------------

fn interactive(bin: &CString) {
    let (rows, cols) = term_size(STDOUT).unwrap_or((DEFAULT_ROWS, DEFAULT_COLS));
    let _raw = RawGuard::enable(STDIN);
    // Alt screen, clear, hide cursor (the popup carries its own visible state).
    write_all(STDOUT, b"\x1b[?1049h\x1b[2J\x1b[?25l");

    let mut session = Terminal::new(rows, cols);
    session.process(BASE_SCREEN);

    let mut pres = Presenter::new(rows, cols);
    let mut chord = Chord::new();
    let mut armed = false;
    pres.flush(&session, None, armed);

    let mut buf = [0u8; 8192];
    'session: loop {
        if !poll_readable(STDIN, Duration::from_millis(250)) {
            continue;
        }
        let n = read_fd(STDIN, &mut buf);
        if n <= 0 {
            break;
        }
        let mut open = false;
        for &b in &buf[..n as usize] {
            match chord.feed(b) {
                Action::Quit => break 'session,
                Action::OpenOverlay => open = true,
                Action::Forward(bytes) => {
                    if bytes == [SLASH] {
                        open = true;
                    }
                }
                Action::Pending => {}
            }
        }
        if chord.armed != armed {
            armed = chord.armed;
            pres.flush(&session, None, armed);
        }
        if open {
            let quit = run_overlay(bin, rows, cols, &session, &mut pres);
            chord = Chord::new();
            armed = false;
            if quit {
                break 'session;
            }
            pres.flush(&session, None, false); // remove the popup -> reveal session
        }
    }

    write_all(STDOUT, b"\x1b[?25h\x1b[?1049l");
}

/// Spawn the bar on a PTY and composite it as a popup over `session` until it
/// exits. Returns true if the bar asked the driver to quit (QUIT_SENTINEL).
fn run_overlay(bin: &CString, rows: u16, cols: u16, session: &Terminal, pres: &mut Presenter) -> bool {
    let (master, pid) = spawn_on_pty(bin, rows, cols);
    let mut bar = Terminal::new(rows, cols);

    let mut fds = [
        libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: STDIN, events: libc::POLLIN, revents: 0 },
    ];
    let mut buf = [0u8; 8192];
    let mut last_gen = u64::MAX;
    let mut exited = false;

    loop {
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, 50) };
        if r < 0 {
            break;
        }
        if fds[0].revents != 0 {
            let n = read_fd(master, &mut buf);
            if n <= 0 {
                exited = true;
                break;
            }
            bar.process(&buf[..n as usize]);
            let replies = bar.take_responses();
            if !replies.is_empty() {
                write_all(master, &replies);
            }
        }
        if fds[1].revents & libc::POLLIN != 0 {
            let n = read_fd(STDIN, &mut buf);
            if n > 0 {
                write_all(master, &buf[..n as usize]); // input sink -> bar
            }
        }
        if bar.generation() != last_gen {
            last_gen = bar.generation();
            pres.flush(session, Some(&bar), false);
        }
        fds[0].revents = 0;
        fds[1].revents = 0;
    }

    let code = wait_for(pid, !exited);
    code == Some(QUIT_SENTINEL)
}

// ---------------------------------------------------------------------------
// libc/PTY glue — the only unsafe code in the POC.
// ---------------------------------------------------------------------------

/// Spawn `bin` as the foreground process of a fresh PTY of the given size.
fn spawn_on_pty(bin: &CString, rows: u16, cols: u16) -> (libc::c_int, libc::pid_t) {
    let mut master: libc::c_int = -1;
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pid = unsafe {
        libc::forkpty(
            &mut master as *mut libc::c_int,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws as *const libc::winsize,
        )
    };
    match pid {
        -1 => panic!("forkpty failed: {}", std::io::Error::last_os_error()),
        0 => {
            let argv = [bin.as_ptr(), std::ptr::null()];
            unsafe {
                libc::execv(bin.as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }
        _ => (master, pid),
    }
}

/// Wait for the child to terminate and return its exit code (None if it was
/// killed by a signal). With `terminate`, SIGTERM is sent first — cleanup for a
/// child that may still be running; without it, the natural exit code is read
/// (the child is expected to have already exited on its own).
fn wait_for(pid: libc::pid_t, terminate: bool) -> Option<i32> {
    unsafe {
        if terminate {
            libc::kill(pid, libc::SIGTERM);
        }
        let mut status: libc::c_int = 0;
        if libc::waitpid(pid, &mut status as *mut libc::c_int, 0) != pid {
            return None;
        }
        if libc::WIFEXITED(status) {
            Some(libc::WEXITSTATUS(status))
        } else {
            None
        }
    }
}

fn read_fd(fd: libc::c_int, buf: &mut [u8]) -> isize {
    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) }
}

fn write_all(fd: libc::c_int, mut buf: &[u8]) {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
        buf = &buf[n as usize..];
    }
}

fn poll_readable(fd: libc::c_int, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    let r = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, ms) };
    r > 0 && (pfd.revents & libc::POLLIN) != 0
}

fn isatty(fd: libc::c_int) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

fn term_size(fd: libc::c_int) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws as *mut libc::winsize) };
    if r == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
    }
}

/// Puts a tty into raw mode and restores the original termios on drop.
struct RawGuard {
    fd: libc::c_int,
    orig: libc::termios,
}

impl RawGuard {
    fn enable(fd: libc::c_int) -> RawGuard {
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        unsafe { libc::tcgetattr(fd, &mut orig as *mut libc::termios) };
        let mut raw = orig;
        unsafe {
            libc::cfmakeraw(&mut raw as *mut libc::termios);
            libc::tcsetattr(fd, libc::TCSANOW, &raw as *const libc::termios);
        }
        RawGuard { fd, orig }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig as *const libc::termios) };
    }
}
