//! Client-side command-palette overlay (#2): hosts the `posh-palette` renderer
//! subprocess and drives it over the RFC 0005 JSON-RPC control channel.
//!
//! The renderer draws the palette to a PTY whose emulated screen this module
//! tracks (`screen()`); the client composites that onto the live session view.
//! Selections arrive on the control socket as JSON-RPC requests this module
//! surfaces as [`PaletteEvent::Action`] for the client to dispatch.
//!
//! Lifecycle: [`Palette::spawn`] locates and launches the binary and completes
//! the `initialize` handshake; [`Palette::open`] summons the palette;
//! `pump`/`forward_input`/`poll_events` drive it while up; [`Palette::shutdown`]
//! tears it down (`ui.shutdown` + a `SIGKILL` backstop).

use std::ffi::CString;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use posh_term::Terminal;
use serde_json::{json, Value};

use crate::pty;
use crate::util;

/// RFC 0005 protocol version this client speaks.
const PROTOCOL_VERSION: i64 = 1;
const BINARY_NAME: &str = "posh-palette";
/// How long to wait for the renderer's `initialize` response before giving up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
/// Grace period after `ui.shutdown` before the `SIGKILL` backstop.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(300);

/// The outcome of draining the control socket.
pub enum PaletteEvent {
    /// The user chose a command; the client should dispatch this action (RFC
    /// 0005 §7). The palette has closed.
    Action { method: String, params: Value },
    /// The palette was dismissed without a selection, or the renderer closed it.
    Cancelled,
    /// The user pressed copy in an info dialog (RFC 0005 §4.3). The client copies
    /// the dialog body to the terminal clipboard (OSC 52); the dialog stays open.
    Copy,
    /// Nothing actionable yet (a partial line, or only a response/ack arrived).
    None,
}

/// A running palette renderer and its emulated screen.
pub struct Palette {
    master: RawFd,
    ctrl: RawFd,
    pid: libc::pid_t,
    /// The renderer's emulated screen (the palette pixels).
    rterm: Terminal,
    open: bool,
    ctrl_buf: Vec<u8>,
    next_id: i64,
    /// The body of the most recent info dialog (RFC 0005 §3.2), held so a
    /// `ui.copy` can be answered with an OSC 52 to the real terminal.
    dialog_body: String,
}

