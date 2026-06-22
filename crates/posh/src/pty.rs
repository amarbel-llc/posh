//! PTY allocation, shell spawning, window-size ioctls, and tty raw mode.

use std::ffi::CString;
use std::os::fd::RawFd;

use crate::util::{Error, Result};

pub fn term_size(fd: RawFd) -> (u16, u16) {
    // SAFETY: TIOCGWINSZ writes through a valid &mut winsize.
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
    // SAFETY: TIOCSWINSZ reads from a valid &winsize.
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

/// Whether the tty's line discipline currently echoes input (`c_lflag & ECHO`).
/// Reading `c_lflag` off the pty master reflects the slave's termios on Linux,
/// so the server can tell an optimistic-echo client when local echo is safe
/// (FDR 0006). `false` when `fd` is not a tty.
pub fn echo_on(fd: RawFd) -> bool {
    // SAFETY: tcgetattr writes through a valid &mut termios before it is read.
    unsafe {
        let mut tio: libc::termios = std::mem::zeroed();
        libc::tcgetattr(fd, &mut tio) == 0 && tio.c_lflag & libc::ECHO != 0
    }
}

pub struct PtyChild {
    pub master: RawFd,
    pub pid: libc::pid_t,
}

/// Opens a PTY and forks; the child becomes session leader on the slave side
/// and execs `command` (or, when None, `$SHELL` as a login shell with a
/// "-"-prefixed argv[0], the traditional signal). `extra_env` entries are
/// exported into the child environment. When `cwd` is set the child `chdir`s
/// into it before exec (a failure is non-fatal — the child keeps the inherited
/// cwd — so a stale OSC-7 path can't strand the shell). Used by the escape-to-
/// shell overlay (FDR 0008) to land the shell in the session's working dir.
pub fn spawn_shell(
    command: Option<&[String]>,
    rows: u16,
    cols: u16,
    extra_env: &[(String, String)],
    cwd: Option<&str>,
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
    // Pre-allocate the cwd CString before fork(); the child only dereferences
    // the raw pointer (chdir is async-signal-safe).
    let cwd_owned: Option<CString> = match cwd {
        Some(d) => Some(CString::new(d).map_err(|e| Error(e.to_string()))?),
        None => None,
    };
    let cwd_ptr: *const libc::c_char =
        cwd_owned.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());

    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: all pointers passed below (argv, env, paths) outlive the
    // calls; the forked child touches only async-signal-safe functions
    // (setsid/ioctl/dup2/close/putenv-of-preallocated/execvp/_exit) — no
    // allocation happens between fork and exec.
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
            // Land in the requested working directory (the session's OSC-7 cwd
            // for the escape-to-shell overlay). Non-fatal: on failure the child
            // keeps the inherited cwd rather than refusing to start.
            if !cwd_ptr.is_null() {
                libc::chdir(cwd_ptr);
            }
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

pub struct PtyControlChild {
    pub master: RawFd,
    /// Host end of the control socket; the child sees its peer as fd 3.
    pub control: RawFd,
    pub pid: libc::pid_t,
}

/// Like `spawn_shell` but for a co-process that needs a private control channel
/// alongside its terminal: opens a PTY *and* a `socketpair`, forks, and in the
/// child dup2's the child socket end onto fd 3 before exec'ing `bin` (no login
/// shell, no env/cwd massaging). Used to host the command-palette renderer
/// (RFC 0005): the renderer draws to the PTY and speaks JSON-RPC on fd 3.
pub fn spawn_with_control(bin: &CString, rows: u16, cols: u16) -> Result<PtyControlChild> {
    let argv: [*const libc::c_char; 2] = [bin.as_ptr(), std::ptr::null()];
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut sp: [libc::c_int; 2] = [-1, -1];
    // SAFETY: all pointers passed to the libc calls are valid for the call;
    // the forked child touches only async-signal-safe functions
    // (setsid/ioctl/dup2/close/execv/_exit) with no allocation between fork and
    // exec (argv/bin were built before the fork).
    unsafe {
        if libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sp.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let (host_ctrl, child_ctrl) = (sp[0], sp[1]);
        // openpty's termios/winsize params are *const on Linux, *mut on
        // macOS/BSD; cast so each platform's signature resolves (see spawn_shell).
        if libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null::<libc::termios>() as *mut _,
            &ws as *const _ as *mut _,
        ) < 0
        {
            let e = std::io::Error::last_os_error();
            libc::close(host_ctrl);
            libc::close(child_ctrl);
            return Err(e.into());
        }
        let pid = libc::fork();
        if pid < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(master);
            libc::close(slave);
            libc::close(host_ctrl);
            libc::close(child_ctrl);
            return Err(e.into());
        }
        if pid == 0 {
            // Child: slave PTY becomes the controlling terminal on stdio, and
            // the child's socket end is moved to fd 3 (where the renderer reads
            // JSON-RPC). dup2 atomically closes any fd already at 3.
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            libc::close(master);
            libc::close(host_ctrl);
            if child_ctrl != 3 {
                libc::dup2(child_ctrl, 3);
                libc::close(child_ctrl);
            }
            // Close the redundant slave ref unless it is now stdio or the
            // control fd (slave == 3 means dup2 above already repurposed it).
            if slave > 2 && slave != 3 {
                libc::close(slave);
            }
            libc::execv(bin.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
        libc::close(slave);
        libc::close(child_ctrl);
        Ok(PtyControlChild {
            master,
            control: host_ctrl,
            pid,
        })
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
    // SAFETY: cfmakeraw mutates a valid &mut termios in place.
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
        // SAFETY: tcgetattr writes through a valid &mut termios before it
        // is read; tcsetattr reads from a valid reference.
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
        // SAFETY: tcsetattr reads from a valid &termios.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
        }
    }

    /// Re-enters raw mode after [`RawMode::restore`] (resume from suspend).
    pub fn reapply(&self) {
        // SAFETY: tcsetattr reads from a valid temporary termios.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &raw_termios(&self.orig));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::FromRawFd;

    /// `spawn_shell(cwd=...)` lands the child in that directory: run `pwd -P`
    /// (physical, getcwd-based, so it reflects the chdir rather than an inherited
    /// $PWD) and confirm it prints the requested dir. /bin/sh is guaranteed in
    /// the nix build sandbox.
    #[test]
    fn spawn_shell_honors_cwd() {
        let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let dir_str = dir.to_str().unwrap().to_string();
        let cmd = vec!["/bin/sh".to_string(), "-c".to_string(), "pwd -P".to_string()];
        let child = spawn_shell(Some(&cmd), 24, 80, &[], Some(&dir_str)).unwrap();
        // Blocking read to EOF: the child prints once and exits, closing the
        // slave, so the master sees EOF. from_raw_fd owns + closes the master.
        let mut f = unsafe { std::fs::File::from_raw_fd(child.master) };
        let mut out = String::new();
        let _ = f.read_to_string(&mut out);
        assert!(
            out.contains(&dir_str),
            "pwd -P output {out:?} should contain the spawn cwd {dir_str:?}"
        );
    }
}
