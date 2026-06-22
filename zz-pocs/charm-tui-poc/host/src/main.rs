//! charm-tui-host: drive a long-running bubbletea renderer (`tui/tui-bin`) over
//! a JSON-RPC-style control channel and composite its output over a retained
//! session screen — proving posh's client-side emulator can run a charmbracelet
//! renderer as a host-driven, mux-style overlay. Throwaway POC: hardcoded
//! constants, no flags. All libc/PTY FFI is confined here so posh-term stays
//! 100% safe.
//!
//! The renderer is spawned once on a PTY (visual channel) plus a control socket
//! on its fd 3. The host sends `show {view:"palette", commands}` / `hide`; the
//! renderer reports palette `selected`/`cancel` events back on the same socket.
//! `Ctrl-^` (or a bare `/`) opens the palette directly. The host owns input
//! routing (the open trigger handled here; keystrokes forwarded to the renderer
//! while the palette is up) and compositing: the palette is a popup anchored a
//! third of the way down over a greyed-out (dimmed) session background, painted
//! with a per-cell diff (reusing posh_term::sgr_params) that writes only the
//! cells that changed.
//!
//! Two behaviours chosen by whether stdout is a tty (an OS fact, not a flag):
//!   * stdout IS a tty  -> interactive.
//!   * stdout is NOT a tty -> self-test: exercise the RPC round-trip (palette
//!     show + selection event, chord view) and the compositor (anchored popup,
//!     greyed chord background), plus the chord parser. Print PASS/FAIL.

use std::ffi::CString;
use std::time::Duration;

use posh_term::{sgr_params, Cell, Color, Screen, Style, Terminal};

/// Path to the renderer binary, anchored at compile time to this crate's
/// manifest dir so it resolves regardless of the runtime CWD.
const TUI_BIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tui/tui-bin");
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
const STDIN: libc::c_int = 0;
const STDOUT: libc::c_int = 1;
/// Quiet period with no PTY output that counts as "the renderer finished drawing".
const IDLE: Duration = Duration::from_millis(400);

// Open triggers: Ctrl-^ (0x1e, matches remote/client.rs ESCAPE_KEY) or a bare
// "/" opens the command palette directly. Quit is the palette's own command.
const CTRL_CARET: u8 = 0x1e;
const SLASH: u8 = b'/';

fn is_open_trigger(b: u8) -> bool {
    b == CTRL_CARET || b == SLASH
}

/// The only host-supported commands. Sent to the renderer when the palette
/// opens; selecting one is handled below. (Unsupported demo commands removed.)
const COMMANDS: &[&str] = &["Quit", "Clear session", "Redraw session"];

/// The retained "live session" background screen the overlays composite over.
const BASE_SCREEN: &[u8] = b"\x1b[2J\x1b[H  posh client \xe2\x80\x94 live session (POC base screen)\r\n\r\n  \x1b[1m/\x1b[0m  or  \x1b[1mCtrl-^ .\x1b[0m   command palette\r\n  \x1b[1mCtrl-^ q\x1b[0m            quit\r\n";

fn main() {
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
// Control channel: JSON-RPC-style messages to/from the renderer.
// ---------------------------------------------------------------------------

fn send_rpc(ctrl: libc::c_int, msg: &serde_json::Value) {
    let mut s = msg.to_string();
    s.push('\n');
    write_all(ctrl, s.as_bytes());
}

fn send_show_palette(ctrl: libc::c_int) {
    let cmds: Vec<serde_json::Value> = COMMANDS
        .iter()
        .map(|n| serde_json::json!({ "name": n, "shortcut": "" }))
        .collect();
    send_rpc(ctrl, &serde_json::json!({"method":"show","params":{"view":"palette","commands": cmds}}));
}

fn send_hide(ctrl: libc::c_int) {
    send_rpc(ctrl, &serde_json::json!({"method":"hide","params":{}}));
}

/// Parse one renderer event line: returns (kind, command).
fn parse_event(line: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("method")?.as_str()? != "event" {
        return None;
    }
    let p = v.get("params")?;
    let kind = p.get("kind")?.as_str()?.to_string();
    let command = p.get("command").and_then(|c| c.as_str()).unwrap_or("").to_string();
    Some((kind, command))
}

// ---------------------------------------------------------------------------
// Compositor: session background + a centered/anchored overlay, with optional
// grey-out, diffed to the real terminal.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    None,
    Palette,
}