/// Locate the `posh-palette` binary: `$POSH_PALETTE` override, else next to the
/// running executable, else the first match on `$PATH`.
///
/// "Next to the executable" tries both `current_exe()` and `argv[0]`: under a
/// nix `symlinkJoin` toolset, posh and posh-palette are symlinked side by side,
/// but `current_exe()` resolves through the symlink to posh's *own* store path,
/// whose sibling dir lacks posh-palette — while `argv[0]`, when invoked by path,
/// still points into the join dir that has it. A profile install finds it on
/// `$PATH` regardless.
fn palette_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("POSH_PALETTE") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe_paths = [
        std::env::current_exe().ok(),
        std::env::args_os().next().map(PathBuf::from),
    ];
    for dir in exe_paths.iter().flatten().filter_map(|p| p.parent()) {
        if dir.as_os_str().is_empty() {
            continue; // a bare argv[0] like "posh" has no useful directory
        }
        let cand = dir.join(BINARY_NAME);
        if cand.is_file() {
            return Some(cand);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for cand in std::env::split_paths(&path).map(|d| d.join(BINARY_NAME)) {
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

impl Palette {
    /// Spawn the renderer and complete the `initialize` handshake. Returns
    /// `None` if the binary can't be found/launched or the handshake fails — in
    /// which case the palette is simply unavailable (a non-fatal client
    /// degradation, not an error).
    pub fn spawn(rows: u16, cols: u16) -> Option<Palette> {
        let bin = palette_binary()?;
        let bin_c = CString::new(bin.as_os_str().as_bytes()).ok()?;
        let child = pty::spawn_with_control(&bin_c, rows, cols).ok()?;
        // Non-blocking so the poll loop's reads never stall on a partial line.
        util::set_nonblocking(child.master).ok()?;
        util::set_nonblocking(child.control).ok()?;
        let mut p = Palette {
            master: child.master,
            ctrl: child.control,
            pid: child.pid,
            rterm: Terminal::new(rows, cols),
            open: false,
            ctrl_buf: Vec::new(),
            next_id: 0,
            dialog_body: String::new(),
        };
        if p.handshake() {
            Some(p)
        } else {
            p.shutdown();
            None
        }
    }

    /// The renderer's screen to composite, or `None` while the palette is hidden.
    pub fn screen(&self) -> Option<&Terminal> {
        self.open.then_some(&self.rterm)
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn master_fd(&self) -> RawFd {
        self.master
    }

    pub fn ctrl_fd(&self) -> RawFd {
        self.ctrl
    }

    /// Summon the palette with a command list (RFC 0005 §3.2 `ui.show`). The
    /// `{}` ack is ignored — the client composites from the rendered screen.
    pub fn open(&mut self, title: &str, commands: Value) {
        self.send_request(
            "ui.show",
            json!({ "view": "palette", "title": title, "commands": commands }),
        );
        self.open = true;
    }

    /// Summon an info dialog (RFC 0005 §3.2 `ui.show` view="dialog"): `body` is
    /// shown with copy + dismiss affordances (#99). The body is retained so a
    /// later `ui.copy` can be answered with an OSC 52 to the real terminal.
    pub fn show_dialog(&mut self, title: &str, body: &str) {
        self.dialog_body = body.to_string();
        self.send_request(
            "ui.show",
            json!({ "view": "dialog", "title": title, "body": body }),
        );
        self.open = true;
    }

    /// The body of the most recent info dialog, for answering a `ui.copy`.
    pub fn dialog_body(&self) -> &str {
        &self.dialog_body
    }

    /// Drain the renderer PTY into the emulated screen. Returns whether the
    /// screen changed (the caller should recomposite).
    pub fn pump(&mut self) -> bool {
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

    /// Forward user keystrokes to the renderer (its stdin is the PTY).
    pub fn forward_input(&self, bytes: &[u8]) {
        write_all(self.master, bytes);
    }

    /// Drain the control socket and surface the first actionable event. A
    /// selected command's action is answered with `{}` (the renderer discards
    /// the response) and returned for the client to dispatch.
    pub fn poll_events(&mut self) -> PaletteEvent {
        if read_fd_into(self.ctrl, &mut self.ctrl_buf) <= 0 {
            return PaletteEvent::None;
        }
        while let Some(line) = self.next_line() {
            let Ok(v) = serde_json::from_slice::<Value>(&line) else {
                continue; // unparseable line: ignore (RFC 0005 §6)
            };
            match v.get("method").and_then(Value::as_str) {
                Some("ui.cancelled") => {
                    self.open = false;
                    return PaletteEvent::Cancelled;
                }
                Some("ui.copy") => {
                    // Dialog copy (RFC 0005 §4.3): the dialog stays open.
                    return PaletteEvent::Copy;
                }
                Some(method) if v.get("id").is_some() => {
                    // A selected-command action request: ack, then hand it up.
                    self.send_response(v.get("id").cloned().unwrap_or(Value::Null), json!({}));
                    let method = method.to_string();
                    let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
                    self.open = false;
                    return PaletteEvent::Action { method, params };
                }
                _ => {} // a response to our own request, or noise: ignore
            }
        }
        PaletteEvent::None
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        pty::set_term_size(self.master, rows, cols);
        self.rterm.resize(rows, cols);
    }

    /// Coordinated teardown: ask the renderer to exit, give it a grace period,
    /// then `SIGKILL` if it has not (its event loop may be wedged).
    pub fn shutdown(mut self) {
        self.send_notification("ui.shutdown", json!({}));
        unsafe { libc::close(self.ctrl) };
        unsafe { libc::close(self.master) };
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        loop {
            if reaped(self.pid) {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            let mut status: libc::c_int = 0;
            libc::waitpid(self.pid, &mut status, 0);
        }
    }

    // --- JSON-RPC plumbing (RFC 0005 §2) ---

    fn handshake(&mut self) -> bool {
        let id = self.send_request("initialize", json!({ "protocol": PROTOCOL_VERSION }));
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while let Some(v) = self.read_message_blocking(deadline) {
            if v.get("id").and_then(Value::as_i64) != Some(id) {
                continue; // not our response
            }
            return v.get("error").is_none()
                && v.get("result")
                    .and_then(|r| r.get("protocol"))
                    .and_then(Value::as_i64)
                    == Some(PROTOCOL_VERSION);
        }
        false
    }

    fn send_request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.write_message(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        }));
        id
    }

    fn send_notification(&mut self, method: &str, params: Value) {
        self.write_message(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn send_response(&mut self, id: Value, result: Value) {
        self.write_message(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn write_message(&self, v: &Value) {
        let mut line = serde_json::to_vec(v).unwrap_or_default();
        line.push(b'\n');
        write_all(self.ctrl, &line);
    }

    /// Pop one complete NDJSON line from the control buffer, if any.
    fn next_line(&mut self) -> Option<Vec<u8>> {
        let pos = self.ctrl_buf.iter().position(|&b| b == b'\n')?;
        Some(self.ctrl_buf.drain(..=pos).collect())
    }

    /// Block (up to `deadline`) for one parsed control message.
    fn read_message_blocking(&mut self, deadline: Instant) -> Option<Value> {
        loop {
            if let Some(line) = self.next_line() {
                match serde_json::from_slice::<Value>(&line) {
                    Ok(v) => return Some(v),
                    Err(_) => continue,
                }
            }
            let now = Instant::now();
            if now >= deadline || !poll_readable(self.ctrl, deadline - now) {
                return None;
            }
            if read_fd_into(self.ctrl, &mut self.ctrl_buf) <= 0 {
                return None;
            }
        }
    }
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> isize {
    // SAFETY: read into a valid, sized buffer.
    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) }
}

/// Read whatever is available on `fd` and append it to `buf`. Returns the read
/// count (<= 0 on EOF/error/`EWOULDBLOCK`).
fn read_fd_into(fd: RawFd, buf: &mut Vec<u8>) -> isize {
    let mut tmp = [0u8; 4096];
    let n = read_fd(fd, &mut tmp);
    if n > 0 {
        buf.extend_from_slice(&tmp[..n as usize]);
    }
    n
}

fn write_all(fd: RawFd, mut buf: &[u8]) {
    while !buf.is_empty() {
        // SAFETY: write from a valid, sized buffer.
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
        buf = &buf[n as usize..];
    }
}

fn poll_readable(fd: RawFd, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: poll over a single valid pollfd.
    unsafe { libc::poll(&mut pfd, 1, ms) > 0 && pfd.revents & libc::POLLIN != 0 }
}

/// Non-blocking reap check: true once the child has been collected.
fn reaped(pid: libc::pid_t) -> bool {
    let mut status: libc::c_int = 0;
    // SAFETY: waitpid writes through a valid &mut status.
    unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) == pid }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a Palette whose control socket is a test-driven socketpair; returns
    /// the peer fd the test writes renderer→client lines to. master/pid are
    /// inert (no subprocess).
    fn palette_with_ctrl() -> (Palette, RawFd) {
        let mut sp: [libc::c_int; 2] = [-1, -1];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sp.as_mut_ptr()) },
            0
        );
        util::set_nonblocking(sp[0]).unwrap();
        let p = Palette {
            master: -1,
            ctrl: sp[0],
            pid: 0,
            rterm: Terminal::new(24, 80),
            open: true,
            ctrl_buf: Vec::new(),
            next_id: 0,
            dialog_body: String::new(),
        };
        (p, sp[1])
    }

    fn write_line(fd: RawFd, s: &str) {
        let mut b = s.as_bytes().to_vec();
        b.push(b'\n');
        write_all(fd, &b);
    }

    // A selected-command request becomes an Action carrying the method and
    // params verbatim, and closes the palette (RFC 0005 §4.1).
    #[test]
    fn poll_events_surfaces_a_selected_action() {
        let (mut p, peer) = palette_with_ctrl();
        write_line(
            peer,
            r#"{"jsonrpc":"2.0","id":7,"method":"echo.set","params":{"model":"optimistic"}}"#,
        );
        match p.poll_events() {
            PaletteEvent::Action { method, params } => {
                assert_eq!(method, "echo.set");
                assert_eq!(params["model"], "optimistic");
            }
            _ => panic!("expected an Action"),
        }
        assert!(!p.is_open(), "selecting a command closes the palette");
        unsafe { libc::close(peer) };
    }

    // A ui.cancelled notification closes the palette without an action.
    #[test]
    fn poll_events_handles_cancel() {
        let (mut p, peer) = palette_with_ctrl();
        write_line(peer, r#"{"jsonrpc":"2.0","method":"ui.cancelled"}"#);
        assert!(matches!(p.poll_events(), PaletteEvent::Cancelled));
        assert!(!p.is_open());
        unsafe { libc::close(peer) };
    }

    // A ui.copy notification surfaces a Copy event and keeps the dialog open
    // (the client copies the body to the clipboard; the dialog stays up).
    #[test]
    fn poll_events_surfaces_copy() {
        let (mut p, peer) = palette_with_ctrl();
        write_line(peer, r#"{"jsonrpc":"2.0","method":"ui.copy"}"#);
        assert!(matches!(p.poll_events(), PaletteEvent::Copy));
        assert!(p.is_open(), "copy must not close the dialog");
        unsafe { libc::close(peer) };
    }

    // show_dialog summons a `dialog` view carrying the body, and retains that
    // body so a later ui.copy can be answered with an OSC 52.
    #[test]
    fn show_dialog_sends_dialog_view_and_keeps_body() {
        let (mut p, peer) = palette_with_ctrl();
        p.show_dialog("agent forwarding", "agent-fwd: on\nserver: up");
        assert_eq!(p.dialog_body(), "agent-fwd: on\nserver: up");
        assert!(p.is_open());
        let mut buf = [0u8; 512];
        let n = unsafe { libc::read(peer, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        let line = std::str::from_utf8(&buf[..n.max(0) as usize]).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["method"], "ui.show");
        assert_eq!(v["params"]["view"], "dialog");
        assert_eq!(v["params"]["body"], "agent-fwd: on\nserver: up");
        unsafe { libc::close(peer) };
    }

    // A response to one of our own requests (no method) is not an event.
    #[test]
    fn poll_events_ignores_responses() {
        let (mut p, peer) = palette_with_ctrl();
        write_line(peer, r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert!(matches!(p.poll_events(), PaletteEvent::None));
        assert!(p.is_open(), "an ack must not close the palette");
        unsafe { libc::close(peer) };
    }

    // Full round-trip against the REAL posh-palette binary: spawn + handshake,
    // show a command, select it, read the action back. Skipped (no-op) when the
    // binary isn't locatable — e.g. the hermetic `cargo test` sandbox, where
    // posh-palette is a separate derivation. Run locally with POSH_PALETTE set.
    #[test]
    fn real_binary_round_trip() {
        if palette_binary().is_none() {
            eprintln!("skip: posh-palette not found (set POSH_PALETTE to run)");
            return;
        }
        let mut p = Palette::spawn(24, 80).expect("spawn + handshake");
        p.open(
            "Commands",
            json!([{ "name": "Quit", "action": { "method": "app.quit" } }]),
        );
        // Drain the renderer until it has painted the palette.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !p.rterm.dump_text().contains("Quit") {
            poll_readable(p.master, Duration::from_millis(100));
            p.pump();
        }
        assert!(
            p.rterm.dump_text().contains("Quit"),
            "renderer never drew the command"
        );
        // Select it (Enter) and read the action back.
        p.forward_input(b"\r");
        let mut action = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && action.is_none() {
            poll_readable(p.ctrl, Duration::from_millis(100));
            if let PaletteEvent::Action { method, .. } = p.poll_events() {
                action = Some(method);
            }
        }
        assert_eq!(action.as_deref(), Some("app.quit"));
        p.shutdown();
    }

    // Selecting a command must round-trip its action's NESTED params intact
    // (#3: the server-logging command carries {scope, enabled}; the existing
    // round-trip only covered a param-less app.quit). Skipped without the binary.
    #[test]
    fn real_binary_selection_preserves_nested_params() {
        if palette_binary().is_none() {
            eprintln!("skip: posh-palette not found (set POSH_PALETTE to run)");
            return;
        }
        let mut p = Palette::spawn(24, 80).expect("spawn + handshake");
        p.open(
            "Commands",
            json!([{
                "name": "Server debug logging: on",
                "action": { "method": "logging.set", "params": { "scope": "server", "enabled": true } }
            }]),
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !p.rterm.dump_text().contains("Server") {
            poll_readable(p.master, Duration::from_millis(100));
            p.pump();
        }
        p.forward_input(b"\r"); // select the only command
        let mut got = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && got.is_none() {
            poll_readable(p.ctrl, Duration::from_millis(100));
            if let PaletteEvent::Action { method, params } = p.poll_events() {
                got = Some((method, params));
            }
        }
        let (method, params) = got.expect("an action came back");
        assert_eq!(method, "logging.set");
        assert_eq!(params["scope"], "server", "nested scope param lost: {params}");
        assert_eq!(params["enabled"], true, "nested enabled param lost: {params}");
        p.shutdown();
    }

    // A long command list (a 9-entry palette like the real one) must reach the
    // renderer intact — the wire write and the renderer's parse must not truncate
    // it. Reproduces the #3 report of a missing "Server debug logging". Skipped
    // without the binary; a tall PTY avoids the renderer clipping the panel.
    #[test]
    fn real_binary_lists_a_long_command_set() {
        if palette_binary().is_none() {
            eprintln!("skip: posh-palette not found (set POSH_PALETTE to run)");
            return;
        }
        let mut p = Palette::spawn(40, 100).expect("spawn + handshake");
        p.open(
            "Commands",
            json!([
                { "name": "Echo: adaptive", "action": { "method": "echo.set" } },
                { "name": "Echo: optimistic", "action": { "method": "echo.set" } },
                { "name": "Echo: always", "action": { "method": "echo.set" } },
                { "name": "Echo: never", "action": { "method": "echo.set" } },
                { "name": "Client debug logging: on", "action": { "method": "logging.set" } },
                { "name": "Server debug logging: on", "action": { "method": "logging.set", "params": { "scope": "server" } } },
                { "name": "Shell out (server)", "action": { "method": "shell.open" } },
                { "name": "Suspend client", "action": { "method": "client.suspend" } },
                { "name": "Quit session", "action": { "method": "app.quit" } },
            ]),
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !p.rterm.dump_text().contains("Quit session") {
            poll_readable(p.master, Duration::from_millis(100));
            p.pump();
        }
        let text = p.rterm.dump_text();
        assert!(
            text.contains("Server debug logging"),
            "a later command was dropped:\n{text}"
        );
        assert!(
            text.contains("Quit session"),
            "the last command was dropped:\n{text}"
        );
        p.shutdown();
    }
}
