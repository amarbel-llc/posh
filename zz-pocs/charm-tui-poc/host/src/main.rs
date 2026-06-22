//! charm-tui-host: a chord-summoned command-palette overlay composited over a
//! REAL local shell running in a `posh_term::Terminal` — the same client-side
//! emulator the posh roaming client drives (`server_term`, fed by the remote
//! shell). The chord+overlay is factored into an `Overlay` component whose
//! interface matches what `remote/client.rs` provides, so lifting it into the
//! real client is a swap (local shell -> `server_term`), not a rewrite.
//! Throwaway POC: hardcoded constants, no flags. All libc/PTY FFI is confined
//! here so posh-term stays 100% safe.
//!
//! Session: `$SHELL` on a PTY feeds `session: Terminal`; the host renders it to
//! the real terminal with a per-cell diff and routes stdin to it. `Ctrl-^` opens
//! the palette (a bubbletea renderer on a PTY + a JSON-RPC control socket on its
//! fd 3); while open the session greys and stdin goes to the palette. Each
//! command carries a JSON-RPC `action` the renderer echoes back; the host
//! dispatches it. Logging/echo are mocks surfaced via a transient banner (the
//! POC stand-in for the client's `NotificationEngine`) until lifted into posh,
//! where `echo.set` -> `predict::build` swap and `logging.toggle` ->
//! `util::log_enable/disable`. SIGWINCH resizes the shell + session live.
//!
//! stdout a tty -> interactive; not a tty -> headless self-test (PASS/FAIL).

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
/// Ctrl-^ opens the palette (matches remote/client.rs ESCAPE_KEY). A bare "/" is
/// deliberately not a trigger — too common a character for an escape hatch.
const CTRL_CARET: u8 = 0x1e;

fn is_open_trigger(b: u8) -> bool {
    b == CTRL_CARET
}

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
// Mock client state + transient banner (POC stand-ins for the real client).
// ---------------------------------------------------------------------------

/// POC-local stand-in for the client settings the palette would drive. The real
/// client swaps this for `predict::build()` / `util::log_enable` on lift.
struct MockState {
    logging: bool,
    echo: String,
}

impl Default for MockState {
    fn default() -> MockState {
        MockState {
            logging: false,
            echo: "Adaptive".to_string(),
        }
    }
}

/// A transient top-row status message — the POC stand-in for the client's
/// `NotificationEngine` (remote/display.rs). Auto-clears after a short window.
struct Banner {
    text: String,
    expires_at: Option<Instant>,
}

impl Banner {
    fn new() -> Banner {
        Banner {
            text: String::new(),
            expires_at: None,
        }
    }

    fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.expires_at = Some(Instant::now() + Duration::from_millis(1500));
    }

    /// Clear if expired; returns true if it changed (so the host recomposites).
    fn tick(&mut self) -> bool {
        if matches!(self.expires_at, Some(t) if Instant::now() >= t) {
            self.text.clear();
            self.expires_at = None;
            return true;
        }
        false
    }

    fn active(&self) -> bool {
        !self.text.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Palette commands + JSON-RPC control protocol.
// ---------------------------------------------------------------------------

/// The host-supported palette commands. Each carries a JSON-RPC `action` — the
/// request the renderer echoes back when chosen — so selections flow as a small
/// JSON-RPC API (dispatched in `dispatch_rpc`), the same method surface a remote
/// peer could service over the wire (#3). Logging/echo are POC mocks.
fn palette_commands() -> Vec<serde_json::Value> {
    let echo = |m: &str| {
        serde_json::json!({
            "name": format!("Predictive echo: {m}"),
            "action": {"method": "echo.set", "params": {"model": m}},
        })
    };
    vec![
        serde_json::json!({"name":"Toggle debug logging","action":{"method":"logging.toggle"}}),
        echo("Adaptive"),
        echo("Optimistic"),
        echo("Always"),
        echo("Never"),
        serde_json::json!({"name":"Quit","action":{"method":"app.quit"}}),
    ]
}

/// Dispatch a JSON-RPC request from the palette against the mock state, raising a
/// banner. Returns true if the driver should quit. The real client handles these
/// same methods for real (echo.set -> predict::build swap; logging.toggle ->
/// util::log_enable/disable).
fn dispatch_rpc(method: &str, params: &serde_json::Value, state: &mut MockState, banner: &mut Banner) -> bool {
    match method {
        "app.quit" => return true,
        "logging.toggle" => {
            state.logging = !state.logging;
            banner.set(&format!("debug logging: {}", if state.logging { "on" } else { "off" }));
        }
        "echo.set" => {
            if let Some(m) = params.get("model").and_then(|v| v.as_str()) {
                state.echo = m.to_string();
                banner.set(&format!("predictive echo: {m}"));
            }
        }
        _ => {} // ui.cancel and unknowns: just close
    }
    false
}

fn parse_request(line: &str) -> Option<(String, serde_json::Value)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let method = v.get("method")?.as_str()?.to_string();
    let params = v.get("params").cloned().unwrap_or(serde_json::Value::Null);
    Some((method, params))
}

