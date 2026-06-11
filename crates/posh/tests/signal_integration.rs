//! Signal-handling e2e for both client paths (github #14): SIGTERM must
//! wind the client down cleanly — restore the tty and exit 0 — instead of
//! dying with the default disposition (which leaves the user's shell in
//! raw mode and, on the remote path, the server lingering).

use std::os::fd::{FromRawFd, RawFd};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn posh_cmd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_posh"))
}

/// Per-test POSH_DIR. Unix socket paths cap at ~107 bytes; the deeply
/// nested TMPDIR that `nix develop` exports blows that through temp_dir(),
/// so fall back to /tmp when the base is already long.
fn test_posh_dir(prefix: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir();
    let base = if base.as_os_str().len() > 40 {
        std::path::PathBuf::from("/tmp")
    } else {
        base
    };
    let dir = base.join(format!("{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// posix_openpt master/slave pair; the slave becomes the child's stdio so
/// RawMode::enable and term_size see a real tty. The master is left
/// nonblocking for drain().
fn open_pty_pair() -> (RawFd, RawFd) {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0, "posix_openpt failed");
        assert_eq!(libc::grantpt(master), 0, "grantpt failed");
        assert_eq!(libc::unlockpt(master), 0, "unlockpt failed");
        // ptsname_r (reentrant, buffer-out) is glibc-only; macOS/BSD has only
        // the non-reentrant ptsname(fd) -> *mut c_char returning an internal
        // buffer. Copy that into `name` so the open() below is identical.
        let mut name = [0 as libc::c_char; 128];
        #[cfg(target_os = "linux")]
        assert_eq!(
            libc::ptsname_r(master, name.as_mut_ptr(), name.len()),
            0,
            "ptsname_r failed"
        );
        #[cfg(not(target_os = "linux"))]
        {
            let p = libc::ptsname(master);
            assert!(!p.is_null(), "ptsname failed");
            let len = libc::strlen(p);
            assert!(len < name.len(), "pts name too long for buffer");
            std::ptr::copy_nonoverlapping(p, name.as_mut_ptr(), len);
        }
        let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        assert!(slave >= 0, "open pty slave failed");
        // A real window size: banner/render assertions need columns.
        let ws = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        (master, slave)
    }
}

fn spawn_on_pty(cmd: &mut Command, slave: RawFd) -> Child {
    let stdio = |fd: RawFd| unsafe { Stdio::from_raw_fd(fd) };
    cmd.stdin(stdio(unsafe { libc::dup(slave) }))
        .stdout(stdio(unsafe { libc::dup(slave) }))
        .stderr(stdio(slave))
        .spawn()
        .expect("spawn posh on pty")
}

fn drain_into(master: RawFd, out: &mut Vec<u8>) -> usize {
    let mut total = 0;
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            return total;
        }
        out.extend_from_slice(&buf[..n as usize]);
        total += n as usize;
    }
}

fn drain(master: RawFd) -> usize {
    drain_into(master, &mut Vec::new())
}

