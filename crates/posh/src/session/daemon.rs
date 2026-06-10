//! Per-session daemon: owns the PTY and broadcasts output to attached
//! clients over a Unix socket (zmx daemonLoop port).

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

use posh_term::Terminal;

use crate::pty::{self, PtyChild};
use crate::session::ipc::{self, FrameBuffer, SessionInfo, Tag};
use crate::session::{self, Config};
use crate::util::{self, Error, Result};

const SCROLLBACK: usize = 10_000;

/// Ensures the session exists, forking off a daemon when needed. Returns
/// true when a new session was created. The daemon is a double-forked
/// grandchild that never returns from this function (it exits the process).
pub fn ensure_session(cfg: &Config, name: &str, command: Option<Vec<String>>) -> Result<bool> {
    let path = cfg.socket_path(name)?;
    if session::session_socket_exists(&path) {
        match session::probe_session(&path) {
            Ok(_) => {
                if command.is_some() {
                    util::log_write(
                        "warn",
                        &format!("session already exists, ignoring command session={name}"),
                    );
                }
                return Ok(false);
            }
            Err(_) => {
                let _ = std::fs::remove_file(&path);
            }
        }
    } else if std::fs::symlink_metadata(&path).is_ok() {
        return Err(Error(format!(
            "{} exists and is not a socket",
            path.display()
        )));
    }

    // Bind before forking so a racing client can connect (and queue) as soon
    // as the parent returns.
    let listener =
        UnixListener::bind(&path).map_err(|e| Error(format!("bind {}: {e}", path.display())))?;
    if util::double_fork()? {
        drop(listener);
        std::thread::sleep(std::time::Duration::from_millis(10));
        return Ok(true);
    }
    daemon_main(cfg, name, listener, command);
}

struct ClientConn {
    stream: UnixStream,
    read_buf: FrameBuffer,
    write_buf: Vec<u8>,
    // Zero means "size not yet reported"; ignored for the shared minimum.
    rows: u16,
    cols: u16,
}

impl ClientConn {
    fn queue(&mut self, tag: Tag, payload: &[u8]) {
        ipc::append_frame(&mut self.write_buf, tag, payload);
    }
}

/// Elementwise minimum size across all clients that have reported one
/// (tmux `window-size smallest`).
fn min_client_size(clients: &[ClientConn]) -> Option<(u16, u16)> {
    let mut acc: Option<(u16, u16)> = None;
    for c in clients {
        if c.rows == 0 || c.cols == 0 {
            continue;
        }
        acc = Some(match acc {
            None => (c.rows, c.cols),
            Some((r, w)) => (r.min(c.rows), w.min(c.cols)),
        });
    }
    acc
}

fn apply_client_size(clients: &[ClientConn], pty_fd: RawFd, term: &mut Terminal) {
    if let Some((rows, cols)) = min_client_size(clients) {
        pty::set_term_size(pty_fd, rows, cols);
        term.resize(rows, cols);
    }
}