fn send_rpc(ctrl: libc::c_int, msg: &serde_json::Value) {
    let mut s = msg.to_string();
    s.push('\n');
    write_all(ctrl, s.as_bytes());
}

fn send_show_palette(ctrl: libc::c_int) {
    send_rpc(ctrl, &serde_json::json!({"method":"show","params":{"view":"palette","commands": palette_commands()}}));
}

// ---------------------------------------------------------------------------
// Overlay: the chord-summoned palette, factored for a clean lift into the real
// client. It owns the renderer process + the overlay emulator, but NOT the
// session emulator — the host (and, on lift, the client) owns that and asks the
// overlay to composite over it.
// ---------------------------------------------------------------------------

/// Outcome of pumping the control socket.
enum Event {
    Quit,
    Closed, // the palette was dismissed (a selection ran, or cancel)
    None,
}

struct Overlay {
    master: libc::c_int, // renderer PTY
    ctrl: libc::c_int,   // JSON-RPC control socket
    pid: libc::pid_t,
    rterm: Terminal, // the renderer's emulated screen (palette pixels)
    open: bool,
    ctrl_buf: Vec<u8>,
    state: MockState,
}

impl Overlay {
    fn spawn(bin: &CString, rows: u16, cols: u16) -> Overlay {
        let (master, ctrl, pid) = spawn_renderer(bin, rows, cols);
        Overlay {
            master,
            ctrl,
            pid,
            rterm: Terminal::new(rows, cols),
            open: false,
            ctrl_buf: Vec::new(),
            state: MockState::default(),
        }
    }

    fn is_open(&self) -> bool {
        self.open
    }

    /// The overlay screen to composite, or None when the palette is closed.
    fn screen(&self) -> Option<&Terminal> {
        self.open.then_some(&self.rterm)
    }

    fn open(&mut self) {
        self.open = true;
        send_show_palette(self.ctrl);
    }

    /// Drain the renderer PTY into `rterm`. Returns true if it drew (recomposite).
    fn pump(&mut self) -> bool {
        let mut buf = [0u8; 8192];
        let n = read_fd(self.master, &mut buf);
        if n <= 0 {
            return false;
        }
        let before = self.rterm.generation();
        self.rterm.process(&buf[..n as usize]);
        let replies = self.rterm.take_responses();
        if !replies.is_empty() {
            write_all(self.master, &replies);
        }
        self.rterm.generation() != before
    }

    fn forward_input(&self, bytes: &[u8]) {
        write_all(self.master, bytes);
    }

