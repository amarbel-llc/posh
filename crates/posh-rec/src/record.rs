//! `posh-rec record [--out f.castx] -- <cmd> [args...]`: spawn a command under
//! a PTY, tee its output to the terminal AND to a `.castx` recording.
//!
//! This is the only part of posh-rec that needs PTY/libc FFI, so it lives in
//! the **binary** (declared `mod record;` in `main.rs`, never in `lib.rs`),
//! keeping the `#![forbid(unsafe_code)]` library pure. The unsafe syscall
//! patterns mirror the posh binary's `crates/posh/src/pty.rs` (a separate
//! crate, so an own copy — not a shared module — per ADR-0003). Serialization
//! is the safe library's [`posh_rec::castx::Recorder`].

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use posh_rec::castx::{Header, PoshRec, Recorder};

const STDIN_FD: RawFd = 0;
const STDOUT_FD: RawFd = 1;

const USAGE: &str = "\
usage: posh-rec record [--out FILE] [--via posh|ssh --host HOST] -- <cmd> [args...]

Spawn <cmd> under a PTY, tee its output to the terminal and to a .castx
recording (default --out recording.castx). Replay it with `posh-rec replay`.

With --via/--host the command is run over a remote transport, for capturing a
remote session's client-side rendering (diff a posh vs ssh recording to localize
a drawing bug):
  --via posh --host HOST  ->  posh HOST -- <cmd>   (roaming; HOST:session for a
                              persistent session)
  --via ssh  --host HOST  ->  ssh -t HOST <cmd>    (no posh in the loop — the
                              ground-truth render)
The remote <cmd> must already exist on HOST (e.g. deploy posht with
posht/run-remote.sh, or install it).";

/// Remote transport for `--via` (FDR 0006 capture loop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Via {
    Posh,
    Ssh,
}

/// Run `posh-rec record`. Returns the child's exit code on success.
pub fn run(args: &[String]) -> Result<i32, String> {
    let (out_path, command) = parse_args(args)?;

    // The recording's size is the current terminal's; falls back to 24x80 when
    // stdout is not a tty (e.g. redirected — also how the integration test runs).
    let (rows, cols) = term_size(STDOUT_FD);

    let file = std::fs::File::create(&out_path).map_err(|e| format!("{out_path}: {e}"))?;
    let mut rec = Recorder::new(std::io::BufWriter::new(file));
    rec.write_header(&Header {
        version: 2,
        width: cols,
        height: rows,
        posh_rec: Some(PoshRec {
            v: 1,
            emu_rev: posh_term::version().to_string(),
        }),
    })
    .map_err(|e| format!("{out_path}: {e}"))?;

    // Best-effort raw mode: a redirected/non-tty stdin just isn't put in raw
    // mode (and isn't forwarded as input). Restored on drop.
    let raw = RawMode::enable(STDIN_FD);
    install_sigwinch();

    let pty = spawn(&command, rows, cols).map_err(|e| format!("spawn {}: {e}", command[0]))?;
    let start = Instant::now();
    let code = tee_loop(&mut rec, pty.master, start);

    let status = wait_child(pty.pid);
    // SAFETY: closing our own master fd once the loop is done.
    unsafe { libc::close(pty.master) };
    let _ = rec.finish();
    drop(raw); // restore the terminal before printing

    eprintln!("posh-rec: recorded → {out_path}");
    // Prefer the child's real exit status; fall back to the loop's view.
    Ok(if status >= 0 { status } else { code })
}

fn parse_args(args: &[String]) -> Result<(String, Vec<String>), String> {
    let mut out_path = "recording.castx".to_string();
    let mut via: Option<Via> = None;
    let mut host: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                out_path = args
                    .get(i + 1)
                    .ok_or("--out requires a value")?
                    .clone();
                i += 2;
            }
            "--via" => {
                via = Some(match args.get(i + 1).map(String::as_str) {
                    Some("posh") => Via::Posh,
                    Some("ssh") => Via::Ssh,
                    other => {
                        return Err(format!("--via expects posh|ssh, got {other:?}\n\n{USAGE}"))
                    }
                });
                i += 2;
            }
            "--host" => {
                host = Some(args.get(i + 1).ok_or("--host requires a value")?.clone());
                i += 2;
            }
            "-h" | "--help" => return Err(USAGE.to_string()),
            "--" => {
                let command = args[i + 1..].to_vec();
                if command.is_empty() {
                    return Err(format!("record requires a command after `--`\n\n{USAGE}"));
                }
                return Ok((out_path, wrap_transport(via, host, command)?));
            }
            other => return Err(format!("unexpected argument {other:?}\n\n{USAGE}")),
        }
    }
    Err(format!("record requires `-- <cmd>`\n\n{USAGE}"))
}

