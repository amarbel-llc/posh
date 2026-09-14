//! The establish takeover and its modals (#1, reshaped by posh#195, FDR 0019).
//!
//! A remote attach takes over the alt screen as the FIRST thing it does —
//! before the mux endpoint is ensured, before any ssh — and shows the whole
//! establishment as a command-palette-style modal composited onto the (empty,
//! greyed) viewport, dismissed on the first frame. Three pieces:
//!
//! - [`Takeover`]: the terminal takeover itself. smcup + raw mode + a stderr
//!   capture (a stray `eprintln!` while the alt screen is up would draw over
//!   the modal; captured lines are replayed onto the real stderr after rmcup).
//!   Dropping it restores the terminal. It owns a tiny differential painter so
//!   a modal can be drawn while no client loop is running yet (the ssh phase,
//!   the cold mux-endpoint bootstrap).
//! - [`CrapModal`]: the progress modal — a captured `crap-present` process fed
//!   ndjson-crap (the `rust_crap` writer; CRAP's producer -> viewport split),
//!   its rendered PTY output emulated into a [`Terminal`] the client composites
//!   with `palette::composite_palette`. The crap-present analog of
//!   [`crate::remote::palette::Palette`]: spawn / pump / screen / resize /
//!   ndjson-writers / teardown.
//! - [`SshModal`]: the interactive modal — the bootstrap `ssh` hosted on a full
//!   PTY (its controlling terminal, so a host-key `yes/no`, a password, or a 2FA
//!   prompt lands there), the user's keystrokes forwarded to it, its output
//!   emulated into the same kind of [`Terminal`], and the `POSH IP` / `POSH
//!   CONNECT` handshake scraped off the byte stream (never shown).
//!
//! When `crap-present` is unavailable the progress modal is simply absent
//! (`spawn` returns `None`) and the takeover shows a blank greyed viewport; when
//! stdout is not a TTY there is no takeover at all ([`Takeover::begin`] returns
//! `None`) and the callers keep their pre-takeover behavior.

use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use posh_term::Terminal;

use crate::pty::RawMode;
use crate::remote::display::{self, Snapshot};
use crate::remote::palette::{composite_palette, EchoWatch};
use crate::remote::sshwrap::{LineScraper, ServerReport};
use crate::util::{self, Result};

const STDIN: RawFd = libc::STDIN_FILENO;
const STDOUT: RawFd = libc::STDOUT_FILENO;

const BINARY_NAME: &str = "crap-present";
/// Grace for `crap-present` to render its verdict, clear its status line, and
/// exit after the stream's summary record + stdin EOF, before we SIGKILL it, so
/// a wedged renderer can never hold up the terminal takeover.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
/// How long [`Takeover::begin`] waits for a freshly spawned progress modal to
/// render its header before the first paint, so the takeover never shows an
/// empty grey screen while a blocking step (the mux endpoint bootstrap) runs.
const PRIME_WAIT: Duration = Duration::from_millis(150);