struct Presenter {
    rows: u16,
    cols: u16,
    prev: Vec<Cell>,
}

impl Presenter {
    fn new(rows: u16, cols: u16) -> Presenter {
        Presenter {
            rows,
            cols,
            prev: vec![Cell::blank(Style::default()); rows as usize * cols as usize],
        }
    }

    fn flush(&mut self, session: &Terminal, overlay: Option<&Terminal>, mode: Mode) {
        let cur = compose(self.rows, self.cols, session, overlay, mode);
        let mut body = Vec::new();
        diff(&self.prev, &cur, self.rows, self.cols, &mut body);
        if !body.is_empty() {
            let mut frame = Vec::with_capacity(body.len() + 16);
            frame.extend_from_slice(b"\x1b[?2026h");
            frame.extend_from_slice(&body);
            frame.extend_from_slice(b"\x1b[?2026l");
            write_all(STDOUT, &frame);
        }
        self.prev = cur;
    }
}

/// Collapse a cell to a dim mid-grey — the "greyed out" background look.
fn dim_cell(cell: &Cell) -> Cell {
    Cell {
        style: Style {
            fg: Color::Rgb(0x70, 0x70, 0x70),
            dim: true,
            ..Style::default()
        },
        ..cell.clone()
    }
}

fn compose(rows: u16, cols: u16, session: &Terminal, overlay: Option<&Terminal>, mode: Mode) -> Vec<Cell> {
    let w = cols as usize;
    let mut cur = vec![Cell::blank(Style::default()); rows as usize * w];

    let grey = mode == Mode::Palette;
    let sscr = session.screen();
    for r in 0..rows {
        for c in 0..cols {
            if let Some(cell) = sscr.cell(r, c) {
                cur[r as usize * w + c as usize] = if grey { dim_cell(cell) } else { cell.clone() };
            }
        }
    }

    if mode != Mode::None {
        if let Some(rend) = overlay {
            if let Some((r0, c0, r1, c1)) = bbox(rend.screen()) {
                let h = r1 - r0 + 1;
                let bw = c1 - c0 + 1;
                // Anchor a third of the way down (expands down / collapses up),
                // centered horizontally.
                let dr = rows / 3;
                let dc = cols.saturating_sub(bw) / 2;
                let rscr = rend.screen();
                for r in 0..h {
                    for c in 0..bw {
                        let (pr, pc) = (dr + r, dc + c);
                        if pr < rows && pc < cols {
                            if let Some(cell) = rscr.cell(r0 + r, c0 + c) {
                                cur[pr as usize * w + pc as usize] = cell.clone();
                            }
                        }
                    }
                }
            }
        }
    }

    cur
}

/// Non-blank bounding box of a screen: (top, left, bottom, right), or None.
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

/// Minimal escape stream to turn `prev` into `cur`: per row, repaint from the
/// first changed column to the last, reusing posh_term::sgr_params for styling.
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
                continue;
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
    let palette_ok = test_palette_rpc(bin);
    let compose_ok = test_compose(bin);
    let trigger_ok = is_open_trigger(CTRL_CARET) && is_open_trigger(SLASH) && !is_open_trigger(b'a');
    if palette_ok && compose_ok && trigger_ok {
        println!("PASS: RPC palette+selection, compositor (greyed anchored palette over the session), and open triggers all hold");
        0
    } else {
        println!("FAIL: palette_ok={palette_ok} compose_ok={compose_ok} trigger_ok={trigger_ok}");
        1
    }
}