fn wait_for_pty_output(master: RawFd, what: &str) {
    for _ in 0..400 {
        if drain(master) > 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

/// Waits for exit while draining the pty so the child never blocks on a
/// full output buffer.
fn wait_for_exit(child: &mut Child, master: RawFd, secs: u64) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        drain(master);
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("client did not exit within {secs}s of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn attach_client_exits_cleanly_on_sigterm() {
    let dir = test_posh_dir("posh-sigtest");

    let out = posh_cmd()
        .args(["attach", "--detach", "sigtest", "sleep", "300"])
        .env("POSH_DIR", &dir)
        .env_remove("POSH_SESSION")
        .env_remove("POSH_GROUP")
        .output()
        .unwrap();
    assert!(out.status.success(), "attach --detach failed: {out:?}");

    let (master, slave) = open_pty_pair();
    let mut cmd = posh_cmd();
    cmd.args(["attach", "sigtest"])
        .env("POSH_DIR", &dir)
        .env_remove("POSH_SESSION")
        .env_remove("POSH_GROUP");
    let mut child = spawn_on_pty(&mut cmd, slave);
    wait_for_pty_output(master, "attach client first output");

    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let status = wait_for_exit(&mut child, master, 10);
    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM must detach cleanly (tty restore runs), got {status:?}"
    );

    let _ = posh_cmd()
        .args(["kill", "sigtest"])
        .env("POSH_DIR", &dir)
        .output();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn attach_client_exits_with_session_exit_status() {
    // github #18: the daemon propagates the shell's waitpid status via a
    // Tag::Exit frame at teardown, and the attach client exits with it.
    let dir = test_posh_dir("posh-exitstatus");

    let (master, slave) = open_pty_pair();
    let mut cmd = posh_cmd();
    cmd.args(["attach", "exitstatus", "sh", "-c", "read x; exit 7"])
        .env("POSH_DIR", &dir)
        .env_remove("POSH_SESSION")
        .env_remove("POSH_GROUP");
    let mut child = spawn_on_pty(&mut cmd, slave);
    wait_for_pty_output(master, "attach client first output");

    // Wake the shell so it exits with code 7.
    let n = unsafe { libc::write(master, b"go\n".as_ptr() as *const libc::c_void, 3) };
    assert_eq!(n, 3, "writing to the pty failed");

    let status = wait_for_exit(&mut child, master, 10);
    assert_eq!(
        status.code(),
        Some(7),
        "client must exit with the session shell's status, got {status:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn attach_takes_over_and_restores_the_alt_screen() {
    // FDR 0002: the attach byte stream begins by entering the outer
    // terminal's alternate screen (the user's shell screen waits
    // underneath) and ends, after the mode resets, by leaving it — so the
    // pre-attach screen comes back exactly as it was.
    let dir = test_posh_dir("posh-takeover");

    let (master, slave) = open_pty_pair();
    let mut cmd = posh_cmd();
    cmd.args(["attach", "takeover", "sh", "-c", "read x; exit 0"])
        .env("POSH_DIR", &dir)
        .env_remove("POSH_SESSION")
        .env_remove("POSH_GROUP");
    let mut child = spawn_on_pty(&mut cmd, slave);

    let mut bytes = Vec::new();
    for _ in 0..400 {
        if drain_into(master, &mut bytes) > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!bytes.is_empty(), "no attach output");

    // Wake the shell so the session ends and the client restores the tty.
    let n = unsafe { libc::write(master, b"go\n".as_ptr() as *const libc::c_void, 3) };
    assert_eq!(n, 3, "writing to the pty failed");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        drain_into(master, &mut bytes);
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("client did not exit after session end");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    drain_into(master, &mut bytes);

    assert!(
        bytes.starts_with(b"\x1b[?1049h\x1b[2J\x1b[H"),
        "attach must start by taking the alt screen, got {:?}",
        String::from_utf8_lossy(&bytes[..bytes.len().min(24)])
    );
    assert!(
        bytes.ends_with(b"\x1b[?1049l\x1b[?25h"),
        "exit must end by leaving the alt screen, got {:?}",
        String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(48)..])
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn argv0_posh_server_dispatches_to_server() {
    // The package installs `bin/posh-server -> posh`; invoked under that
    // name the binary IS the server subcommand (mosh-server parity — this
    // is exactly what the ssh bootstrap runs on the remote host).
    let dir = test_posh_dir("posh-argv0");
    let alias = dir.join("posh-server");
    let _ = std::fs::remove_file(&alias);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_posh"), &alias).unwrap();
    let out = Command::new(&alias)
        .args(["new", "-p", "62700:62799", "--", "sleep", "1"])
        .env("LC_ALL", "C.UTF-8")
        .env("POSH_SERVER_NETWORK_TMOUT", "5")
        .output()
        .unwrap();
    assert!(out.status.success(), "posh-server failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("POSH CONNECT "),
        "no connect line under argv0=posh-server: {stdout:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remote_client_reports_dead_server_and_times_out() {
    // github #31: nothing listening on the port — the client must say so
    // within a moment and give up with a clear error instead of hanging
    // forever. POSH_CONNECT_TMOUT shrinks the 15s default for the test.
    let (master, slave) = open_pty_pair();
    let mut cmd = posh_cmd();
    cmd.args(["client", "127.0.0.1", "62999"])
        .env("LC_ALL", "C.UTF-8")
        .env("POSH_CONNECT_TMOUT", "3")
        .env("POSH_KEY", "AAAAAAAAAAAAAAAAAAAAAA");
    let mut child = spawn_on_pty(&mut cmd, slave);

    let mut pane = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut status = None;
    while Instant::now() < deadline {
        drain_into(master, &mut pane);
        if let Some(s) = child.try_wait().expect("try_wait") {
            status = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    drain_into(master, &mut pane);
    let text = String::from_utf8_lossy(&pane);
    assert!(
        text.contains("Nothing received from server on UDP port 62999"),
        "no early diagnostic banner in pane: {text:?}"
    );
    assert!(
        text.contains("imed out waiting for server"),
        "no timeout message in pane: {text:?}"
    );
    let status = status.expect("client never exited after the connect timeout");
    assert!(!status.success(), "timeout must exit nonzero, got {status:?}");
}

#[test]
fn remote_client_suspends_on_escape_ctrl_z() {
    // mosh parity (github #30): escape-key + Ctrl-Z suspends the *client*
    // (restore tty, SIGSTOP its process group, repaint on resume) instead
    // of forwarding the bytes to the remote shell.
    let out = posh_cmd()
        .args(["server", "-p", "62500:62599", "--", "sleep", "300"])
        .env("LC_ALL", "C.UTF-8")
        .env("POSH_SERVER_NETWORK_TMOUT", "30")
        .output()
        .unwrap();
    assert!(out.status.success(), "server failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let connect = stdout
        .lines()
        .find(|l| l.starts_with("POSH CONNECT "))
        .unwrap_or_else(|| panic!("no POSH CONNECT line in {stdout:?}"));
    let mut fields = connect.split_whitespace().skip(2);
    let port = fields.next().expect("port").to_string();
    let key = fields.next().expect("key").to_string();

    let (master, slave) = open_pty_pair();
    let mut cmd = posh_cmd();
    cmd.args(["client", "127.0.0.1", &port])
        .env("LC_ALL", "C.UTF-8")
        .env("POSH_KEY", key);
    // Own process group: the suspend is kill(0, SIGSTOP) — job-control
    // semantics — and must not stop the test runner's group.
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = spawn_on_pty(&mut cmd, slave);
    let pid = child.id() as libc::pid_t;
    wait_for_pty_output(master, "remote client first paint");

    // Escape key (Ctrl-^) then Ctrl-Z.
    let chord = [0x1eu8, 0x1a];
    let n = unsafe { libc::write(master, chord.as_ptr() as *const libc::c_void, chord.len()) };
    assert_eq!(n, 2, "writing the suspend chord to the pty failed");

    // The client must STOP (not forward the bytes and keep running).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stopped = false;
    while Instant::now() < deadline {
        drain(master);
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
        if r == pid && libc::WIFSTOPPED(status) {
            stopped = true;
            break;
        }
        assert!(
            !(r == pid && libc::WIFEXITED(status)),
            "client exited instead of suspending"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(stopped, "client did not SIGSTOP itself on Ctrl-^ Ctrl-Z");

    // Resume: the client repaints and keeps working.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    wait_for_pty_output(master, "repaint after resume");

    // And still winds down cleanly.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let status = wait_for_exit(&mut child, master, 15);
    assert_eq!(
        status.code(),
        Some(0),
        "clean exit after resume, got {status:?}"
    );
}

#[test]
fn remote_client_exits_cleanly_on_sigterm() {
    let out = posh_cmd()
        .args(["server", "-p", "62300:62399", "--", "sleep", "300"])
        .env("LC_ALL", "C.UTF-8")
        // Hygiene: if the shutdown handshake regresses, the detached
        // server still times itself out instead of lingering.
        .env("POSH_SERVER_NETWORK_TMOUT", "30")
        .output()
        .unwrap();
    assert!(out.status.success(), "server failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let connect = stdout
        .lines()
        .find(|l| l.starts_with("POSH CONNECT "))
        .unwrap_or_else(|| panic!("no POSH CONNECT line in {stdout:?}"));
    let mut fields = connect.split_whitespace().skip(2);
    let port = fields.next().expect("port").to_string();
    let key = fields.next().expect("key").to_string();

    let (master, slave) = open_pty_pair();
    let mut cmd = posh_cmd();
    cmd.args(["client", "127.0.0.1", &port])
        .env("LC_ALL", "C.UTF-8")
        .env("POSH_KEY", key);
    let mut child = spawn_on_pty(&mut cmd, slave);
    wait_for_pty_output(master, "remote client first paint");

    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    // The handler requests a server shutdown; the loop exits once the
    // server acks (well under the 5s grace period on loopback).
    let status = wait_for_exit(&mut child, master, 15);
    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM must wind down via the shutdown handshake, got {status:?}"
    );
}

/// Starts a detached `posh server` with `command`, parses POSH CONNECT, and
/// returns (port, key).
fn start_server(port_range: &str, command: &[&str], envs: &[(&str, &str)]) -> (String, String) {
    let mut cmd = posh_cmd();
    cmd.args(["server", "-p", port_range, "--"])
        .args(command)
        .env("LC_ALL", "C.UTF-8")
        .env("POSH_SERVER_NETWORK_TMOUT", "30");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "server failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let connect = stdout
        .lines()
        .find(|l| l.starts_with("POSH CONNECT "))
        .unwrap_or_else(|| panic!("no POSH CONNECT line in {stdout:?}"));
    let mut fields = connect.split_whitespace().skip(2);
    (
        fields.next().expect("port").to_string(),
        fields.next().expect("key").to_string(),
    )
}

/// Drives a client against `port`, presses Enter to release the remote
/// command, and returns the client's exit status.
fn drive_client_to_exit(port: &str, key: &str) -> std::process::ExitStatus {
    let (master, slave) = open_pty_pair();
    let mut cmd = posh_cmd();
    cmd.args(["client", "127.0.0.1", port])
        .env("LC_ALL", "C.UTF-8")
        .env("POSH_KEY", key);
    let mut child = spawn_on_pty(&mut cmd, slave);
    wait_for_pty_output(master, "remote client first paint");
    let nl = [b'\r'];
    unsafe { libc::write(master, nl.as_ptr() as *const libc::c_void, 1) };
    wait_for_exit(&mut child, master, 20)
}

#[test]
fn remote_exit_status_propagates_over_udp() {
    // RFC 0001 §3 EXIT_STATUS: the server carries the command's
    // shell-style exit code on its shutdown frames and the client process
    // exits with it — `posh box; echo $?` is truthful.
    let (port, key) = start_server("62800:62849", &["/bin/sh", "-c", "read line; exit 7"], &[]);
    let status = drive_client_to_exit(&port, &key);
    assert_eq!(
        status.code(),
        Some(7),
        "remote exit status lost over the transport: {status:?}"
    );
}

#[test]
fn remote_session_attach_carries_exit_status_end_to_end() {
    // The host:session composition (RFC 0001 §2): the transport server
    // wraps an inner `posh attach`; the session shell's code must pass
    // through BOTH layers — the daemon's Exit frame (#18), then the
    // EXIT_STATUS capability over UDP.
    let dir = test_posh_dir("posh-remote-attach");
    let out = posh_cmd()
        .args([
            "attach",
            "--detach",
            "dev",
            "/bin/sh",
            "-c",
            "read line; exit 7",
        ])
        .env("POSH_DIR", &dir)
        .env_remove("POSH_SESSION")
        .env_remove("POSH_GROUP")
        .output()
        .unwrap();
    assert!(out.status.success(), "attach --detach failed: {out:?}");

    let dir_str = dir.to_str().unwrap().to_string();
    let (port, key) = start_server(
        "62850:62899",
        &[env!("CARGO_BIN_EXE_posh"), "attach", "dev"],
        &[("POSH_DIR", dir_str.as_str())],
    );
    let status = drive_client_to_exit(&port, &key);
    assert_eq!(
        status.code(),
        Some(7),
        "session exit status lost through the composition: {status:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
