//! Local session persistence (zmx port): daemon-per-session over Unix
//! sockets, organized into groups under a socket directory.

pub mod client;
pub mod daemon;
pub mod ipc;

use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util::{self, Error, Result};
use ipc::{SessionInfo, Tag};

/// sockaddr_un sun_path is 108 bytes on Linux including the NUL.
const MAX_SOCKET_PATH: usize = 107;

/// Socket directory base resolution: POSH_DIR > XDG_RUNTIME_DIR/posh >
/// TMPDIR/posh-{uid} > /tmp/posh-{uid}.
pub fn resolve_socket_base(
    posh_dir: Option<&str>,
    xdg_runtime_dir: Option<&str>,
    tmpdir: Option<&str>,
    uid: u32,
) -> PathBuf {
    if let Some(dir) = posh_dir.filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = xdg_runtime_dir.filter(|d| !d.is_empty()) {
        return Path::new(dir).join("posh");
    }
    if let Some(dir) = tmpdir.filter(|d| !d.is_empty()) {
        return Path::new(dir).join(format!("posh-{uid}"));
    }
    PathBuf::from(format!("/tmp/posh-{uid}"))
}

pub struct Config {
    pub socket_dir: PathBuf,
    pub group: String,
}

impl Config {
    pub fn new(group: &str) -> Result<Config> {
        if group.is_empty() {
            return Err(Error::from("group name cannot be empty"));
        }
        if group.contains('/') || group.contains("..") {
            return Err(Error(format!("invalid group name: {group}")));
        }
        let env = |k: &str| std::env::var(k).ok();
        let uid = unsafe { libc::getuid() };
        let base = resolve_socket_base(
            env("POSH_DIR").as_deref(),
            env("XDG_RUNTIME_DIR").as_deref(),
            env("TMPDIR").as_deref(),
            uid,
        );
        let socket_dir = base.join(group);
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(&socket_dir)
            .map_err(|e| Error(format!("cannot create {}: {e}", socket_dir.display())))?;
        Ok(Config {
            socket_dir,
            group: group.to_string(),
        })
    }

    pub fn socket_path(&self, name: &str) -> Result<PathBuf> {
        let encoded = util::encode_session_name(name);
        let path = self.socket_dir.join(&encoded);
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(Error(format!(
                "socket path too long ({} bytes, max {MAX_SOCKET_PATH}): {}",
                path.as_os_str().len(),
                path.display()
            )));
        }
        Ok(path)
    }

    pub fn log_path(&self, name: &str) -> PathBuf {
        self.socket_dir
            .join(format!("{}.log", util::encode_session_name(name)))
    }
}

pub fn session_socket_exists(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.file_type().is_socket(),
        Err(_) => false,
    }
}

pub struct Probe {
    pub stream: UnixStream,
    pub info: SessionInfo,
}

/// Connects to a session socket and asks for Info; one-second timeout. Used
/// both for liveness checks and to enumerate session metadata.
pub fn probe_session(path: &Path) -> Result<Probe> {
    let stream = UnixStream::connect(path).map_err(|e| Error(format!("connect: {e}")))?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    ipc::send(std::os::fd::AsRawFd::as_raw_fd(&stream), Tag::Info, b"")?;
    let mut fb = ipc::FrameBuffer::new();
    let mut stream_ref = &stream;
    loop {
        if let Some(frame) = fb.next() {
            if frame.tag == Tag::Info {
                let info = SessionInfo::decode(&frame.payload)
                    .ok_or_else(|| Error::from("bad info payload"))?;
                return Ok(Probe { stream, info });
            }
            continue;
        }
        let mut tmp = [0u8; 4096];
        let n = stream_ref
            .read(&mut tmp)
            .map_err(|e| Error(format!("probe read: {e}")))?;
        if n == 0 {
            return Err(Error::from("connection closed during probe"));
        }
        fb.feed(&tmp[..n]);
    }
}