/// show palette over RPC, assert it renders the supported commands, then filter
/// + Enter and assert the renderer reports the right `selected` event.
fn test_palette_rpc(bin: &CString) -> bool {
    let (master, ctrl, pid) = spawn_renderer(bin, DEFAULT_ROWS, DEFAULT_COLS);
    let mut term = Terminal::new(DEFAULT_ROWS, DEFAULT_COLS);
    drain_until_idle(master, &mut term, IDLE);

    send_show_palette(ctrl);
    drain_until_idle(master, &mut term, IDLE);
    let shown = term.dump_text();
    let shows = shown.contains("Commands") && shown.contains("Quit") && shown.contains("Clear session");
    eprintln!("--- palette (rpc) ---\n{shown}\n---------------------");

    write_all(master, b"clear"); // filter to "Clear session"
    drain_until_idle(master, &mut term, IDLE);
    write_all(master, b"\r");
    let ev = read_event(ctrl, Duration::from_millis(800));
    let ev_ok = ev == Some(("selected".to_string(), "Clear session".to_string()));
    eprintln!("--- palette event ---\n{ev:?} (want selected/Clear session)\n---------------------");

    wait_for(pid, true);
    unsafe { libc::close(ctrl) };
    shows && ev_ok
}

/// Compose the palette over a session background and assert: the background is
/// greyed, the palette is present and anchored a third down with the yellow
/// (double) border, and the greyed session text still shows above it.
fn test_compose(bin: &CString) -> bool {
    let (master, ctrl, pid) = spawn_renderer(bin, DEFAULT_ROWS, DEFAULT_COLS);
    let mut renderer = Terminal::new(DEFAULT_ROWS, DEFAULT_COLS);
    drain_until_idle(master, &mut renderer, IDLE);

    let mut session = Terminal::new(DEFAULT_ROWS, DEFAULT_COLS);
    session.process(BASE_SCREEN);

    send_show_palette(ctrl);
    drain_until_idle(master, &mut renderer, IDLE);
    let pal = compose(DEFAULT_ROWS, DEFAULT_COLS, &session, Some(&renderer), Mode::Palette);
    let rows: Vec<String> = (0..DEFAULT_ROWS).map(|r| row_text(&pal, r, DEFAULT_COLS)).collect();
    let anchor = (DEFAULT_ROWS / 3) as usize;

    let greyed = is_dimmed(&pal, 0, DEFAULT_COLS, 'p');
    let bg_above = rows[0].contains("posh client");
    let popup = rows.join("\n").contains("Commands") && rows.join("\n").contains("Quit");
    let anchored = rows[anchor].contains('╔'); // double (yellow) border top-left

    let ok = greyed && bg_above && popup && anchored;
    eprintln!("--- compose ---\ngreyed={greyed} bg_above={bg_above} popup={popup} anchored={anchored}\n---------------");
    wait_for(pid, true);
    unsafe { libc::close(ctrl) };
    ok
}

/// True if the first cell in `row` whose char is `ch` carries the dim style.
fn is_dimmed(grid: &[Cell], row: u16, cols: u16, ch: char) -> bool {
    let w = cols as usize;
    let base = row as usize * w;
    (0..w).find(|&c| grid[base + c].ch == ch).map(|c| grid[base + c].style.dim).unwrap_or(false)
}

fn row_text(grid: &[Cell], r: u16, cols: u16) -> String {
    let w = cols as usize;
    let base = r as usize * w;
    let mut s = String::new();
    for c in 0..w {
        let ch = grid[base + c].ch;
        s.push(if ch == '\0' { ' ' } else { ch });
    }
    s.trim_end().to_string()
}

/// Read one event line from the control socket within `timeout`.
fn read_event(ctrl: libc::c_int, timeout: Duration) -> Option<(String, String)> {
    if !poll_readable(ctrl, timeout) {
        return None;
    }
    let mut buf = [0u8; 4096];
    let n = read_fd(ctrl, &mut buf);
    if n <= 0 {
        return None;
    }
    let s = std::str::from_utf8(&buf[..n as usize]).ok()?;
    s.lines().find_map(|line| parse_event(line.trim()))
}

/// Read PTY output into `term` until idle or EOF, echoing query replies.
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
// Interactive: drive the renderer + composite (for a human).
// ---------------------------------------------------------------------------

