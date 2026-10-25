//! `poshterity record [--out f.castx] -- <cmd> [args...]`: spawn a command under
//! a PTY, tee its output to the terminal AND to a `.castx` recording.
//!
//! This is the only part of poshterity that needs PTY/libc FFI, so it lives in
//! the **binary** (declared `mod record;` in `main.rs`, never in `lib.rs`),
//! keeping the `#![forbid(unsafe_code)]` library pure. The unsafe syscall
//! patterns mirror the posh binary's `crates/posh/src/pty.rs` (a separate
//! crate, so an own copy — not a shared module — per ADR-0003). Serialization
//! is the safe library's [`poshterity::castx::Recorder`].

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use poshterity::castx::{Header, Poshterity, Recorder};

const STDIN_FD: RawFd = 0;
const STDOUT_FD: RawFd = 1;

const USAGE: &str = "\
usage: poshterity record [--out FILE] [--size COLSxROWS] [--end-on-eof]
                         [--mark-fifo PATH] [--via posh|ssh --host HOST] -- <cmd> [args...]

Spawn <cmd> under a PTY, tee its output to the terminal and to a .castx
recording (default --out recording.castx). Replay it with `poshterity replay`.

  --size COLSxROWS   fix the PTY and the header at this size (default: the
                     terminal's, else 24x80); terminal resizes are then ignored.
  --end-on-eof       when poshterity's own stdin ends, hang the child up and
                     finish the recording (for a piped, scripted session).
  --mark-fifo PATH   create a fifo at PATH; every line written to it becomes a
                     named marker in the recording (replay/step/assert can stop
                     at it). The fifo is removed when recording ends.

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

/// Run `poshterity record`. Returns the child's exit code on success.
pub fn run(args: &[String]) -> Result<i32, String> {
    let Opts { out: out_path, command, size, end_on_eof, mark_fifo } = parse_args(args)?;

    // The recording's size is --size, else the current terminal's (24x80 when
    // stdout is not a tty, e.g. redirected — also how the integration test runs).
    let (rows, cols) = match size {
        Some((cols, rows)) => (rows, cols),
        None => term_size(STDOUT_FD),
    };

    // Created before the child so the path exists the moment recording starts.
    let marks = mark_fifo.as_deref().map(MarkFifo::create).transpose()?;

    let file = std::fs::File::create(&out_path).map_err(|e| format!("{out_path}: {e}"))?;
    let mut rec = Recorder::new(std::io::BufWriter::new(file));
    rec.write_header(&Header {
        version: 2,
        width: cols,
        height: rows,
        poshterity: Some(Poshterity {
            v: 1,
            emu_rev: posh_term::emu_rev(),
        }),
    })
    .map_err(|e| format!("{out_path}: {e}"))?;

    // Best-effort raw mode: a redirected/non-tty stdin just isn't put in raw
    // mode (and isn't forwarded as input). Restored on drop.
    let raw = RawMode::enable(STDIN_FD);
    install_sigwinch();

    let pty = spawn(&command, rows, cols).map_err(|e| format!("spawn {}: {e}", command[0]))?;
    let start = Instant::now();
    let tee = Tee { fixed_size: size.is_some(), end_on_eof, marks: marks.as_ref() };
    let ended = tee_loop(&mut rec, pty.master, start, &tee);
    let code = 0;

    if ended == Ended::StdinEof {
        // --end-on-eof: hang the terminal up. Closing the master sends the
        // child's session SIGHUP; the explicit kill covers a child that is not
        // the foreground process group.
        // SAFETY: closing our own master fd; kill(2) on our own child.
        unsafe {
            libc::close(pty.master);
            libc::kill(pty.pid, libc::SIGHUP);
        }
    }
    let status = wait_child(pty.pid);
    if ended != Ended::StdinEof {
        // SAFETY: closing our own master fd once the loop is done.
        unsafe { libc::close(pty.master) };
    }
    let _ = rec.finish();
    drop(marks); // removes the fifo
    drop(raw); // restore the terminal before printing

    eprintln!("poshterity: recorded → {out_path}");
    // Prefer the child's real exit status; fall back to the loop's view.
    Ok(if status >= 0 { status } else { code })
}

/// Parsed `record` arguments.
#[derive(Debug, PartialEq, Eq)]
struct Opts {
    out: String,
    /// The command to spawn, already wrapped for `--via`.
    command: Vec<String>,
    /// `--size COLSxROWS`: the PTY and the header, whatever the terminal is.
    size: Option<(u16, u16)>,
    /// `--end-on-eof`: hang up the child when poshterity's stdin ends.
    end_on_eof: bool,
    /// `--mark-fifo PATH`: each line written to this fifo becomes a marker.
    mark_fifo: Option<String>,
}

fn parse_size(s: &str) -> Option<(u16, u16)> {
    let (w, h) = s.split_once('x')?;
    let (w, h): (u16, u16) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut out_path = "recording.castx".to_string();
    let mut via: Option<Via> = None;
    let mut host: Option<String> = None;
    let mut size: Option<(u16, u16)> = None;
    let mut end_on_eof = false;
    let mut mark_fifo: Option<String> = None;
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
            "--size" => {
                let v = args.get(i + 1).ok_or("--size requires COLSxROWS")?;
                size = Some(parse_size(v).ok_or_else(|| {
                    format!("--size expects COLSxROWS with both > 0, e.g. 100x30, got {v:?}")
                })?);
                i += 2;
            }
            "--end-on-eof" => {
                end_on_eof = true;
                i += 1;
            }
            "--mark-fifo" => {
                mark_fifo = Some(args.get(i + 1).ok_or("--mark-fifo requires a path")?.clone());
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
                return Ok(Opts {
                    out: out_path,
                    command: wrap_transport(via, host, command)?,
                    size,
                    end_on_eof,
                    mark_fifo,
                });
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

/// Why [`tee_loop`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// The child closed its side of the PTY (it exited).
    ChildClosed,
    /// stdin reached EOF and `--end-on-eof` asked to stop there.
    StdinEof,
}

/// What the tee loop does beyond copying bytes.
struct Tee<'a> {
    /// `--size` was given: terminal resizes do not reach the child.
    fixed_size: bool,
    end_on_eof: bool,
    marks: Option<&'a MarkFifo>,
}

/// A fifo whose lines become `m` marker events (`--mark-fifo`). Opened
/// read-write so the fifo always has a writer (ours) and never reports EOF as
/// outside writers come and go; removed on drop.
struct MarkFifo {
    fd: RawFd,
    path: std::path::PathBuf,
    pending: std::cell::RefCell<Vec<u8>>,
}

impl MarkFifo {
    fn create(path: &str) -> Result<MarkFifo, String> {
        use std::os::unix::ffi::OsStrExt;
        let cpath = std::ffi::CString::new(std::path::Path::new(path).as_os_str().as_bytes())
            .map_err(|_| format!("{path}: path contains a NUL"))?;
        // SAFETY: cpath is a valid NUL-terminated path for both calls.
        let fd = unsafe {
            if libc::mkfifo(cpath.as_ptr(), 0o600) != 0 {
                return Err(format!("{path}: {}", std::io::Error::last_os_error()));
            }
            libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC)
        };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            let _ = std::fs::remove_file(path);
            return Err(format!("{path}: {e}"));
        }
        Ok(MarkFifo { fd, path: path.into(), pending: Default::default() })
    }

    /// Read what is available and return the complete, non-empty lines.
    fn lines(&self) -> Vec<String> {
        let mut buf = [0u8; 1024];
        let mut pending = self.pending.borrow_mut();
        // Non-blocking: take what is there. WouldBlock/EOF end the read.
        while let Ok(n @ 1..) = read_fd(self.fd, &mut buf) {
            pending.extend_from_slice(&buf[..n]);
        }
        let mut out = Vec::new();
        while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=nl).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).trim().to_string();
            if !text.is_empty() {
                out.push(text);
            }
        }
        out
    }
}

