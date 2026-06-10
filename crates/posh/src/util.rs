//! Shared plumbing: error type, percent-encoding of session names, poll()
//! wrappers, fd helpers, signal flags, daemonization, and file logging.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error(e.to_string())
    }
}

impl From<String> for Error {
    fn from(s: String) -> Error {
        Error(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Error {
        Error(s.to_string())
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
    Err(Error(format!(
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
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(rc as usize)
    }
}

pub fn read_fd(fd: RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

pub fn write_fd(fd: RawFd, buf: &[u8]) -> std::io::Result<usize> {
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
pub fn write_all_retry(fd: RawFd, mut data: &[u8], max_wait_ms: u64) -> std::io::Result<()> {
    let start = now_ms();
    while !data.is_empty() {
        match write_fd(fd, data) {
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if now_ms().saturating_sub(start) >= max_wait_ms {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                let mut fds = [pollfd(fd, libc::POLLOUT)];
                let _ = poll(&mut fds, 10);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
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

extern "C" fn on_sigwinch(_: libc::c_int) {
    SIGWINCH_RECEIVED.store(true, Ordering::Release);
}

extern "C" fn on_sigterm(_: libc::c_int) {
    SIGTERM_RECEIVED.store(true, Ordering::Release);
}

extern "C" fn on_sigusr1(_: libc::c_int) {
    SIGUSR1_RECEIVED.store(true, Ordering::Release);
}

fn install_handler(signo: libc::c_int, handler: usize) {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler;
        libc::sigemptyset(&mut sa.sa_mask);
        // No SA_RESTART: poll() must return EINTR so loops notice the flag.
        sa.sa_flags = 0;
        libc::sigaction(signo, &sa, std::ptr::null_mut());
    }
}

pub fn install_sigwinch_handler() {
    install_handler(
        libc::SIGWINCH,
        on_sigwinch as extern "C" fn(libc::c_int) as usize,
    );
}

pub fn install_sigterm_handler() {
    install_handler(
        libc::SIGTERM,
        on_sigterm as extern "C" fn(libc::c_int) as usize,
    );
}

pub fn install_sigusr1_handler() {
    install_handler(
        libc::SIGUSR1,
        on_sigusr1 as extern "C" fn(libc::c_int) as usize,
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

pub fn log_init(path: &Path) -> Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    *LOGGER.lock().unwrap() = Some(LogFile {
        file,
        path: path.to_path_buf(),
        size,
    });
    Ok(())
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
        if let Ok(file) = OpenOptions::new().create(true).append(true).open(&log.path) {
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