/// Wrap `cmd` so it runs over a remote transport: `--via posh` becomes
/// `posh HOST -- <cmd>` (roaming; pass HOST:session for a persistent session),
/// `--via ssh` becomes `ssh -t HOST <cmd>` (no posh in the loop). Without
/// `--via`/`--host` the command is returned unchanged. The two flags are all or
/// nothing — one without the other is a usage error.
fn wrap_transport(
    via: Option<Via>,
    host: Option<String>,
    cmd: Vec<String>,
) -> Result<Vec<String>, String> {
    match (via, host) {
        (None, None) => Ok(cmd),
        (Some(_), None) | (None, Some(_)) => {
            Err(format!("--via and --host must be given together\n\n{USAGE}"))
        }
        (Some(Via::Posh), Some(host)) => {
            let mut argv = vec!["posh".to_string(), host, "--".to_string()];
            argv.extend(cmd);
            Ok(argv)
        }
        (Some(Via::Ssh), Some(host)) => {
            let mut argv = vec!["ssh".to_string(), "-t".to_string(), host];
            argv.extend(cmd);
            Ok(argv)
        }
    }
}

/// Copy bytes between the local terminal and the PTY master until the child's
/// output ends, recording output (`o`), input (`i`), and resizes (`r`).
fn tee_loop(rec: &mut Recorder<impl std::io::Write>, master: RawFd, start: Instant) -> i32 {
    let mut buf = [0u8; 4096];
    let mut stdin_open = true;
    loop {
        if SIGWINCH.swap(false, Ordering::AcqRel) {
            let (rows, cols) = term_size(STDOUT_FD);
            set_term_size(master, rows, cols);
            let _ = rec.resize(start.elapsed().as_secs_f64(), cols, rows);
        }

        let mut fds = [
            libc::pollfd {
                fd: if stdin_open { STDIN_FD } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: master,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: fds is a valid, sized array for the duration of the call.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue; // SIGWINCH (or similar) — re-check flags
            }
            break;
        }

        // PTY master -> stdout (tee) + record as output.
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match read_fd(master, &mut buf) {
                Ok(0) => break, // child closed the slave
                Ok(k) => {
                    write_all(STDOUT_FD, &buf[..k]);
                    let _ = rec.output(start.elapsed().as_secs_f64(), &buf[..k]);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break, // EIO once the slave is gone
            }
        }

        // stdin -> PTY master + record as input.
        if stdin_open && fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match read_fd(STDIN_FD, &mut buf) {
                Ok(0) => stdin_open = false, // EOF: stop forwarding, keep teeing
                Ok(k) => {
                    write_all(master, &buf[..k]);
                    let _ = rec.input(start.elapsed().as_secs_f64(), &buf[..k]);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => stdin_open = false,
            }
        }
    }
    0
}

// --- PTY + window size (mirrors crates/posh/src/pty.rs) ---------------------

fn term_size(fd: RawFd) -> (u16, u16) {
    // SAFETY: TIOCGWINSZ writes through a valid &mut winsize.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
            return (ws.ws_row, ws.ws_col);
        }
    }
    (24, 80)
}