impl Drop for MarkFifo {
    fn drop(&mut self) {
        // SAFETY: closing the fd we opened.
        unsafe { libc::close(self.fd) };
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Copy bytes between the local terminal and the PTY master until the child's
/// output ends, recording output (`o`), input (`i`), resizes (`r`) and the
/// markers arriving on `tee.marks` (`m`).
fn tee_loop(rec: &mut Recorder<impl std::io::Write>, master: RawFd, start: Instant, tee: &Tee) -> Ended {
    let mut buf = [0u8; 4096];
    let mut stdin_open = true;
    loop {
        if SIGWINCH.swap(false, Ordering::AcqRel) && !tee.fixed_size {
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
            libc::pollfd {
                fd: tee.marks.map_or(-1, |m| m.fd),
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

        // Marker lines -> `m` events, stamped when they are read.
        if let Some(marks) = tee.marks.filter(|_| fds[2].revents & libc::POLLIN != 0) {
            for name in marks.lines() {
                let _ = rec.marker(start.elapsed().as_secs_f64(), &name);
            }
        }

        // stdin -> PTY master + record as input.
        if stdin_open && fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match read_fd(STDIN_FD, &mut buf) {
                Ok(0) => {
                    // EOF: stop forwarding. With --end-on-eof that ends the
                    // recording (the caller hangs the child up); otherwise keep
                    // teeing until the child exits.
                    if tee.end_on_eof {
                        return Ended::StdinEof;
                    }
                    stdin_open = false;
                }
                Ok(k) => {
                    write_all(master, &buf[..k]);
                    let _ = rec.input(start.elapsed().as_secs_f64(), &buf[..k]);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => stdin_open = false,
            }
        }
    }
    Ended::ChildClosed
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
        let o = parse_args(&argv(&["--", "echo", "hi"])).unwrap();
        assert_eq!(o.out, "recording.castx");
        assert_eq!(o.command, argv(&["echo", "hi"]));
        assert_eq!((o.size, o.end_on_eof, o.mark_fifo), (None, false, None));
    }

    #[test]
    fn via_posh_wraps_as_a_roaming_shell() {
        let o = parse_args(&argv(&[
            "--out", "r.castx", "--via", "posh", "--host", "box", "--", "posht", "--altscroll",
        ]))
        .unwrap();
        assert_eq!(o.out, "r.castx");
        assert_eq!(o.command, argv(&["posh", "box", "--", "posht", "--altscroll"]));
    }

    #[test]
    fn via_ssh_wraps_with_a_tty_and_no_dashdash() {
        let o = parse_args(&argv(&["--via", "ssh", "--host", "user@box", "--", "posht"])).unwrap();
        assert_eq!(o.command, argv(&["ssh", "-t", "user@box", "posht"]));
    }

    #[test]
    fn size_end_on_eof_and_mark_fifo_are_parsed() {
        let o = parse_args(&argv(&[
            "--size", "100x30", "--end-on-eof", "--mark-fifo", "/tmp/m.fifo", "--", "sh",
        ]))
        .unwrap();
        assert_eq!(o.size, Some((100, 30)), "COLSxROWS");
        assert!(o.end_on_eof);
        assert_eq!(o.mark_fifo.as_deref(), Some("/tmp/m.fifo"));
    }

    #[test]
    fn bad_size_is_a_usage_error() {
        for bad in ["100", "x30", "0x30", "100x0", "axb"] {
            assert!(parse_args(&argv(&["--size", bad, "--", "sh"])).is_err(), "{bad}");
        }
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
