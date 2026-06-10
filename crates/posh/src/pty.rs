//! PTY allocation, shell spawning, window-size ioctls, and tty raw mode.

use std::ffi::CString;
use std::os::fd::RawFd;

use crate::util::{Error, Result};

pub fn term_size(fd: RawFd) -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
            return (ws.ws_row, ws.ws_col);
        }
    }
    (24, 80)
}

pub fn set_term_size(fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

pub struct PtyChild {
    pub master: RawFd,
    pub pid: libc::pid_t,
}

/// Opens a PTY and forks; the child becomes session leader on the slave side
/// and execs `command` (or, when None, `$SHELL` as a login shell with a
/// "-"-prefixed argv[0], the traditional signal). `extra_env` entries are
/// exported into the child environment.
pub fn spawn_shell(
    command: Option<&[String]>,
    rows: u16,
    cols: u16,
    extra_env: &[(String, String)],
) -> Result<PtyChild> {
    // Everything the child needs is allocated before fork(): allocating in
    // the forked child of a (potentially multi-threaded) process is unsafe.
    let (exec_path, argv_owned) = build_argv(command)?;
    let mut argv: Vec<*const libc::c_char> = argv_owned.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    let env_owned: Vec<CString> = extra_env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")).map_err(|e| Error(e.to_string())))
        .collect::<Result<_>>()?;

    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    unsafe {
        // openpty's termios/winsize params are *const on Linux but *mut on
        // macOS/BSD; cast to let each platform's signature resolve the
        // mutability. See docs/decisions/0001-posh-term-libc-portability.md.
        if libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null::<libc::termios>() as *mut _,
            &ws as *const _ as *mut _,
        ) < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let pid = libc::fork();
        if pid < 0 {
            libc::close(master);
            libc::close(slave);
            return Err(std::io::Error::last_os_error().into());
        }
        if pid == 0 {
            // Child: new session, slave PTY becomes the controlling terminal.
            libc::setsid();
            // ioctl request is c_int-width on Linux, c_ulong on macOS; cast
            // the constant to match.
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            libc::close(master);
            for env in &env_owned {
                // putenv keeps the pointer; the strings stay alive until exec.
                libc::putenv(env.as_ptr() as *mut libc::c_char);
            }
            libc::execvp(exec_path.as_ptr(), argv.as_ptr());
            libc::_exit(1);
        }
        libc::close(slave);
        Ok(PtyChild { master, pid })
    }
}

fn build_argv(command: Option<&[String]>) -> Result<(CString, Vec<CString>)> {
    match command {
        Some(args) if !args.is_empty() => {
            let path = CString::new(args[0].as_str()).map_err(|e| Error(e.to_string()))?;
            let argv = args
                .iter()
                .map(|a| CString::new(a.as_str()).map_err(|e| Error(e.to_string())))
                .collect::<Result<_>>()?;
            Ok((path, argv))
        }
        _ => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            let base = shell.rsplit('/').next().unwrap_or(&shell);
            let path = CString::new(shell.as_str()).map_err(|e| Error(e.to_string()))?;
            let argv0 = CString::new(format!("-{base}")).map_err(|e| Error(e.to_string()))?;
            Ok((path, vec![argv0]))
        }
    }
}

/// RAII raw-mode guard; restores the original termios (with TCSAFLUSH, so
/// unread input is discarded) on drop.
pub struct RawMode {
    fd: RawFd,
    orig: libc::termios,
}

fn raw_termios(orig: &libc::termios) -> libc::termios {
    let mut raw = *orig;
    unsafe { libc::cfmakeraw(&mut raw) };
    // _POSIX_VDISABLE: free Ctrl-V (literal-next) and Ctrl-\ (SIGQUIT)
    // so the latter can be used as the detach key.
    raw.c_cc[libc::VLNEXT] = 0;
    raw.c_cc[libc::VQUIT] = 0;
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    raw
}

impl RawMode {
    pub fn enable(fd: RawFd) -> Result<RawMode> {
        unsafe {
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut orig) != 0 {
                return Err(Error("not a terminal".to_string()));
            }
            if libc::tcsetattr(fd, libc::TCSANOW, &raw_termios(&orig)) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(RawMode { fd, orig })
        }
    }

    /// Temporarily restores the original termios (suspend); pair with
    /// [`RawMode::reapply`] on resume.
    pub fn restore(&self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
        }
    }

    /// Re-enters raw mode after [`RawMode::restore`] (resume from suspend).
    pub fn reapply(&self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &raw_termios(&self.orig));
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.orig);
        }
    }
}
