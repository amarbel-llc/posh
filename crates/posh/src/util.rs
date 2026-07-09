//! Shared plumbing: error type, percent-encoding of session names, poll()
//! wrappers, fd helpers, signal flags, daemonization, and file logging.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub type Result<T> = std::result::Result<T, Error>;

/// The crate error: a plain message, or an I/O error kept whole so callers
/// can branch on `io::ErrorKind` (#120) instead of string-matching — e.g.
/// distinguishing NotFound (benign "not created yet") from PermissionDenied
/// at a stat site. Display output is identical to the old stringly type.
/// (posh-proto keeps its own string-tuple Error: protocol decode errors carry
/// no I/O kind — a deliberate divergence, not drift.)
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Msg(String),
}

impl Error {
    /// The underlying I/O error kind, when there is one. Production call sites
    /// mostly match on `Error::Io(e)` directly (e.g. `validate_session_dir`'s
    /// NotFound gate matches before wrapping); this accessor serves assertions
    /// and future callers holding an already-wrapped Error.
    #[allow(dead_code)]
    pub fn kind(&self) -> Option<std::io::ErrorKind> {
        match self {
            Error::Io(e) => Some(e.kind()),
            Error::Msg(_) => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => e.fmt(f),
            Error::Msg(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Msg(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Error {
        Error::Msg(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Error {
        Error::Msg(s.to_string())
    }
}

// Bridge posh-proto's mirror error into this crate's error, so the frame
// decode paths that moved into posh-proto (github #75) keep flowing through
// `?` at posh-side call sites (e.g. `ClientMessage::decode` in remote::sync).
impl From<posh_proto::Error> for Error {
    fn from(e: posh_proto::Error) -> Error {
        Error::Msg(e.0)
    }
}

/// Milliseconds since process start (monotonic).
pub fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

// ---------------------------------------------------------------------------
// Session-name percent-encoding (zmx semantics: only bytes that are unsafe in
// a single filename component are escaped).

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn filename_safe(ch: u8) -> bool {
    ch != b'/' && ch != b'\\' && ch != b'%' && ch != 0
}

pub fn encode_session_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for &b in name.as_bytes() {
        if filename_safe(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

pub fn decode_session_name(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Locale checks (mosh locale_utils.cc port): posh moves raw UTF-8 bytes
// between the endpoints, so both sides must run a UTF-8 charset.

/// The variable controlling LC_CTYPE: LC_ALL > LC_CTYPE > LANG. Returns
/// (variable name, value); empty name when none is set.
pub fn ctype_locale(
    lc_all: Option<&str>,
    lc_ctype: Option<&str>,
    lang: Option<&str>,
) -> (String, String) {
    for (name, value) in [("LC_ALL", lc_all), ("LC_CTYPE", lc_ctype), ("LANG", lang)] {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            return (name.to_string(), v.to_string());
        }
    }
    (String::new(), String::new())
}

/// Charset implied by a locale value: the codeset after '.', with any
/// '@modifier' stripped. Bare "C"/"POSIX" (and unset) mean US-ASCII.
pub fn locale_charset(value: &str) -> String {
    let v = value.split('@').next().unwrap_or(value);
    let cs = match v.split_once('.') {
        Some((_, cs)) => cs,
        None => v,
    };
    match cs {
        "" | "C" | "POSIX" | "ANSI_X3.4-1968" => "US-ASCII".to_string(),
        other => other.to_string(),
    }
}

pub fn charset_is_utf8(charset: &str) -> bool {
    charset.eq_ignore_ascii_case("UTF-8") || charset.eq_ignore_ascii_case("utf8")
}

/// Refuses to run without a UTF-8 charset, with the mosh-style explanation.
pub fn check_utf8_locale(program: &str) -> Result<()> {
    let env = |k: &str| std::env::var(k).ok();
    let (name, value) = ctype_locale(
        env("LC_ALL").as_deref(),
        env("LC_CTYPE").as_deref(),
        env("LANG").as_deref(),
    );
    let charset = locale_charset(&value);
    if charset_is_utf8(&charset) {
        return Ok(());
    }
    let var = if name.is_empty() {
        "[no charset variables]".to_string()
    } else {
        format!("{name}={value}")
    };
    Err(Error::Msg(format!(
        "{program} needs a UTF-8 native locale to run.\n\n\
         Unfortunately, the environment ({var}) specifies\n\
         the character set \"{charset}\"."
    )))
}

// ---------------------------------------------------------------------------
// poll() and raw-fd helpers

pub fn pollfd(fd: RawFd, events: i16) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

/// Thin poll(2) wrapper. EINTR is surfaced as ErrorKind::Interrupted so event
/// loops can re-check their signal flags.
pub fn poll(fds: &mut [libc::pollfd], timeout_ms: i32) -> std::io::Result<usize> {
    // SAFETY: pointer and length come from the same live slice.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(rc as usize)
    }
}

pub fn read_fd(fd: RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    // SAFETY: pointer and length come from the same live slice.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

pub fn write_fd(fd: RawFd, buf: &[u8]) -> std::io::Result<usize> {
    // SAFETY: pointer and length come from the same live slice.
    let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// Write all of `data`, waiting up to `max_wait_ms` for a non-blocking fd to
/// drain. Bytes that still cannot be written when the budget runs out are
/// dropped (the daemon must not wedge on a stopped PTY reader).
///
/// Returns the number of bytes actually written. `Ok(n)` with `n < data.len()`
/// means the budget expired and `data.len() - n` bytes were DROPPED — callers
/// that model a differential surface (the render path) must treat a short write
/// as a desync and force a full repaint (see `remote::client::render_to`).
/// `Ok(data.len())` means the whole buffer drained. A real I/O error is still
/// returned as `Err`.
pub fn write_all_retry(fd: RawFd, data: &[u8], max_wait_ms: u64) -> std::io::Result<usize> {
    let start = now_ms();
    let mut rest = data;
    while !rest.is_empty() {
        match write_fd(fd, rest) {
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if now_ms().saturating_sub(start) >= max_wait_ms {
                    // Budget spent: report how much made it out (the rest is
                    // dropped) rather than an opaque TimedOut that hides the loss.
                    return Ok(data.len() - rest.len());
                }
                let mut fds = [pollfd(fd, libc::POLLOUT)];
                let _ = poll(&mut fds, 10);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(data.len())
}

// ---------------------------------------------------------------------------
// Process control — the safe surface over the scattered libc FFI (github
// #36). Call sites outside this module and pty.rs should use these rather
// than raw libc.

/// Sends `signal` to `pid`'s whole process group.
pub fn kill_pgroup(pid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: kill(2) takes two plain integers; a negative pid addresses
    // the process group.
    unsafe { libc::kill(-pid, signal) };
}

/// Stops our own process group (job-control suspend; SIGCONT resumes).
pub fn stop_own_pgroup() {
    // SAFETY: kill(2) with pid 0 signals the caller's own process group.
    unsafe { libc::kill(0, libc::SIGSTOP) };
}

/// Non-blocking reap: `Some(raw wait status)` when `pid` was reaped.
pub fn try_reap(pid: libc::pid_t) -> Option<libc::c_int> {
    let mut status = 0;
    // SAFETY: waitpid(2) writes the status through a valid &mut int.
    if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
        Some(status)
    } else {
        None
    }
}

/// Blocking reap; returns the raw wait status (0 when nothing was reaped).
pub fn reap(pid: libc::pid_t) -> libc::c_int {
    let mut status = 0;
    // SAFETY: as try_reap.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    status
}

/// Shell-style exit code from a raw wait status: WEXITSTATUS, or
/// 128+signal when signaled.
pub fn exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        0
    }
}

pub fn close_fd(fd: RawFd) {
    // SAFETY: close(2) on a plain integer fd.
    unsafe { libc::close(fd) };
}

/// Real uid of the process (cannot fail).
pub fn uid() -> u32 {
    // SAFETY: getuid(2) takes no arguments and always succeeds.
    unsafe { libc::getuid() }
}

/// Whether `fd` refers to a terminal.
pub fn is_tty(fd: RawFd) -> bool {
    // SAFETY: isatty(2) on a plain integer fd.
    (unsafe { libc::isatty(fd) }) == 1
}

pub fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: two fcntl(2) calls on a plain integer fd.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Signal flags

pub static SIGWINCH_RECEIVED: AtomicBool = AtomicBool::new(false);
pub static SIGTERM_RECEIVED: AtomicBool = AtomicBool::new(false);
pub static SIGUSR1_RECEIVED: AtomicBool = AtomicBool::new(false);
pub static SIGUSR2_RECEIVED: AtomicBool = AtomicBool::new(false);
pub static SIGCONT_RECEIVED: AtomicBool = AtomicBool::new(false);

/// The signal number that last requested a terminating shutdown, or 0 if the
/// terminate flag was set by something other than a caught signal. The daemon
/// reads this when `SIGTERM_RECEIVED` fires so its teardown log can name the
/// actual signal (SIGTERM vs SIGHUP vs SIGINT) — the difference between the
/// three was invisible before, which made a daemon that vanished on an uncaught
/// SIGHUP/SIGINT indistinguishable from one killed by SIGKILL (posh#136-adjacent
/// silent-death investigation).
pub static LAST_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_sigwinch(_: libc::c_int) {
    SIGWINCH_RECEIVED.store(true, Ordering::Release);
}

extern "C" fn on_sigterm(_: libc::c_int) {
    SIGTERM_RECEIVED.store(true, Ordering::Release);
}

/// Terminating-signal handler that also records WHICH signal fired, so the
/// consumer can log it. Routes SIGTERM/SIGHUP/SIGINT to the same terminate
/// flag (all three mean "wind down") while preserving the signo in LAST_SIGNAL.
extern "C" fn on_terminating_signal(signo: libc::c_int) {
    LAST_SIGNAL.store(signo, Ordering::Release);
    SIGTERM_RECEIVED.store(true, Ordering::Release);
}

extern "C" fn on_sigusr1(_: libc::c_int) {
    SIGUSR1_RECEIVED.store(true, Ordering::Release);
}

extern "C" fn on_sigusr2(_: libc::c_int) {
    SIGUSR2_RECEIVED.store(true, Ordering::Release);
}

extern "C" fn on_sigcont(_: libc::c_int) {
    SIGCONT_RECEIVED.store(true, Ordering::Release);
}

fn install_handler(signo: libc::c_int, handler: usize) {
    // SAFETY: sigaction is zero-initialized then fully set; every handler
    // routed here only stores to an AtomicBool (async-signal-safe).
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler;
        libc::sigemptyset(&mut sa.sa_mask);
        // No SA_RESTART: poll() must return EINTR so loops notice the flag.
        sa.sa_flags = 0;
        libc::sigaction(signo, &sa, std::ptr::null_mut());
    }
}

/// Daemon-side terminating-signal wiring: catches SIGTERM, SIGHUP, and SIGINT,
/// routing all three to `SIGTERM_RECEIVED` (they all mean "wind down") while
/// recording WHICH signal fired in `LAST_SIGNAL`, so a daemon that winds down
/// names the cause in its teardown log instead of dying silently under the
/// default disposition. The daemon is a `setsid` session leader with no
/// controlling terminal, so it should never legitimately receive SIGHUP/SIGINT
/// — catching them here does not change that a terminating signal shuts the
/// daemon down (posh#136 chose to make the death VISIBLE before deciding whether
/// to make it survivable), it only makes the event observable.
pub fn install_daemon_signal_handlers() {
    let handler = on_terminating_signal as extern "C" fn(libc::c_int) as usize;
    for signo in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        install_handler(signo, handler);
    }
}

/// Human-readable name for a terminating signal number, for log lines.
pub fn signal_name(signo: libc::c_int) -> &'static str {
    match signo {
        libc::SIGTERM => "SIGTERM",
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        _ => "signal",
    }
}

pub fn install_sigusr1_handler() {
    install_handler(
        libc::SIGUSR1,
        on_sigusr1 as extern "C" fn(libc::c_int) as usize,
    );
}

/// SIGUSR2 flags a one-shot transport-state dump (remote/diag.rs): the roaming
/// server and client each snapshot their live transport state to the
/// POSH_DEBUG_LOG sink (or a default per-pid file) on the next loop iteration.
/// Installed individually by the roaming server and roaming client — NOT in the
/// shared client bundle, since the local session-attach client never consumes
/// the flag.
pub fn install_sigusr2_handler() {
    install_handler(
        libc::SIGUSR2,
        on_sigusr2 as extern "C" fn(libc::c_int) as usize,
    );
}

/// Client-side signal wiring (mosh stmclient): SIGWINCH flags a resize;
/// SIGTERM, SIGINT, and SIGHUP all route to SIGTERM_RECEIVED so the loop
/// winds down and restores the tty (raw mode clears ISIG, but kill(1) and
/// terminal hangup would otherwise terminate with the default disposition
/// mid-raw); SIGCONT sets SIGCONT_RECEIVED so the screen repaints after
/// SIGSTOP/fg.
pub fn install_client_signal_handlers() {
    install_handler(
        libc::SIGWINCH,
        on_sigwinch as extern "C" fn(libc::c_int) as usize,
    );
    let on_term = on_sigterm as extern "C" fn(libc::c_int) as usize;
    for signo in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        install_handler(signo, on_term);
    }
    install_handler(
        libc::SIGCONT,
        on_sigcont as extern "C" fn(libc::c_int) as usize,
    );
}

pub fn ignore_signal(signo: libc::c_int) {
    install_handler(signo, libc::SIG_IGN);
}

pub fn take_flag(flag: &AtomicBool) -> bool {
    flag.swap(false, Ordering::AcqRel)
}

// ---------------------------------------------------------------------------
// Daemonization

/// Double-fork-and-setsid. Returns true in the original (calling) process
/// after the intermediate child has been reaped; the grandchild (the daemon)
/// gets false. The intermediate process never returns.
pub fn double_fork() -> Result<bool> {
    // SAFETY: the process is single-threaded at every call site (daemon and
    // server startup, before any event loop), so fork(2) is not racing
    // allocator or lock state; the intermediate child only calls
    // async-signal-safe functions before _exit.
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if pid > 0 {
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
            return Ok(true);
        }
        // Intermediate child: new session, then fork the real daemon.
        libc::setsid();
        let pid2 = libc::fork();
        if pid2 != 0 {
            libc::_exit(0);
        }
    }
    Ok(false)
}

pub fn redirect_stdio_devnull() {
    // SAFETY: open(2)/dup2(2)/close(2) on integer fds; the path is a
    // static NUL-terminated literal.
    unsafe {
        let nullfd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if nullfd >= 0 {
            libc::dup2(nullfd, 0);
            libc::dup2(nullfd, 1);
            libc::dup2(nullfd, 2);
            if nullfd > 2 {
                libc::close(nullfd);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// File logging with simple size rotation (ported from zmx log.zig: 5MB cap,
// current file renamed to "<path>.old" on rotation).

const LOG_MAX_SIZE: u64 = 5 * 1024 * 1024;

struct LogFile {
    file: File,
    path: PathBuf,
    size: u64,
}

static LOGGER: Mutex<Option<LogFile>> = Mutex::new(None);

/// Open `path` for appending, creating it PRIVATE (0600) if absent (#118):
/// diagnostic sinks carry terminal titles and transport state, so they must
/// not be born world-readable under a permissive umask. An existing file
/// keeps its mode (`mode()` applies at creation only).
pub(crate) fn open_private_append(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

/// Write `data` to a PRIVATE (0600) file, truncating any existing one (#118).
/// For diagnostic payloads — forensic bundles — that contain raw screen bytes
/// (whatever was on the terminal: prompts, credentials, output).
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)
}

pub fn log_init(path: &Path) -> Result<()> {
    let file = open_private_append(path)?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    *LOGGER.lock().unwrap() = Some(LogFile {
        file,
        path: path.to_path_buf(),
        size,
    });
    Ok(())
}

/// Whether a log sink has been initialized (POSH_DEBUG_LOG armed it, or a prior
/// `log_init`). The SIGUSR2 state dump uses this to decide whether to reuse the
/// existing sink or lazily open its own default per-pid file.
pub fn log_active() -> bool {
    LOGGER.lock().unwrap().is_some()
}

/// Close the log sink (runtime disable, e.g. the `Ctrl-^ d` toggle). `log_write`
/// silently drops afterward and `log_active` reports false until re-enabled.
pub fn log_disable() {
    *LOGGER.lock().unwrap() = None;
}

/// Logs a line if a log file has been initialized; silently drops otherwise
/// (CLI invocations do not log, only daemons do).
pub fn log_write(level: &str, msg: &str) {
    let mut guard = LOGGER.lock().unwrap();
    let Some(log) = guard.as_mut() else {
        return;
    };
    if log.size >= LOG_MAX_SIZE {
        let old = log.path.with_extension("log.old");
        let _ = std::fs::rename(&log.path, &old);
        if let Ok(file) = open_private_append(&log.path) {
            log.file = file;
            log.size = 0;
        }
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!("[{ts}] [{level}]: {msg}\n");
    if log.file.write_all(line.as_bytes()).is_ok() {
        log.size += line.len() as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_passes_safe_names_through() {
        assert_eq!(encode_session_name("my-session"), "my-session");
    }

    #[test]
    fn encode_escapes_unsafe_bytes() {
        assert_eq!(encode_session_name("projects/web"), "projects%2Fweb");
        assert_eq!(encode_session_name("a/b/c"), "a%2Fb%2Fc");
        assert_eq!(encode_session_name("100%done"), "100%25done");
        assert_eq!(encode_session_name("win\\path"), "win%5Cpath");
    }

    #[test]
    fn decode_reverses_encoding() {
        assert_eq!(decode_session_name("my-session"), "my-session");
        assert_eq!(decode_session_name("projects%2Fweb"), "projects/web");
        assert_eq!(decode_session_name("100%25done"), "100%done");
    }

    #[test]
    fn decode_preserves_invalid_escapes() {
        assert_eq!(decode_session_name("50%"), "50%");
        assert_eq!(decode_session_name("a%zz"), "a%zz");
    }

    #[test]
    fn diagnostic_files_are_created_private() {
        // #118: diagnostic sinks and forensic payloads carry terminal content
        // (raw screen bytes in forensics), so both creation paths must produce
        // 0600 files regardless of umask.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("posh-util-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let w = dir.join("private-write");
        write_private(&w, b"screen bytes").unwrap();
        let mode = std::fs::metadata(&w).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "write_private must create 0600, got {mode:o}");

        let a = dir.join("private-append");
        drop(open_private_append(&a).unwrap());
        let mode = std::fs::metadata(&a).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "open_private_append must create 0600, got {mode:o}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exit_code_maps_wait_status_shell_style() {
        // Raw wait statuses: exit code in bits 8..16, signal in bits 0..7.
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(7 << 8), 7);
        assert_eq!(exit_code(libc::SIGKILL), 128 + libc::SIGKILL);
        assert_eq!(exit_code(libc::SIGTERM), 128 + libc::SIGTERM);
    }

    #[test]
    fn ctype_locale_priority() {
        assert_eq!(
            ctype_locale(Some("C"), Some("en_US.UTF-8"), Some("de_DE.UTF-8")),
            ("LC_ALL".to_string(), "C".to_string())
        );
        assert_eq!(
            ctype_locale(None, Some("en_US.UTF-8"), Some("de_DE.UTF-8")),
            ("LC_CTYPE".to_string(), "en_US.UTF-8".to_string())
        );
        assert_eq!(
            ctype_locale(None, None, Some("de_DE.UTF-8")),
            ("LANG".to_string(), "de_DE.UTF-8".to_string())
        );
        assert_eq!(
            ctype_locale(Some(""), None, None),
            (String::new(), String::new())
        );
    }

    #[test]
    fn locale_charset_extraction() {
        assert_eq!(locale_charset("en_US.UTF-8"), "UTF-8");
        assert_eq!(locale_charset("C.UTF-8"), "UTF-8");
        assert_eq!(locale_charset("de_DE.utf8"), "utf8");
        assert_eq!(locale_charset("ja_JP.eucJP"), "eucJP");
        assert_eq!(locale_charset("de_DE.UTF-8@euro"), "UTF-8");
        assert_eq!(locale_charset("C"), "US-ASCII");
        assert_eq!(locale_charset("POSIX"), "US-ASCII");
        assert_eq!(locale_charset(""), "US-ASCII");
    }

    #[test]
    fn utf8_charset_detection() {
        assert!(charset_is_utf8("UTF-8"));
        assert!(charset_is_utf8("utf-8"));
        assert!(charset_is_utf8("utf8"));
        assert!(charset_is_utf8("UTF8"));
        assert!(!charset_is_utf8("US-ASCII"));
        assert!(!charset_is_utf8("ISO-8859-1"));
    }

    #[test]
    fn encode_decode_roundtrip() {
        for name in [
            "simple",
            "with/slash",
            "multi/level/path",
            "percent%sign",
            "back\\slash",
            "mixed/path%with\\all",
        ] {
            assert_eq!(decode_session_name(&encode_session_name(name)), name);
        }
    }
}
