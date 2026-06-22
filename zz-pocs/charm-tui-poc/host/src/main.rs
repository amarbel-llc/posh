//! charm-tui-host: spawn the bubbletea POC binary (`tui/tui-bin`, now a v2
//! command palette modeled on trapeze's "/" Commands dialog) on a PTY and host
//! it on a `posh_term::Terminal`, popped up as a chord-summoned overlay —
//! proving posh's client-side emulator can summon and render an arbitrary
//! charmbracelet TUI. Throwaway POC: hardcoded constants, no flags, no config.
//! All libc/PTY FFI is confined here so posh-term stays 100% safe.
//!
//! This mirrors the server-side escape-to-shell overlay (FDR 0008) but
//! client-side and local: a base "session" screen runs underneath, a chord (or
//! a bare "/", trapeze's native trigger) swaps the input sink + render source to
//! the command bar, and dismissing it (Esc) restores the base.
//!
//! When the Ctrl-^ prefix is armed (awaiting its second key), a reverse-video
//! status line is shown so the chord state is legible — the prior demo gave no
//! hint, which made it hard to discover how to act/exit.
//!
//! One binary, two behaviours chosen by whether stdout is a tty (an OS fact,
//! not a flag):
//!   * stdout IS a tty  -> interactive: base screen + chord/"/" -> command bar.
//!   * stdout is NOT a tty -> self-test: assert (a) posh_term renders the hosted
//!     command bar (title + commands, filtering, selection) and (b) the chord
//!     state machine maps correctly. Print PASS/FAIL, exit 0/1.

use std::ffi::CString;
use std::time::Duration;

use posh_term::Terminal;

/// Path to the bubbletea binary, anchored at compile time to this crate's
/// manifest dir so it resolves regardless of the runtime CWD.
const TUI_BIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tui/tui-bin");
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
const STDIN: libc::c_int = 0;
const STDOUT: libc::c_int = 1;
/// Quiet period with no PTY output that counts as "the TUI finished drawing".
const IDLE: Duration = Duration::from_millis(400);

// Chord: Ctrl-^ (0x1e) prefix + a key, matching posh's existing escape chord
// (remote/client.rs ESCAPE_KEY). `Ctrl-^ .` is the reachable stand-in for the
// eventual `Ctrl-.` (a bare Ctrl-. is not a control byte and needs the kitty /
// CSI-u keyboard protocol to report — deferred).
const CHORD_PREFIX: u8 = 0x1e; // Ctrl-^
const CHORD_OPEN: u8 = b'.'; // Ctrl-^ .  -> summon the command bar
const CHORD_QUIT: u8 = b'q'; // Ctrl-^ q  -> quit the driver
const SLASH: u8 = b'/'; // bare "/" also summons (trapeze's native trigger)

fn main() {
    // Make the child's color/term detection deterministic and non-blocking.
    std::env::set_var("TERM", "xterm-256color");
    std::env::set_var("COLORTERM", "truecolor");

    let bin = std::fs::canonicalize(TUI_BIN)
        .unwrap_or_else(|e| panic!("cannot find {TUI_BIN}: {e}"));
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
// Self-test: the deterministic, headless PASS/FAIL path.
// ---------------------------------------------------------------------------

fn selftest(bin: &CString) -> i32 {
    let hosting_ok = test_command_bar(bin);
    let chord_ok = test_chord();
    if hosting_ok && chord_ok {
        println!("PASS: posh_term hosted the bubbletea command bar and the chord state machine maps correctly");
        0
    } else {
        println!("FAIL: hosting_ok={hosting_ok} chord_ok={chord_ok}");
        1
    }
}

/// Spawn the command bar on a PTY, then assert through the emulated screen:
/// it renders the palette, typing filters it, and Enter runs the selection.
fn test_command_bar(bin: &CString) -> bool {
    let (master, pid) = spawn_on_pty(bin, DEFAULT_ROWS, DEFAULT_COLS);
    let mut term = Terminal::new(DEFAULT_ROWS, DEFAULT_COLS);

    drain_until_idle(master, &mut term, IDLE);
    let initial = term.dump_text();
    let shows_bar = initial.contains("Commands") && initial.contains("New Session");
    eprintln!("--- command bar (initial) ---\n{initial}\n-----------------------------");

    // Type "quit" to filter the list down to the Quit command.
    write_all(master, b"quit");
    drain_until_idle(master, &mut term, IDLE);
    let filtered = term.dump_text();
    let filtered_ok = filtered.contains("Quit") && !filtered.contains("New Session");
    eprintln!("--- after filter \"quit\" ---\n{filtered}\n---------------------------");

    // Enter runs the selected command; the program echoes "ran: Quit" and exits.
    write_all(master, b"\r");
    drain_until_idle(master, &mut term, IDLE);
    let ran = term.dump_text();
    let ran_ok = ran.contains("ran: Quit");
    eprintln!("--- after Enter ---\n{ran}\n-------------------");

    reap(pid);
    shows_bar && filtered_ok && ran_ok
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
// Interactive: base screen + chord-summoned command bar (for a human to drive).
// ---------------------------------------------------------------------------

fn interactive(bin: &CString) {
    let (rows, cols) = term_size(STDOUT).unwrap_or((DEFAULT_ROWS, DEFAULT_COLS));
    let _raw = RawGuard::enable(STDIN);
    write_all(STDOUT, b"\x1b[?1049h"); // alt screen: restore the user's view on exit

    draw_base();
    let mut chord = Chord::new();
    let mut armed = false;
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
                    // Other forwarded bytes would reach the session; the POC
                    // base screen has nothing underneath, so they are dropped.
                }
                Action::Pending => {}
            }
        }
        // Reflect the chord-armed state in the status line.
        if chord.armed != armed {
            armed = chord.armed;
            render_armed(rows, armed);
        }
        if open {
            host_overlay(bin, rows, cols);
            draw_base();
            armed = false;
        }
    }

    write_all(STDOUT, b"\x1b[?1049l");
}