fn set_term_size(fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads from a valid &winsize.
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

struct Pty {
    master: RawFd,
    pid: libc::pid_t,
}

/// Open a PTY, fork, and exec `command` on the slave side as a session leader.
fn spawn(command: &[String], rows: u16, cols: u16) -> std::io::Result<Pty> {
    use std::ffi::CString;

    // Allocate everything the child needs before fork(): no allocation may
    // happen between fork and exec.
    let cstr = |s: &str| CString::new(s).map_err(std::io::Error::other);
    let path = cstr(&command[0])?;
    let argv_owned: Vec<CString> = command.iter().map(|a| cstr(a)).collect::<Result<_, _>>()?;
    let mut argv: Vec<*const libc::c_char> = argv_owned.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());

    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: pointers (argv, path) outlive the calls; the forked child runs
    // only async-signal-safe functions and never allocates before execvp.
    unsafe {
        // openpty's termios/winsize params are *const on Linux, *mut on macOS;
        // cast so each platform's signature resolves (ADR-0001).
        if libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null::<libc::termios>() as *mut _,
            &ws as *const _ as *mut _,
        ) < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let pid = libc::fork();
        if pid < 0 {
            libc::close(master);
            libc::close(slave);
            return Err(std::io::Error::last_os_error());
        }
        if pid == 0 {
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            libc::close(master);
            libc::execvp(path.as_ptr(), argv.as_ptr());
            libc::_exit(127); // exec failed
        }
        libc::close(slave);
        Ok(Pty { master, pid })
    }
}

fn wait_child(pid: libc::pid_t) -> i32 {
    let mut status: libc::c_int = 0;
    // SAFETY: status is a valid &mut c_int.
    let r = unsafe { libc::waitpid(pid, &mut status, 0) };
    if r < 0 {
        return -1;
    }
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        -1
    }
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    // SAFETY: read into a valid, sized buffer.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn write_all(fd: RawFd, mut data: &[u8]) {
    while !data.is_empty() {
        // SAFETY: write from a valid, sized slice.
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n <= 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break; // best-effort tee; a closed stdout shouldn't abort recording
        }
        data = &data[n as usize..];
    }
}

// --- raw mode (best-effort) + SIGWINCH --------------------------------------

/// RAII raw-mode guard. `enable` returns `None` when `fd` isn't a tty, so a
/// redirected stdin simply isn't put in raw mode.
struct RawMode {
    fd: RawFd,
    orig: libc::termios,
}

impl RawMode {
    fn enable(fd: RawFd) -> Option<RawMode> {
        // SAFETY: tcgetattr writes through a valid &mut termios; tcsetattr
        // reads from valid references.
        unsafe {
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut orig) != 0 {
                return None; // not a terminal
            }
            let mut raw = orig;
            libc::cfmakeraw(&mut raw);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawMode { fd, orig })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: tcsetattr reads from a valid &termios.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.orig);
        }
    }
}

static SIGWINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigwinch(_: libc::c_int) {
    SIGWINCH.store(true, Ordering::Release);
}

fn install_sigwinch() {
    // SAFETY: sigaction is zero-initialized then fully set; the handler only
    // stores to an AtomicBool (async-signal-safe). No SA_RESTART so poll()
    // returns EINTR and the loop notices the flag.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigwinch as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_via_passes_the_command_through() {
        let (out, cmd) = parse_args(&argv(&["--", "echo", "hi"])).unwrap();
        assert_eq!(out, "recording.castx");
        assert_eq!(cmd, argv(&["echo", "hi"]));
    }

    #[test]
    fn via_posh_wraps_as_a_roaming_shell() {
        let (out, cmd) = parse_args(&argv(&[
            "--out", "r.castx", "--via", "posh", "--host", "box", "--", "posht", "--altscroll",
        ]))
        .unwrap();
        assert_eq!(out, "r.castx");
        assert_eq!(cmd, argv(&["posh", "box", "--", "posht", "--altscroll"]));
    }

    #[test]
    fn via_ssh_wraps_with_a_tty_and_no_dashdash() {
        let (_out, cmd) =
            parse_args(&argv(&["--via", "ssh", "--host", "user@box", "--", "posht"])).unwrap();
        assert_eq!(cmd, argv(&["ssh", "-t", "user@box", "posht"]));
    }

    #[test]
    fn via_without_host_is_a_usage_error() {
        assert!(parse_args(&argv(&["--via", "posh", "--", "posht"])).is_err());
    }

    #[test]
    fn host_without_via_is_a_usage_error() {
        assert!(parse_args(&argv(&["--host", "box", "--", "posht"])).is_err());
    }

    #[test]
    fn unknown_via_value_is_rejected() {
        assert!(parse_args(&argv(&["--via", "telnet", "--host", "box", "--", "posht"])).is_err());
    }
}
