//! Connect-progress indicator (#1): while an attach is establishing — before
//! posh takes over the terminal — show a live "establishing connection" spinner
//! on the PRIMARY screen. posh emits ndjson-crap (the `rust_crap` writer) to a
//! `crap-present` child that renders it; this is CRAP's producer -> viewport
//! split (the writer never draws). On the first frame the stream is finished,
//! the child torn down, and posh takes over the alt screen. When `crap-present`
//! is unavailable or stdout is not a TTY the indicator is simply absent and posh
//! takes over immediately, exactly as before.
//!
//! The ndjson-crap writing itself lives in the client loop (`client::drive_client`),
//! which owns the child's stdin for the establish; this module owns locating,
//! spawning, and tearing down the `crap-present` process. The spawn is
//! deliberately simpler than the palette renderer (`remote::palette`): no PTY,
//! no control channel, no compositing — `crap-present` inherits posh's real
//! stdout and draws to the terminal directly, fed ndjson-crap on a plain pipe.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

/// Spawn a `crap-present` child to draw the establish spinner on the primary
/// screen: its stdin is a pipe the client writes ndjson-crap to; its
/// stdout/stderr are inherited so it draws to the real terminal. Returns `None`
/// — a silent no-op, so the caller takes over the terminal immediately as before
/// — when stdout is not a TTY, `crap-present` is not found, or the spawn fails.
/// The returned child's `stdin` is the piped write end; the caller takes it to
/// feed the ndjson-crap stream.
pub fn spawn() -> Option<Child> {
    // A spinner only makes sense on a terminal, and CRAP's Status Line profile is
    // TTY-only; off a tty (a pipe, a test harness) there is nothing to draw.
    if unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1 {
        return None;
    }
    let bin = crap_present_binary()?;
    Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()
}

/// Wait for a `crap-present` child to finish after the ndjson-crap stream's
/// summary record and stdin EOF (the caller drops the child's stdin before
/// calling this): it renders its final verdict, clears its status line, and
/// exits. Bounded by [`SHUTDOWN_GRACE`], then SIGKILL + reap. Mirrors
/// `Palette::shutdown`.
pub fn teardown(mut child: Child) {
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(_) => return,
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_is_a_noop_off_a_tty() {
        // The test harness's stdout is captured, not a terminal, so the indicator
        // is absent regardless of whether crap-present is installed — the
        // graceful-degrade path the caller relies on to take over immediately.
        assert!(spawn().is_none(), "no spinner when stdout is not a tty");
    }
}