fn cleanup_stale_socket(path: &Path) {
    util::log_write(
        "warn",
        &format!("stale socket found, cleaning up {}", path.display()),
    );
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// list

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ListFormat {
    Default,
    Short,
}

struct SessionEntry {
    name: String,
    pid: Option<i32>,
    clients: Option<u64>,
    error: Option<String>,
    cmd: Option<String>,
    cwd: Option<String>,
}

pub fn cmd_list(cfg: &Config, format: ListFormat) -> Result<()> {
    let current = std::env::var("POSH_SESSION").ok();
    let mut sessions: Vec<SessionEntry> = Vec::new();

    for entry in std::fs::read_dir(&cfg.socket_dir)? {
        let entry = entry?;
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !file_type.is_socket() {
            continue;
        }
        let encoded = entry.file_name().to_string_lossy().into_owned();
        let name = util::decode_session_name(&encoded);
        let path = entry.path();
        match probe_session(&path) {
            Ok(probe) => sessions.push(SessionEntry {
                name,
                pid: Some(probe.info.pid),
                clients: Some(probe.info.clients),
                error: None,
                cmd: (!probe.info.cmd.is_empty()).then_some(probe.info.cmd),
                cwd: (!probe.info.cwd.is_empty()).then_some(probe.info.cwd),
            }),
            Err(e) => {
                sessions.push(SessionEntry {
                    name,
                    pid: None,
                    clients: None,
                    error: Some(e.to_string()),
                    cmd: None,
                    cwd: None,
                });
                cleanup_stale_socket(&path);
            }
        }
    }

    if sessions.is_empty() {
        if format == ListFormat::Default {
            println!("no sessions found in {}", cfg.socket_dir.display());
        }
        return Ok(());
    }

    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    for s in &sessions {
        print_session_line(s, format, current.as_deref());
    }
    Ok(())
}

fn print_session_line(s: &SessionEntry, format: ListFormat, current: Option<&str>) {
    let prefix = match current {
        Some(cur) if cur == s.name => "\u{2192} ",
        Some(_) => "  ",
        None => "",
    };
    if format == ListFormat::Short {
        if s.error.is_none() {
            println!("{}", s.name);
        }
        return;
    }
    if let Some(err) = &s.error {
        println!(
            "{prefix}session_name={}\tstatus={err}\t(cleaning up)",
            s.name
        );
        return;
    }
    let mut line = format!(
        "{prefix}session_name={}\tpid={}\tclients={}",
        s.name,
        s.pid.unwrap_or(0),
        s.clients.unwrap_or(0)
    );
    if let Some(cwd) = &s.cwd {
        line.push_str(&format!("\tstarted_in={cwd}"));
    }
    if let Some(cmd) = &s.cmd {
        line.push_str(&format!("\tcmd={cmd}"));
    }
    println!("{line}");
}

// ---------------------------------------------------------------------------
// kill / detach / run

pub fn cmd_kill(cfg: &Config, name: &str) -> Result<()> {
    let path = cfg.socket_path(name)?;
    if !session_socket_exists(&path) {
        return Err(Error(format!(
            "cannot kill session because it does not exist session_name={name}"
        )));
    }
    match probe_session(&path) {
        Ok(probe) => {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&probe.stream);
            let _ = ipc::send(fd, Tag::Kill, b"");
            println!("killed session {name}");
        }
        Err(_) => {
            cleanup_stale_socket(&path);
            println!("cleaned up stale session {name}");
        }
    }
    Ok(())
}

/// Detaches all clients from a named session (or, with None, the session
/// identified by $POSH_SESSION).
pub fn cmd_detach(cfg: &Config, name: Option<&str>) -> Result<()> {
    let owned;
    let name = match name {
        Some(n) => n,
        None => {
            owned = std::env::var("POSH_SESSION").map_err(|_| {
                Error::from("POSH_SESSION env var not found: are you inside a posh session?")
            })?;
            &owned
        }
    };
    let path = cfg.socket_path(name)?;
    if !session_socket_exists(&path) {
        return Err(Error(format!("session does not exist session_name={name}")));
    }
    match probe_session(&path) {
        Ok(probe) => {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&probe.stream);
            let _ = ipc::send(fd, Tag::DetachAll, b"");
            Ok(())
        }
        Err(e) => {
            cleanup_stale_socket(&path);
            Err(Error(format!("session unresponsive: {e}")))
        }
    }
}

/// Runs a command inside a session (creating it if needed) without attaching:
/// the command text is written to the session PTY as if typed.
pub fn cmd_run(cfg: &Config, name: &str, args: &[String]) -> Result<()> {
    let created = daemon::ensure_session(cfg, name, None)?;
    if created {
        println!("session \"{name}\" created");
    }

    let mut text = if args.is_empty() {
        // No argv: accept the command on stdin when it is not a tty.
        if unsafe { libc::isatty(0) } == 1 {
            String::new()
        } else {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
    } else {
        args.join(" ")
    };
    if text.is_empty() {
        return Err(Error::from("command required"));
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }

    let path = cfg.socket_path(name)?;
    let probe = probe_session(&path).map_err(|e| Error(format!("session not ready: {e}")))?;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&probe.stream);
    ipc::send(fd, Tag::Run, text.as_bytes())?;

    probe
        .stream
        .set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut fb = ipc::FrameBuffer::new();
    let mut stream_ref = &probe.stream;
    loop {
        if let Some(frame) = fb.next() {
            if frame.tag == Tag::Ack {
                println!("command sent");
                return Ok(());
            }
            continue;
        }
        let mut tmp = [0u8; 4096];
        let n = stream_ref
            .read(&mut tmp)
            .map_err(|_| Error::from("timeout waiting for ack"))?;
        if n == 0 {
            return Err(Error::from("connection closed before ack"));
        }
        fb.feed(&tmp[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_base_prefers_posh_dir() {
        let base = resolve_socket_base(Some("/var/posh"), Some("/run/user/7"), Some("/var/tmp"), 7);
        assert_eq!(base, PathBuf::from("/var/posh"));
    }

    #[test]
    fn socket_base_falls_back_to_xdg_runtime() {
        let base = resolve_socket_base(None, Some("/run/user/7"), Some("/var/tmp"), 7);
        assert_eq!(base, PathBuf::from("/run/user/7/posh"));
    }

    #[test]
    fn socket_base_falls_back_to_tmpdir_with_uid() {
        let base = resolve_socket_base(None, None, Some("/var/tmp"), 1000);
        assert_eq!(base, PathBuf::from("/var/tmp/posh-1000"));
    }

    #[test]
    fn socket_base_final_fallback_is_tmp_uid() {
        let base = resolve_socket_base(None, None, None, 1000);
        assert_eq!(base, PathBuf::from("/tmp/posh-1000"));
    }

    #[test]
    fn socket_base_ignores_empty_values() {
        let base = resolve_socket_base(Some(""), Some(""), Some(""), 42);
        assert_eq!(base, PathBuf::from("/tmp/posh-42"));
    }
}