/// Draw the stand-in "live session" base screen.
fn draw_base() {
    let mut s = Vec::new();
    s.extend_from_slice(b"\x1b[2J\x1b[H");
    s.extend_from_slice(b"  posh client \xe2\x80\x94 live session (POC base screen)\r\n\r\n");
    s.extend_from_slice(b"  \x1b[1m/\x1b[0m  or  \x1b[1mCtrl-^ .\x1b[0m   command palette\r\n");
    s.extend_from_slice(b"  \x1b[1mCtrl-^ q\x1b[0m            quit\r\n");
    write_all(STDOUT, &s);
}

/// Show or clear a reverse-video status line on the bottom row indicating that
/// the Ctrl-^ prefix is armed and awaiting its second key. Cursor is saved and
/// restored so the base screen is undisturbed.
fn render_armed(rows: u16, armed: bool) {
    let mut s = Vec::new();
    s.extend_from_slice(b"\x1b7"); // DECSC: save cursor
    s.extend_from_slice(format!("\x1b[{rows};1H\x1b[2K").as_bytes());
    if armed {
        s.extend_from_slice(
            "\x1b[7m PREFIX  Ctrl-^  —  press  .  palette   ·   q  quit   (any other key cancels) \x1b[0m"
                .as_bytes(),
        );
    }
    s.extend_from_slice(b"\x1b8"); // DECRC: restore cursor
    write_all(STDOUT, &s);
}

/// Spawn the TUI on a PTY and host it on a posh_term::Terminal, rendering to
/// the real terminal and forwarding keystrokes, until the overlay exits.
fn host_overlay(bin: &CString, rows: u16, cols: u16) {
    let (master, pid) = spawn_on_pty(bin, rows, cols);
    let mut term = Terminal::new(rows, cols);

    let mut fds = [
        libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: STDIN, events: libc::POLLIN, revents: 0 },
    ];
    let mut buf = [0u8; 8192];
    let mut last_gen = u64::MAX;

    loop {
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, 50) };
        if r < 0 {
            break;
        }
        if fds[0].revents != 0 {
            let n = read_fd(master, &mut buf);
            if n <= 0 {
                break; // overlay process exited
            }
            term.process(&buf[..n as usize]);
            let replies = term.take_responses();
            if !replies.is_empty() {
                write_all(master, &replies);
            }
        }
        if fds[1].revents & libc::POLLIN != 0 {
            let n = read_fd(STDIN, &mut buf);
            if n > 0 {
                write_all(master, &buf[..n as usize]); // input sink -> overlay
            }
        }
        if term.generation() != last_gen {
            last_gen = term.generation();
            let mut frame = Vec::with_capacity(4096);
            frame.extend_from_slice(b"\x1b[H");
            frame.extend_from_slice(&term.dump_vt());
            write_all(STDOUT, &frame);
        }
        fds[0].revents = 0;
        fds[1].revents = 0;
    }

    reap(pid);
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

fn reap(pid: libc::pid_t) {
    unsafe {
        libc::kill(pid, libc::SIGTERM);
        let mut status: libc::c_int = 0;
        libc::waitpid(pid, &mut status, 0);
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
