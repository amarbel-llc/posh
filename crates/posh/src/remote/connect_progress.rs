//! Connect-progress modal (#1, reshaped by posh#195): while an attach is
//! establishing, posh takes over the alt screen IMMEDIATELY and shows the
//! "establishing connection" progress as a command-palette-style modal overlay
//! composited onto the (empty, greyed) viewport, dismissed on the first frame.
//!
//! The renderer is still `crap-present` fed ndjson-crap (the `rust_crap` writer;
//! CRAP's producer -> viewport split, the writer never draws) — but instead of
//! drawing to the real primary screen (the pre-#195 behavior), its output is
//! CAPTURED off a PTY that represents the modal, exactly like the command-
//! palette renderer (`remote::palette`). posh feeds it ndjson-crap on a pipe
//! (its stdin), reads its rendered terminal output off the PTY into an emulated
//! [`Terminal`], and the client composites that with `palette::composite_palette`
//! each frame. On the first frame the stream is finished (`ok`) and the child
//! torn down; on timeout/abort it is finished with a `not_ok` verdict. When
//! `crap-present` is unavailable or stdout is not a TTY the modal is simply
//! absent (`spawn` returns `None`) — posh still takes over immediately, just
//! without the overlay, and the transport-agnostic "Last contact" banner covers
//! the no-modal case.
//!
//! [`CrapModal`] is the crap-present analog of [`crate::remote::palette::Palette`]:
//! spawn / pump / screen / resize / ndjson-writers / teardown.

use std::os::fd::{FromRawFd, RawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use posh_term::Terminal;

use crate::remote::palette::EchoWatch;

const BINARY_NAME: &str = "crap-present";
/// Grace for `crap-present` to render its verdict, clear its status line, and
/// exit after the stream's summary record + stdin EOF, before we SIGKILL it, so
/// a wedged renderer can never hold up the terminal takeover.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

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

/// The establish modal: a captured `crap-present` process. Its stdout is a PTY
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
        if unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1 {
            return None;
        }
        let bin = crap_present_binary()?;
        let bin_c = std::ffi::CString::new(bin.as_os_str().as_encoded_bytes()).ok()?;
        let child = crate::pty::spawn_capture(&bin_c, rows, cols).ok()?;
        crate::util::set_nonblocking(child.master).ok()?;
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

    /// Close the stdin pipe (crap-present sees EOF, renders its verdict, exits),
    /// give it [`SHUTDOWN_GRACE`], then SIGKILL + reap. Closes the PTY. Mirrors
    /// `Palette::shutdown`.
    pub fn teardown(self) {
        let CrapModal {
            master, stdin, pid, ..
        } = self;
        drop(stdin); // stdin EOF: crap-present finishes rendering and exits
        let deadline = Instant::now() + SHUTDOWN_GRACE;
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
    }
}