fn daemon_main(
    cfg: &Config,
    name: &str,
    listener: UnixListener,
    command: Option<Vec<String>>,
) -> ! {
    util::redirect_stdio_devnull();
    let _ = util::log_init(&cfg.log_path(name));
    util::install_sigterm_handler();
    let socket_path = cfg.socket_path(name).expect("socket path");
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    // stdio is detached, so the PTY starts at the 24x80 default; the first
    // client Init resizes it.
    let (rows, cols) = (24u16, 80u16);
    let envs = vec![
        ("POSH_SESSION".to_string(), name.to_string()),
        ("POSH_GROUP".to_string(), cfg.group.clone()),
    ];
    let child = match pty::spawn_shell(command.as_deref(), rows, cols, &envs) {
        Ok(c) => c,
        Err(e) => {
            util::log_write("error", &format!("failed to spawn pty: {e}"));
            let _ = std::fs::remove_file(&socket_path);
            std::process::exit(1);
        }
    };
    util::log_write(
        "info",
        &format!("daemon started session={name} pid={}", child.pid),
    );

    let _ = listener.set_nonblocking(true);
    let _ = util::set_nonblocking(child.master);

    let mut term = Terminal::with_scrollback(rows, cols, SCROLLBACK);
    let mut clients: Vec<ClientConn> = Vec::new();
    let info_cmd = command.as_ref().map(|c| c.join(" ")).unwrap_or_default();

    daemon_loop(&listener, &child, &mut term, &mut clients, &info_cmd, &cwd);

    // Teardown: drop client connections (their EOF is the detach notice),
    // then bring the shell down: SIGHUP first (shells ignore SIGTERM), then
    // SIGKILL, both to the whole process group.
    util::log_write("info", &format!("shutting down daemon session={name}"));
    clients.clear();
    unsafe {
        libc::kill(-child.pid, libc::SIGHUP);
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    unsafe {
        libc::kill(-child.pid, libc::SIGKILL);
        let mut status = 0;
        libc::waitpid(child.pid, &mut status, 0);
        libc::close(child.master);
    }
    let _ = std::fs::remove_file(&socket_path);
    std::process::exit(0);
}

fn daemon_loop(
    listener: &UnixListener,
    child: &PtyChild,
    term: &mut Terminal,
    clients: &mut Vec<ClientConn>,
    info_cmd: &str,
    cwd: &str,
) {
    let listener_fd = listener.as_raw_fd();
    let pty_fd = child.master;
    let mut has_pty_output = false;
    let mut has_had_client = false;
    let err_events = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;

    'daemon: loop {
        if util::take_flag(&util::SIGTERM_RECEIVED) {
            util::log_write("info", "SIGTERM received, shutting down gracefully");
            break;
        }

        let mut fds = Vec::with_capacity(2 + clients.len());
        fds.push(util::pollfd(listener_fd, libc::POLLIN));
        fds.push(util::pollfd(pty_fd, libc::POLLIN));
        for c in clients.iter() {
            let mut events = libc::POLLIN;
            if !c.write_buf.is_empty() {
                events |= libc::POLLOUT;
            }
            fds.push(util::pollfd(c.stream.as_raw_fd(), events));
        }

        match util::poll(&mut fds, -1) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                util::log_write("error", &format!("poll failed: {e}"));
                break;
            }
        }

        // New client connections.
        if fds[0].revents & err_events != 0 {
            util::log_write("error", "server socket error");
            break;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            if let Ok((stream, _)) = listener.accept() {
                let _ = stream.set_nonblocking(true);
                util::log_write(
                    "info",
                    &format!("client connected fd={}", stream.as_raw_fd()),
                );
                clients.push(ClientConn {
                    stream,
                    read_buf: FrameBuffer::new(),
                    write_buf: Vec::new(),
                    rows: 0,
                    cols: 0,
                });
            }
        }

        // PTY output: feed the terminal model, return any query replies to
        // the application, and broadcast raw bytes to all clients.
        if fds[1].revents & (libc::POLLIN | err_events) != 0 {
            let mut buf = [0u8; 4096];
            match util::read_fd(pty_fd, &mut buf) {
                Ok(0) => {
                    util::log_write("info", "shell exited");
                    break;
                }
                Ok(n) => {
                    term.process(&buf[..n]);
                    let responses = term.take_responses();
                    if !responses.is_empty() {
                        let _ = util::write_all_retry(pty_fd, &responses, 100);
                    }
                    has_pty_output = true;
                    for c in clients.iter_mut() {
                        c.queue(Tag::Output, &buf[..n]);
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    // EIO on Linux when the slave side is gone.
                    util::log_write("info", "pty closed");
                    break;
                }
            }
        }

        // Client traffic. Iterate only over the clients present when the
        // pollfd set was built; walk backwards so removal is safe.
        let polled = fds.len() - 2;
        let mut i = clients.len().min(polled);
        while i > 0 {
            i -= 1;
            let revents = fds[i + 2].revents;
            if revents == 0 {
                continue;
            }
            let mut remove = false;
            let mut resized = false;
            let mut detach_all = false;
            let total_clients = clients.len();
            {
                let c = &mut clients[i];
                if revents & libc::POLLIN != 0 {
                    match c.read_buf.read_from(c.stream.as_raw_fd()) {
                        Ok(0) => remove = true,
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => remove = true,
                    }
                    if !remove {
                        while let Some(frame) = c.read_buf.next() {
                            match frame.tag {
                                Tag::Input => {
                                    let _ = util::write_all_retry(pty_fd, &frame.payload, 100);
                                }
                                Tag::Init => {
                                    if let Some((r, w)) = ipc::decode_resize(&frame.payload) {
                                        c.rows = r;
                                        c.cols = w;
                                        resized = true;
                                    }
                                    // Replay terminal state only on re-attach:
                                    // on the very first attach the live stream
                                    // (shell init, DA queries) must win.
                                    if has_pty_output && has_had_client {
                                        let dump = term.dump_vt();
                                        c.queue(Tag::Output, &dump);
                                    }
                                    has_had_client = true;
                                }
                                Tag::Resize => {
                                    if let Some((r, w)) = ipc::decode_resize(&frame.payload) {
                                        c.rows = r;
                                        c.cols = w;
                                        resized = true;
                                    }
                                }
                                Tag::Detach => {
                                    remove = true;
                                    break;
                                }
                                Tag::DetachAll => {
                                    detach_all = true;
                                    break;
                                }
                                Tag::Kill => break 'daemon,
                                Tag::Info => {
                                    let info = SessionInfo {
                                        clients: (total_clients - 1) as u64,
                                        pid: child.pid,
                                        cmd: info_cmd.to_string(),
                                        cwd: cwd.to_string(),
                                    };
                                    c.queue(Tag::Info, &info.encode());
                                }
                                Tag::History => {
                                    let out = if ipc::decode_history_format(&frame.payload) {
                                        term.dump_vt()
                                    } else {
                                        term.dump_text().into_bytes()
                                    };
                                    c.queue(Tag::History, &out);
                                }
                                Tag::Run => {
                                    let _ = util::write_all_retry(pty_fd, &frame.payload, 1000);
                                    c.queue(Tag::Ack, b"");
                                    has_had_client = true;
                                }
                                Tag::Output | Tag::Ack => {}
                            }
                        }
                    }
                }
                if !remove && revents & libc::POLLOUT != 0 && !c.write_buf.is_empty() {
                    match c.stream.write(&c.write_buf) {
                        Ok(n) => {
                            c.write_buf.drain(..n);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => remove = true,
                    }
                }
                if revents & err_events != 0 {
                    remove = true;
                }
            }
            if detach_all {
                util::log_write("info", &format!("detach all clients={}", clients.len()));
                clients.clear();
                break;
            }
            if remove {
                let fd = clients[i].stream.as_raw_fd();
                clients.remove(i);
                util::log_write(
                    "info",
                    &format!("client disconnected fd={fd} remaining={}", clients.len()),
                );
                // The smallest client may have left; grow back (zmx issue #8).
                resized = true;
            }
            if resized {
                apply_client_size(clients, pty_fd, term);
            }
        }
    }
}