    /// Drain control-socket events, dispatch them, update the banner.
    fn poll_events(&mut self, banner: &mut Banner) -> Event {
        let mut buf = [0u8; 4096];
        let n = read_fd(self.ctrl, &mut buf);
        if n <= 0 {
            return Event::None;
        }
        self.ctrl_buf.extend_from_slice(&buf[..n as usize]);
        let mut outcome = Event::None;
        while let Some(pos) = self.ctrl_buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.ctrl_buf.drain(..=pos).collect();
            let Some((method, params)) = std::str::from_utf8(&line).ok().and_then(|s| parse_request(s.trim())) else {
                continue;
            };
            if dispatch_rpc(&method, &params, &mut self.state, banner) {
                return Event::Quit;
            }
            self.open = false; // any action (or ui.cancel) closes the palette
            outcome = Event::Closed;
        }
        outcome
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        set_winsize(self.master, rows, cols);
        self.rterm.resize(rows, cols);
    }

    fn shutdown(self) {
        shutdown_renderer(self.pid, self.ctrl);
    }
}

// ---------------------------------------------------------------------------
// Compositor: session background (greyed when the palette is open) + the
// anchored palette + a top-row banner, diffed to the real terminal.
// ---------------------------------------------------------------------------

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

    fn flush(&mut self, session: &Terminal, overlay: Option<&Terminal>, banner: &Banner) {
        let cur = compose(self.rows, self.cols, session, overlay, banner);
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

fn compose(rows: u16, cols: u16, session: &Terminal, overlay: Option<&Terminal>, banner: &Banner) -> Vec<Cell> {
    let w = cols as usize;
    let mut cur = vec![Cell::blank(Style::default()); rows as usize * w];

    // Session background, greyed while the palette is open.
    let grey = overlay.is_some();
    let sscr = session.screen();
    for r in 0..rows {
        for c in 0..cols {
            if let Some(cell) = sscr.cell(r, c) {
                cur[r as usize * w + c as usize] = if grey { dim_cell(cell) } else { cell.clone() };
            }
        }
    }

    // The palette, anchored a third of the way down, centered horizontally.
    if let Some(rend) = overlay {
        if let Some((r0, c0, r1, c1)) = bbox(rend.screen()) {
            let h = r1 - r0 + 1;
            let bw = c1 - c0 + 1;
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

    // Transient banner on the top row, on top of everything.
    if banner.active() {
        draw_banner(&mut cur, cols, &banner.text);
    }

    cur
}

/// Paint a reverse-video `posh: <text>` banner across the top row.
fn draw_banner(cur: &mut [Cell], cols: u16, text: &str) {
    let label: Vec<char> = format!(" posh: {text} ").chars().collect();
    let style = Style {
        inverse: true,
        bold: true,
        ..Style::default()
    };
    for c in 0..cols as usize {
        cur[c] = Cell {
            ch: label.get(c).copied().unwrap_or(' '),
            style,
            width: 1,
            extra: Vec::new(),
            hyperlink: 0,
        };
    }
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

/// Place the real cursor at the session's cursor when no overlay is up; hide it
/// while the palette is open (the palette has no mapped cursor — a follow-up).
fn update_cursor(session: &Terminal, overlay_open: bool) {
    if overlay_open {
        write_all(STDOUT, b"\x1b[?25l");
        return;
    }
    let c = session.cursor();
    let mut s = format!("\x1b[{};{}H", c.row + 1, c.col + 1).into_bytes();
    s.extend_from_slice(if c.visible { b"\x1b[?25h" } else { b"\x1b[?25l" });
    write_all(STDOUT, &s);
}

// ---------------------------------------------------------------------------
// Interactive: a real shell session + the chord-summoned palette overlay.
// ---------------------------------------------------------------------------

fn interactive(bin: &CString) {
    let (mut rows, mut cols) = term_size(STDOUT).unwrap_or((DEFAULT_ROWS, DEFAULT_COLS));
    let _raw = RawGuard::enable(STDIN);
    install_sigwinch();
    write_all(STDOUT, b"\x1b[?1049h\x1b[2J");

    let (shell_master, shell_pid) = spawn_shell(rows, cols);
    let mut session = Terminal::new(rows, cols);
    let mut overlay = Overlay::spawn(bin, rows, cols);
    let mut banner = Banner::new();
    let mut pres = Presenter::new(rows, cols);
    pres.flush(&session, None, &banner);
    update_cursor(&session, false);

    let mut buf = [0u8; 8192];
    'session: loop {
        let mut fds = [
            libc::pollfd { fd: shell_master, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: overlay.master, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: overlay.ctrl, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: STDIN, events: libc::POLLIN, revents: 0 },
        ];
        // EINTR (e.g. SIGWINCH) returns < 0 with revents untouched (all 0); fall
        // through to the resize check rather than breaking.
        unsafe { libc::poll(fds.as_mut_ptr(), 4, 50) };
        let mut dirty = false;

        if SIGWINCH.swap(false, Ordering::AcqRel) {
            if let Some((nr, nc)) = term_size(STDOUT) {
                rows = nr;
                cols = nc;
                set_winsize(shell_master, rows, cols);
                session.resize(rows, cols);
                overlay.resize(rows, cols);
                pres = Presenter::new(rows, cols);
                write_all(STDOUT, b"\x1b[2J");
                dirty = true;
            }
        }

        // Shell output -> session emulator.
        if fds[0].revents != 0 {
            let n = read_fd(shell_master, &mut buf);
            if n <= 0 {
                break 'session; // shell exited -> end the POC, back to the parent shell
            }
            session.process(&buf[..n as usize]);
            let replies = session.take_responses();
            if !replies.is_empty() {
                write_all(shell_master, &replies);
            }
            dirty = true;
        }

        // Renderer output -> overlay emulator.
        if fds[1].revents != 0 && overlay.pump() && overlay.is_open() {
            dirty = true;
        }

        // Control-socket events from the palette.
        if fds[2].revents & libc::POLLIN != 0 {
            match overlay.poll_events(&mut banner) {
                Event::Quit => break 'session,
                Event::Closed => dirty = true,
                Event::None => {}
            }
        }

        // Stdin: to the palette when open; chord opens it; otherwise to the shell.
        if fds[3].revents & libc::POLLIN != 0 {
            let n = read_fd(STDIN, &mut buf);
            if n <= 0 {
                break 'session;
            }
            if overlay.is_open() {
                overlay.forward_input(&buf[..n as usize]);
            } else if buf[..n as usize].iter().any(|&b| is_open_trigger(b)) {
                overlay.open();
                dirty = true; // grey the session immediately
            } else {
                write_all(shell_master, &buf[..n as usize]);
            }
        }

        if banner.tick() {
            dirty = true;
        }

        if dirty {
            pres.flush(&session, overlay.screen(), &banner);
            update_cursor(&session, overlay.is_open());
        }
    }

    write_all(STDOUT, b"\x1b[?25h\x1b[?1049l");
    overlay.shutdown();
    wait_for(shell_pid, true); // SIGKILL + reap the shell
}

// ---------------------------------------------------------------------------
// Self-test: the deterministic, headless PASS/FAIL path.
// ---------------------------------------------------------------------------

fn selftest(bin: &CString) -> i32 {
    let palette_ok = test_palette_rpc(bin);
    let compose_ok = test_compose(bin);
    let commands_ok = test_commands();
    let shutdown_ok = test_shutdown(bin);
    let trigger_ok = is_open_trigger(CTRL_CARET) && !is_open_trigger(b'/') && !is_open_trigger(b'a');
    if palette_ok && compose_ok && commands_ok && shutdown_ok && trigger_ok {
        println!("PASS: RPC palette+selection, compositor over a session (greyed/anchored + banner), command dispatch, graceful shutdown, and open trigger all hold");
        0
    } else {
        println!("FAIL: palette_ok={palette_ok} compose_ok={compose_ok} commands_ok={commands_ok} shutdown_ok={shutdown_ok} trigger_ok={trigger_ok}");
        1
    }
}

/// show palette over RPC, assert it renders the supported commands, then filter
/// + Enter and assert the renderer echoes back the right JSON-RPC action.
fn test_palette_rpc(bin: &CString) -> bool {
    let (master, ctrl, pid) = spawn_renderer(bin, DEFAULT_ROWS, DEFAULT_COLS);
    let mut term = Terminal::new(DEFAULT_ROWS, DEFAULT_COLS);
    drain_until_idle(master, &mut term, IDLE);

    send_show_palette(ctrl);
    drain_until_idle(master, &mut term, IDLE);
    let shown = term.dump_text();
    let shows = shown.contains("Commands") && shown.contains("Quit") && shown.contains("Toggle debug logging");
    eprintln!("--- palette (rpc) ---\n{shown}\n---------------------");

    write_all(master, b"optimistic"); // filter to "Predictive echo: Optimistic"
    drain_until_idle(master, &mut term, IDLE);
    write_all(master, b"\r");
    let req = read_request(ctrl, Duration::from_millis(800));
    let ev_ok = matches!(&req, Some((m, p))
        if m == "echo.set" && p.get("model").and_then(|v| v.as_str()) == Some("Optimistic"));
    eprintln!("--- palette request ---\n{req:?} (want echo.set / Optimistic)\n-----------------------");

    wait_for(pid, true);
    unsafe { libc::close(ctrl) };
    shows && ev_ok
}

/// Compose the palette over a (canned) session and assert: the session is greyed,
/// the palette is anchored a third down with the yellow border, and a banner
/// paints on the top row.
fn test_compose(bin: &CString) -> bool {
    let (master, ctrl, pid) = spawn_renderer(bin, DEFAULT_ROWS, DEFAULT_COLS);
    let mut rterm = Terminal::new(DEFAULT_ROWS, DEFAULT_COLS);
    drain_until_idle(master, &mut rterm, IDLE);

    let mut session = Terminal::new(DEFAULT_ROWS, DEFAULT_COLS);
    session.process(b"\x1b[2J\x1b[Hhello from the shell");

    send_show_palette(ctrl);
    drain_until_idle(master, &mut rterm, IDLE);

    // No banner: row 0 is the greyed session; the palette is anchored a third
    // down with its yellow double border.
    let plain = compose(DEFAULT_ROWS, DEFAULT_COLS, &session, Some(&rterm), &Banner::new());
    let rows: Vec<String> = (0..DEFAULT_ROWS).map(|r| row_text(&plain, r, DEFAULT_COLS)).collect();
    let anchor = (DEFAULT_ROWS / 3) as usize;
    let greyed = is_dimmed(&plain, 0, DEFAULT_COLS, 'h');
    let bg_above = rows[0].contains("hello from the shell");
    let popup = rows.join("\n").contains("Toggle debug logging") && rows.join("\n").contains("Quit");
    let anchored = rows[anchor].contains('╔');

    // With a banner active: the top row carries the reverse-video status.
    let mut banner = Banner::new();
    banner.set("predictive echo: Optimistic");
    let withbar = compose(DEFAULT_ROWS, DEFAULT_COLS, &session, Some(&rterm), &banner);
    let banner_ok = row_text(&withbar, 0, DEFAULT_COLS).contains("posh: predictive echo: Optimistic");

    let ok = greyed && bg_above && popup && anchored && banner_ok;
    eprintln!("--- compose ---\ngreyed={greyed} bg_above={bg_above} popup={popup} anchored={anchored} banner_ok={banner_ok}\n---------------");
    wait_for(pid, true);
    unsafe { libc::close(ctrl) };
    ok
}

/// Dispatch a few palette actions and assert the mock state updates and the
/// banner reflects the change, and that `app.quit` signals exit.
fn test_commands() -> bool {
    let mut state = MockState::default();
    let mut banner = Banner::new();

    dispatch_rpc("logging.toggle", &serde_json::Value::Null, &mut state, &mut banner);
    let logging_ok = state.logging && banner.text.contains("debug logging: on");
    dispatch_rpc("echo.set", &serde_json::json!({"model":"Optimistic"}), &mut state, &mut banner);
    let echo_ok = state.echo == "Optimistic" && banner.text.contains("Optimistic");
    let quit = dispatch_rpc("app.quit", &serde_json::Value::Null, &mut state, &mut banner);

    eprintln!("--- commands ---\nlogging_ok={logging_ok} echo_ok={echo_ok} quit={quit}\n----------------");
    logging_ok && echo_ok && quit
}

/// Send a `shutdown` request and assert the renderer exits on its own within the
/// grace window (the graceful path — `p.Kill` cancels its context, no SIGKILL).
fn test_shutdown(bin: &CString) -> bool {
    let (master, ctrl, pid) = spawn_renderer(bin, DEFAULT_ROWS, DEFAULT_COLS);
    let mut term = Terminal::new(DEFAULT_ROWS, DEFAULT_COLS);
    drain_until_idle(master, &mut term, IDLE);

    send_rpc(ctrl, &serde_json::json!({"method":"shutdown"}));
    let deadline = Instant::now() + Duration::from_millis(800);
    let mut graceful = false;
    while Instant::now() < deadline {
        let mut status: libc::c_int = 0;
        if unsafe { libc::waitpid(pid, &mut status as *mut libc::c_int, libc::WNOHANG) } == pid {
            graceful = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !graceful {
        wait_for(pid, true);
    }
    unsafe { libc::close(ctrl) };
    eprintln!("--- shutdown ---\ngraceful_exit={graceful}\n----------------");
    graceful
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

/// Read one JSON-RPC request line from the control socket within `timeout`.
fn read_request(ctrl: libc::c_int, timeout: Duration) -> Option<(String, serde_json::Value)> {
    if !poll_readable(ctrl, timeout) {
        return None;
    }
    let mut buf = [0u8; 4096];
    let n = read_fd(ctrl, &mut buf);
    if n <= 0 {
        return None;
    }
    let s = std::str::from_utf8(&buf[..n as usize]).ok()?;
    s.lines().find_map(|line| parse_request(line.trim()))
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
// libc/PTY glue — the only unsafe code in the POC.
// ---------------------------------------------------------------------------

static SIGWINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigwinch(_: libc::c_int) {
    SIGWINCH.store(true, Ordering::Release);
}

fn install_sigwinch() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigwinch as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask as *mut libc::sigset_t);
        sa.sa_flags = 0; // no SA_RESTART: poll returns EINTR promptly on resize
        libc::sigaction(libc::SIGWINCH, &sa as *const libc::sigaction, std::ptr::null_mut());
    }
}

fn set_winsize(fd: libc::c_int, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws as *const libc::winsize) };
}

/// Spawn `$SHELL` (fallback `/bin/sh`) on a fresh PTY. Returns (master, pid).
fn spawn_shell(rows: u16, cols: u16) -> (libc::c_int, libc::pid_t) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let shell_c = CString::new(shell).unwrap_or_else(|_| CString::new("/bin/sh").unwrap());
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
        -1 => panic!("forkpty (shell) failed: {}", std::io::Error::last_os_error()),
        0 => unsafe {
            let argv = [shell_c.as_ptr(), std::ptr::null()];
            libc::execv(shell_c.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        },
        _ => (master, pid),
    }
}

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

/// Wait for the child and return its exit code (None if killed by a signal).
/// With `terminate`, SIGKILL is sent first — bulletproof teardown.
fn wait_for(pid: libc::pid_t, terminate: bool) -> Option<i32> {
    unsafe {
        if terminate {
            libc::kill(pid, libc::SIGKILL);
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

/// Shut the renderer down: ask it to quit over the control channel (coordinated
/// via JSON-RPC), give it a grace period, then SIGKILL if it hasn't exited.
fn shutdown_renderer(pid: libc::pid_t, ctrl: libc::c_int) {
    send_rpc(ctrl, &serde_json::json!({"method":"shutdown"}));
    unsafe { libc::close(ctrl) };

    let deadline = Instant::now() + Duration::from_millis(300);
    loop {
        let mut status: libc::c_int = 0;
        if unsafe { libc::waitpid(pid, &mut status as *mut libc::c_int, libc::WNOHANG) } == pid {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    unsafe {
        libc::kill(pid, libc::SIGKILL);
        let mut status: libc::c_int = 0;
        libc::waitpid(pid, &mut status as *mut libc::c_int, 0);
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