/// Locate `crap-present`: `$POSH_CRAP_PRESENT` override, else next to the running
/// executable (the nix toolset lookup through `current_exe`/argv[0]), else the
/// first match on `$PATH` (the wrap the flake adds to posh's PATH). Mirrors
/// `palette::palette_binary` so the two tools resolve the same way under a nix
/// `symlinkJoin` toolset and an ambient install alike.
fn crap_present_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("POSH_CRAP_PRESENT") {
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
            continue;
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

/// The progress modal: a captured `crap-present` process. Its stdout is a PTY
/// posh reads into `rterm` (the emulated screen the client composites); its
/// stdin is the pipe posh writes ndjson-crap to. The crap-present analog of
/// [`crate::remote::palette::Palette`].
pub struct CrapModal {
    /// Host end of the child's stdout PTY — read rendered output here.
    master: RawFd,
    /// Host (write) end of the ndjson-crap pipe feeding the child's stdin.
    stdin: std::fs::File,
    pid: libc::pid_t,
    /// The emulated screen tracking crap-present's render; `composite_palette`
    /// centers its non-blank region into the modal.
    rterm: Terminal,
    /// The attach target, e.g. "flac:dev" — named in the establish/verdict lines.
    source: String,
    /// Observational guard: catches a query answer posh wrote to this PTY echoing
    /// back into `rterm` (posh#195). Silent unless the slave's ECHO was left on.
    echo_watch: EchoWatch,
}

impl CrapModal {
    /// Spawn `crap-present` as a captured PTY modal and write the establish
    /// header. Returns `None` — a silent no-op, so the caller takes over the
    /// terminal immediately without an overlay — when stdout is not a TTY,
    /// `crap-present` is not found, or the spawn fails.
    pub fn spawn(source: &str, rows: u16, cols: u16) -> Option<CrapModal> {
        // The modal is composited into the viewport, which only exists on a real
        // terminal; off a tty (a pipe, a test harness) there is nothing to show.
        if !util::is_tty(STDOUT) {
            return None;
        }
        let bin = crap_present_binary()?;
        let bin_c = std::ffi::CString::new(bin.as_os_str().as_encoded_bytes()).ok()?;
        let child = crate::pty::spawn_capture(&bin_c, rows, cols).ok()?;
        util::set_nonblocking(child.master).ok()?;
        // SAFETY: `child.stdin` is a freshly-returned owned pipe write end; the
        // File takes sole ownership and closes it on drop (the EOF teardown).
        let stdin = unsafe { std::fs::File::from_raw_fd(child.stdin) };
        let mut m = CrapModal {
            master: child.master,
            stdin,
            pid: child.pid,
            rterm: Terminal::new(rows, cols),
            source: source.to_string(),
            echo_watch: EchoWatch::default(),
        };
        let mut w = rust_crap::NdjsonCrapWriter::new(&mut m.stdin);
        let _ = w.header(&format!("establishing {source}"), source);
        Some(m)
    }

    /// The emulated screen for the client to composite (`composite_palette`).
    pub fn screen(&self) -> &Terminal {
        &self.rterm
    }

    pub fn master_fd(&self) -> RawFd {
        self.master
    }

    /// Drain the crap-present PTY into the emulated screen. Returns whether the
    /// screen changed (the caller should recomposite). Mirrors `Palette::pump`.
    pub fn pump(&mut self) -> bool {
        let mut buf = [0u8; 8192];
        let n = unsafe { libc::read(self.master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            return false;
        }
        let read = &buf[..n as usize];
        // Did our last query answer echo back (slave ECHO on)? Observational.
        self.echo_watch.saw_read(read, "crap-present");
        let before = self.rterm.generation();
        self.rterm.process(read);
        let replies = self.rterm.take_responses();
        if !replies.is_empty() {
            let _ = unsafe {
                libc::write(self.master, replies.as_ptr() as *const libc::c_void, replies.len())
            };
            self.echo_watch.wrote(&replies);
        }
        self.rterm.generation() != before
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        crate::pty::set_term_size(self.master, rows, cols);
        self.rterm.resize(rows, cols);
    }

    /// Finish the stream with the success verdict (first frame): `plan_ahead(1)`
    /// → `ok("connected to …")` → `finish`. The writer flush + a later
    /// [`teardown`](Self::teardown) close the pipe (stdin EOF) so crap-present
    /// renders its verdict and exits.
    pub fn ok(&mut self) {
        let source = self.source.clone();
        let mut w = rust_crap::NdjsonCrapWriter::new(&mut self.stdin);
        let _ = w.plan_ahead(1);
        let _ = w.ok(&format!("connected to {source}"));
        let _ = w.finish();
    }

    /// Finish the stream with a failure verdict (timeout/abort): `plan_ahead(1)`
    /// → `not_ok_diag("establish connection", message=reason)` → `finish`.
    pub fn not_ok(&mut self, reason: &str) {
        let mut w = rust_crap::NdjsonCrapWriter::new(&mut self.stdin);
        let _ = w.plan_ahead(1);
        let _ = w.not_ok_diag("establish connection", &[("message", reason)]);
        let _ = w.finish();
    }

    /// Retire the modal without a verdict (the establishment continues in
    /// another modal — the interactive ssh phase takes the screen): finish the
    /// stream empty and tear the child down.
    pub fn close(mut self) {
        let mut w = rust_crap::NdjsonCrapWriter::new(&mut self.stdin);
        let _ = w.finish();
        self.teardown();
    }

    /// Close the stdin pipe (crap-present sees EOF, renders its verdict, exits),
    /// give it [`SHUTDOWN_GRACE`], then SIGKILL + reap. Closes the PTY. Mirrors
    /// `Palette::shutdown`.
    pub fn teardown(self) {
        let CrapModal {
            master, stdin, pid, ..
        } = self;
        drop(stdin); // stdin EOF: crap-present finishes rendering and exits
        reap_or_kill(pid, master, SHUTDOWN_GRACE);
    }
}

/// Wait up to `grace` for `pid` to exit on its own, then SIGKILL + reap; close
/// `master` either way.
fn reap_or_kill(pid: libc::pid_t, master: RawFd, grace: Duration) {
    let deadline = Instant::now() + grace;
    loop {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid || r < 0 {
            unsafe { libc::close(master) };
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
        libc::waitpid(pid, &mut status, 0);
        libc::close(master);
    }
}

/// The interactive modal (FDR 0019): the bootstrap `ssh` hosted on a full PTY
/// that is its controlling terminal, so every prompt it raises (host-key
/// approval, a password, a 2FA challenge) lands in the modal and is answered
/// by typing. The PTY keeps a normal cooked+echo discipline (NOT the
/// `quiet_emulator_slave` hardening the captured renderers get): a `yes/no`
/// prompt wants echo + line editing, and ssh turns echo off itself for a
/// password. The `POSH …` handshake lines are scraped off the byte stream by
/// [`LineScraper`] before it reaches the emulated screen, so the session key
/// on `POSH CONNECT` is never rendered.
pub struct SshModal {
    master: RawFd,
    pid: libc::pid_t,
    rterm: Terminal,
    scraper: LineScraper,
    /// The PTY hit EOF (ssh exited, or its slave closed).
    eof: bool,
}

impl SshModal {
    /// Spawn `argv` (an `ssh …` command line) on a `rows`×`cols` PTY. `preface`
    /// lines are shown at the top of the modal before any ssh output (the
    /// tailnet dial notice, which would otherwise go to the captured stderr).
    pub fn spawn(argv: &[String], rows: u16, cols: u16, preface: &[String]) -> Result<SshModal> {
        let child = crate::pty::spawn_shell(Some(argv), rows, cols, &[], None)?;
        util::set_nonblocking(child.master)?;
        let mut rterm = Terminal::new(rows, cols);
        for line in preface {
            rterm.process(line.as_bytes());
            rterm.process(b"\r\n");
        }
        Ok(SshModal {
            master: child.master,
            pid: child.pid,
            rterm,
            scraper: LineScraper::default(),
            eof: false,
        })
    }

    pub fn screen(&self) -> &Terminal {
        &self.rterm
    }

    pub fn master_fd(&self) -> RawFd {
        self.master
    }

    /// Forward the user's keystrokes to ssh (its stdin/tty is the PTY).
    pub fn forward_input(&self, bytes: &[u8]) {
        let _ = util::write_all_retry(self.master, bytes, 1000);
    }

    /// Drain the ssh PTY: protocol lines into the [`ServerReport`], everything
    /// else into the emulated screen. Returns whether the screen changed. A
    /// malformed protocol line is the bootstrap's error. EOF/EIO (ssh gone)
    /// latches [`eof`](Self::eof).
    pub fn pump(&mut self) -> Result<bool> {
        let mut buf = [0u8; 8192];
        let n = match util::read_fd(self.master, &mut buf) {
            Ok(0) => {
                self.eof = true;
                return Ok(false);
            }
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(false),
            // EIO: the slave side closed (the child exited) — the PTY's EOF.
            Err(_) => {
                self.eof = true;
                return Ok(false);
            }
        };
        let shown = self.scraper.feed(&buf[..n])?;
        let before = self.rterm.generation();
        self.rterm.process(&shown);
        // ssh never queries the terminal; anything the emulator would answer is
        // dropped rather than written into ssh's stdin (which it forwards).
        let _ = self.rterm.take_responses();
        Ok(self.rterm.generation() != before)
    }

    /// `POSH CONNECT` has been parsed: the roaming server is up.
    pub fn connected(&self) -> bool {
        self.scraper.report().port.is_some()
    }

    pub fn eof(&self) -> bool {
        self.eof
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        crate::pty::set_term_size(self.master, rows, cols);
        self.rterm.resize(rows, cols);
    }

    /// The modal's text, for the failure message's ssh tail (what ssh itself
    /// said) — the stderr capture the pipe-based bootstrap has.
    pub fn screen_text(&self) -> String {
        self.rterm.dump_text()
    }

    /// Ask ssh to stop (SIGTERM) — the user aborted or the bootstrap gave up.
    pub fn interrupt(&self) {
        unsafe { libc::kill(self.pid, libc::SIGTERM) };
    }

    /// Reap ssh (SIGKILL after `grace` if it lingers), close the PTY, and hand
    /// back what the server reported.
    pub fn finish(self, grace: Duration) -> ServerReport {
        let SshModal {
            master,
            pid,
            scraper,
            ..
        } = self;
        reap_or_kill(pid, master, grace);
        scraper.into_report()
    }
}

/// What a client loop inherits from a [`Takeover`] that already owns the
/// terminal: the progress modal to keep compositing until the first frame and
/// the painter's state (so its first frame diffs against what the takeover
/// drew instead of clearing the screen).
pub struct Handoff {
    pub modal: Option<CrapModal>,
    pub last_drawn: Snapshot,
    pub initialized: bool,
}

/// The terminal takeover (FDR 0019): the alt screen is posh's from the moment
/// the attach command is invoked. Holds raw mode (so nothing typed during the
/// establishment echoes over the modal), the alt screen (smcup; rmcup on
/// drop), and a stderr capture (every `eprintln!` while the takeover is live
/// — a mux fallback warning, a tailnet dial notice, the client's own exit
/// line — lands in an anonymous file and is replayed onto the real stderr
/// after rmcup, exactly where a pre-takeover message used to appear). Carries
/// the progress modal between phases and a differential painter for the
/// phases no client loop is running.
pub struct Takeover {
    label: String,
    pub rows: u16,
    pub cols: u16,
    raw: Option<RawMode>,
    saved_stderr: RawFd,
    capture: File,
    modal: Option<CrapModal>,
    last: Snapshot,
    initialized: bool,
}

impl Takeover {
    /// Take over the terminal for an attach to `label` (the target as typed,
    /// e.g. `flac:dev`): raw mode, smcup, the stderr capture, and the progress
    /// modal primed with its header and painted. `None` when stdout is not a
    /// TTY — off-tty there is no viewport to take over, and every caller keeps
    /// its pre-takeover behavior.
    pub fn begin(label: &str) -> Option<Takeover> {
        if !util::is_tty(STDOUT) {
            return None;
        }
        let capture = anonymous_file()?;
        let raw = RawMode::enable(STDIN).ok()?;
        // Handlers before the takeover write (github #48): a SIGTERM racing the
        // first byte must find the flag handler, not the default disposition —
        // which would kill the process with the alt screen stranded.
        util::install_client_signal_handlers();
        // SAFETY: dup/dup2/fcntl on plain integer fds; `capture` stays open for
        // the takeover's lifetime, so fd 2 always has a valid target.
        let saved_stderr = unsafe { libc::dup(libc::STDERR_FILENO) };
        if saved_stderr < 0 {
            return None;
        }
        unsafe {
            // The real stderr must not ride into spawned children (ssh,
            // crap-present): they write to fd 2 — the capture — like us.
            libc::fcntl(saved_stderr, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::dup2(capture.as_raw_fd(), libc::STDERR_FILENO);
        }
        let (rows, cols) = crate::pty::term_size(STDOUT);
        let _ = util::write_all_retry(STDOUT, &display::open(), 1000);
        let mut t = Takeover {
            label: label.to_string(),
            rows,
            cols,
            raw: Some(raw),
            saved_stderr,
            capture,
            modal: None,
            last: Snapshot::blank(rows, cols),
            initialized: false,
        };
        t.ensure_modal();
        Some(t)
    }

    /// Spawn the progress modal if none is up (a fresh establishment phase
    /// after a hand-off ended), wait briefly for its header, and paint.
    pub fn ensure_modal(&mut self) {
        if self.raw.is_none() {
            // Back from a handed-off client loop that restored the tty.
            self.raw = RawMode::enable(STDIN).ok();
        }
        if self.modal.is_none() {
            self.modal = CrapModal::spawn(&self.label, self.rows, self.cols);
            if let Some(m) = self.modal.as_mut() {
                let deadline = Instant::now() + PRIME_WAIT;
                let mut fds = [util::pollfd(m.master_fd(), libc::POLLIN)];
                while Instant::now() < deadline {
                    let left = deadline.saturating_duration_since(Instant::now());
                    let _ = util::poll(&mut fds, left.as_millis() as i32);
                    if m.pump() {
                        break;
                    }
                }
            }
        }
        self.paint_modal();
    }

    fn paint_modal(&mut self) {
        let mut next = Snapshot::blank(self.rows, self.cols);
        if let Some(m) = self.modal.as_ref() {
            composite_palette(&mut next, m.screen(), self.rows, self.cols);
        } else {
            next.cursor_visible = false;
        }
        self.paint(next);
    }

    /// Composite `screen` (the interactive modal) onto the blank greyed
    /// viewport and paint the difference.
    pub fn paint_screen(&mut self, screen: &Terminal) {
        let mut next = Snapshot::blank(self.rows, self.cols);
        composite_palette(&mut next, screen, self.rows, self.cols);
        self.paint(next);
    }

    fn paint(&mut self, next: Snapshot) {
        let bytes = display::new_frame_opt(self.initialized, &self.last, &next, false, false, true);
        self.initialized = true;
        self.last = next;
        let _ = util::write_all_retry(STDOUT, &bytes, 1000);
    }

    /// A terminal resize during a takeover-driven phase: resize the progress
    /// modal (if up) and force the next paint to be a full repaint.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        if let Some(m) = self.modal.as_mut() {
            m.resize(rows, cols);
        }
        self.last = Snapshot::blank(rows, cols);
        self.initialized = false;
    }

    /// Retire the progress modal (the interactive ssh phase takes the screen).
    pub fn close_modal(&mut self) {
        if let Some(m) = self.modal.take() {
            m.close();
        }
    }

    /// Hand the establishment to a client loop: the progress modal (spawned
    /// fresh if the interactive phase retired it) and the painter's state. Raw
    /// mode is released — the client loop enters and restores its own, so a
    /// suspend from inside the session restores the tty the user started with
    /// — and the takeover's own painter forgets the screen (a later paint, on
    /// a fallback, repaints in full).
    pub fn handoff(&mut self) -> Handoff {
        if self.modal.is_none() {
            self.modal = CrapModal::spawn(&self.label, self.rows, self.cols);
        }
        self.raw = None;
        let h = Handoff {
            modal: self.modal.take(),
            last_drawn: self.last.clone(),
            initialized: self.initialized,
        };
        self.initialized = false;
        self.last = Snapshot::blank(self.rows, self.cols);
        h
    }
}

impl Drop for Takeover {
    fn drop(&mut self) {
        if let Some(m) = self.modal.take() {
            m.close();
        }
        let _ = util::write_all_retry(STDOUT, &display::close(), 1000);
        self.raw = None; // restore the tty
        // Real stderr back, then replay what was said while the alt screen
        // was up, so it lands on the primary screen where it can be read.
        // SAFETY: dup2/close on the fds this takeover dup'd above.
        unsafe {
            libc::dup2(self.saved_stderr, libc::STDERR_FILENO);
            libc::close(self.saved_stderr);
        }
        let mut said = Vec::new();
        if self.capture.rewind().is_ok() && self.capture.read_to_end(&mut said).is_ok() && !said.is_empty()
        {
            let _ = std::io::stderr().write_all(&said);
        }
    }
}

/// An anonymous read/write file (created 0600, unlinked at once) for the
/// stderr capture. `None` when the temp dir is unwritable — then there is no
/// takeover, rather than a takeover that loses stderr.
fn anonymous_file() -> Option<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = std::env::temp_dir().join(format!(
        "posh-stderr-{}-{}",
        std::process::id(),
        util::now_ms()
    ));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    let _ = std::fs::remove_file(&path);
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_is_a_noop_off_a_tty() {
        // The test harness's stdout is captured, not a terminal, so the modal is
        // absent regardless of whether crap-present is installed — the
        // graceful-degrade path the caller relies on to take over immediately.
        assert!(
            CrapModal::spawn("host:test", 24, 80).is_none(),
            "no establish modal when stdout is not a tty"
        );
        // Likewise the takeover itself: off-tty there is no viewport, so the
        // callers keep their pre-takeover (no smcup, inherited-stdio) behavior.
        assert!(Takeover::begin("host:test").is_none());
    }

    /// Drive a modal-hosted child to completion, feeding `answer` once the
    /// screen shows `prompt`. Returns the final screen text.
    fn drive(modal: &mut SshModal, prompt: &str, answer: &[u8]) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut answered = false;
        while Instant::now() < deadline && !modal.eof() {
            let mut fds = [util::pollfd(modal.master_fd(), libc::POLLIN)];
            let _ = util::poll(&mut fds, 50);
            modal.pump().expect("pump");
            if !answered && modal.screen_text().contains(prompt) {
                modal.forward_input(answer);
                answered = true;
            }
        }
        modal.screen_text()
    }

    /// The interactive modal end to end against a stand-in for ssh: a prompt
    /// without a trailing newline shows at once, the typed answer is echoed by
    /// the PTY's cooked discipline and read by the child, and the handshake
    /// lines resolve the report WITHOUT the key ever reaching the screen.
    #[test]
    fn ssh_modal_answers_a_prompt_and_scrapes_the_handshake_off_screen() {
        let argv: Vec<String> = [
            "/bin/sh",
            "-c",
            "printf 'Continue (yes/no)? '; read a; echo \"got $a\"; \
             echo 'POSH IP 192.0.2.7'; echo 'POSH AGENT_EXPORT'; \
             echo 'POSH CONNECT 60001 AAAAAAAAAAAAAAAAAAAAAA'; echo done",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut modal = SshModal::spawn(&argv, 24, 80, &["dialing example".to_string()]).unwrap();
        let text = drive(&mut modal, "Continue (yes/no)?", b"yes\r");
        assert!(text.contains("dialing example"), "preface shown: {text}");
        assert!(text.contains("Continue (yes/no)? yes"), "prompt + echoed answer: {text}");
        assert!(text.contains("got yes"), "the child read the answer: {text}");
        assert!(text.contains("done"), "post-handshake output still shows: {text}");
        assert!(!text.contains("POSH"), "protocol lines never render: {text}");
        assert!(!text.contains("AAAAAAAA"), "the key never renders: {text}");
        assert!(modal.connected());
        let report = modal.finish(Duration::from_secs(2));
        assert_eq!(report.ip.as_deref(), Some("192.0.2.7"));
        assert_eq!(report.port, Some(60001));
        assert_eq!(report.key.as_deref(), Some("AAAAAAAAAAAAAAAAAAAAAA"));
        assert!(report.agent_export);
    }

    /// A child that exits without the handshake leaves the report unresolved
    /// and its complaint on screen (the failure message's tail), and finish
    /// reaps it without blocking.
    #[test]
    fn ssh_modal_without_a_handshake_keeps_the_complaint_on_screen() {
        let argv: Vec<String> = ["/bin/sh", "-c", "echo 'Permission denied (publickey).' >&2; exit 255"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut modal = SshModal::spawn(&argv, 24, 80, &[]).unwrap();
        let text = drive(&mut modal, "never", b"");
        assert!(text.contains("Permission denied"), "{text}");
        assert!(!modal.connected());
        assert!(modal.eof());
        let report = modal.finish(Duration::from_secs(2));
        assert_eq!(report.port, None);
    }
}