fn interactive(bin: &CString) {
    let (rows, cols) = term_size(STDOUT).unwrap_or((DEFAULT_ROWS, DEFAULT_COLS));
    let _raw = RawGuard::enable(STDIN);
    write_all(STDOUT, b"\x1b[?1049h\x1b[2J\x1b[?25l");

    let mut session = Terminal::new(rows, cols);
    session.process(BASE_SCREEN);

    let (master, ctrl, pid) = spawn_renderer(bin, rows, cols);
    let mut renderer = Terminal::new(rows, cols);
    let mut pres = Presenter::new(rows, cols);
    let mut mode = Mode::None;
    pres.flush(&session, None, mode);

    let mut fds = [
        libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: ctrl, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: STDIN, events: libc::POLLIN, revents: 0 },
    ];
    let mut buf = [0u8; 8192];
    let mut ctrl_buf: Vec<u8> = Vec::new();
    let mut last_gen = u64::MAX;

    'session: loop {
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 3, 50) };
        if r < 0 {
            break;
        }

        // renderer PTY -> renderer emulator
        if fds[0].revents != 0 {
            let n = read_fd(master, &mut buf);
            if n <= 0 {
                break;
            }
            renderer.process(&buf[..n as usize]);
            let replies = renderer.take_responses();
            if !replies.is_empty() {
                write_all(master, &replies);
            }
        }

        // control socket -> renderer events
        if fds[1].revents & libc::POLLIN != 0 {
            let n = read_fd(ctrl, &mut buf);
            if n <= 0 {
                break;
            }
            ctrl_buf.extend_from_slice(&buf[..n as usize]);
            while let Some(pos) = ctrl_buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = ctrl_buf.drain(..=pos).collect();
                let Some((kind, command)) = std::str::from_utf8(&line).ok().and_then(|s| parse_event(s.trim())) else {
                    continue;
                };
                match kind.as_str() {
                    "selected" => match command.as_str() {
                        "Quit" => break 'session,
                        "Clear session" => session.process(b"\x1b[2J"),
                        "Redraw session" => {
                            session = Terminal::new(rows, cols);
                            session.process(BASE_SCREEN);
                        }
                        _ => {}
                    },
                    _ => {}
                }
                mode = Mode::None;
                pres.flush(&session, None, mode);
            }
        }

        // stdin -> chord (or forwarded to the palette)
        if fds[2].revents & libc::POLLIN != 0 {
            let n = read_fd(STDIN, &mut buf);
            if n <= 0 {
                break;
            }
            if mode == Mode::Palette {
                write_all(master, &buf[..n as usize]); // forward keystrokes to the palette
            } else if buf[..n as usize].iter().any(|&b| is_open_trigger(b)) {
                mode = Mode::Palette;
                send_show_palette(ctrl);
                pres.flush(&session, Some(&renderer), mode); // grey the session immediately
            }
        }

        // re-composite when the renderer redraws an active overlay
        if renderer.generation() != last_gen {
            last_gen = renderer.generation();
            if mode != Mode::None {
                pres.flush(&session, Some(&renderer), mode);
            }
        }

        for f in &mut fds {
            f.revents = 0;
        }
    }

    send_hide(ctrl);
    unsafe { libc::close(ctrl) };
    write_all(STDOUT, b"\x1b[?25h\x1b[?1049l");
    wait_for(pid, true);
}

// ---------------------------------------------------------------------------
// libc/PTY glue — the only unsafe code in the POC.
// ---------------------------------------------------------------------------

/// Spawn `bin` on a fresh PTY plus a control socket routed to its fd 3.
/// Returns (pty_master, host_control_fd, pid).
fn spawn_renderer(bin: &CString, rows: u16, cols: u16) -> (libc::c_int, libc::c_int, libc::pid_t) {
    let mut sp: [libc::c_int; 2] = [-1, -1];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sp.as_mut_ptr()) } != 0 {
        panic!("socketpair failed: {}", std::io::Error::last_os_error());
    }
    let (host_ctrl, child_ctrl) = (sp[0], sp[1]);

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
        0 => unsafe {
            libc::close(host_ctrl);
            if child_ctrl != 3 {
                libc::dup2(child_ctrl, 3);
                libc::close(child_ctrl);
            }
            let argv = [bin.as_ptr(), std::ptr::null()];
            libc::execv(bin.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        },
        _ => {
            unsafe { libc::close(child_ctrl) };
            (master, host_ctrl, pid)
        }
    }
}

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
