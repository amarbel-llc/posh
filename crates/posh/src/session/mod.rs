//! Local session persistence (zmx port): daemon-per-session over Unix
//! sockets, organized into groups under a socket directory.

pub mod client;
pub mod daemon;
pub mod ipc;
mod list_table;

use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
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
            return Err(Error::Msg(format!("invalid group name: {group}")));
        }
        let env = |k: &str| std::env::var(k).ok();
        let uid = util::uid();
        let base = resolve_socket_base(
            env("POSH_DIR").as_deref(),
            env("XDG_RUNTIME_DIR").as_deref(),
            env("TMPDIR").as_deref(),
            uid,
        );
        // A pre-existing base (notably the world-writable `/tmp/posh-<uid>`
        // fallback) must be a real, private, self-owned directory — a
        // recursive create silently trusts whatever is already there, which
        // an attacker on a shared host could have planted. github #7.
        // The base only needs to be a real, self-owned directory (no symlink
        // redirect); it may be group-readable like any `/tmp` intermediate.
        validate_session_dir(&base, uid, false)?;
        let socket_dir = base.join(group);
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(&socket_dir)
            .map_err(|e| Error::Msg(format!("cannot create {}: {e}", socket_dir.display())))?;
        // The leaf that actually holds the sockets must be private (0700) and
        // self-owned — reject an attacker-planted group dir. github #7.
        validate_session_dir(&socket_dir, uid, true)?;
        Ok(Config {
            socket_dir,
            group: group.to_string(),
        })
    }

    pub fn socket_path(&self, name: &str) -> Result<PathBuf> {
        let encoded = util::encode_session_name(name);
        let path = self.socket_dir.join(&encoded);
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(Error::Msg(format!(
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

/// Ensure the named session exists (spawning a detached daemon when it does
/// not) and return a fresh client connection to its socket. Factored from the
/// ensure-then-connect that `client::cmd_attach` did inline (github: RFC 0008
/// §3) so the local attach client and the frame relay (`remote::relay`) share
/// one connect path. `command` seeds a freshly created session's shell and is
/// ignored when the session already exists.
pub(crate) fn connect_or_create(
    cfg: &Config,
    name: &str,
    command: Option<Vec<String>>,
) -> Result<UnixStream> {
    daemon::ensure_session(cfg, name, command)?;
    let path = cfg.socket_path(name)?;
    UnixStream::connect(&path).map_err(|e| Error::Msg(format!("connect {}: {e}", path.display())))
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
    let stream = UnixStream::connect(path).map_err(|e| Error::Msg(format!("connect: {e}")))?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    ipc::send(std::os::fd::AsRawFd::as_raw_fd(&stream), Tag::Info, b"")?;
    let frame = wait_for_frame(&stream, Tag::Info, "info")?;
    let info =
        SessionInfo::decode(&frame.payload).ok_or_else(|| Error::from("bad info payload"))?;
    Ok(Probe { stream, info })
}

/// Reads frames off `stream` (honoring its read timeout) until one tagged
/// `tag` arrives, skipping others. `what` names the wait in errors.
fn wait_for_frame(mut stream: &UnixStream, tag: Tag, what: &str) -> Result<ipc::Frame> {
    let mut fb = ipc::FrameBuffer::new();
    loop {
        if let Some(frame) = fb.next()? {
            if frame.tag == tag {
                return Ok(frame);
            }
            continue;
        }
        let mut tmp = [0u8; 4096];
        let n = stream
            .read(&mut tmp)
            .map_err(|e| Error::Msg(format!("waiting for {what}: {e}")))?;
        if n == 0 {
            return Err(Error::Msg(format!("connection closed waiting for {what}")));
        }
        fb.feed(&tmp[..n]);
    }
}

/// Validates an existing session directory: it must be a real directory (not
/// a symlink) owned by `uid`. With `require_private`, it must additionally
/// have no group/other access (0700). A path that does not exist yet is fine
/// (the caller creates it). github #7. `pub(crate)` so the agent-forwarding
/// endpoint (`remote::agent`) hardens `<base>/agent/` with the same check.
pub(crate) fn validate_session_dir(path: &Path, uid: u32, require_private: bool) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        // Only NotFound is the benign "not created yet" case (#120): a
        // permission-denied or I/O error must surface rather than silently
        // passing a directory we could not actually validate.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Err(Error::Msg(format!(
            "refusing symlinked session dir: {}",
            path.display()
        )));
    }
    if !ft.is_dir() {
        return Err(Error::Msg(format!(
            "session dir is not a directory: {}",
            path.display()
        )));
    }
    if meta.uid() != uid {
        return Err(Error::Msg(format!(
            "session dir {} is not owned by uid {uid}",
            path.display()
        )));
    }
    if require_private && meta.mode() & 0o077 != 0 {
        return Err(Error::Msg(format!(
            "session dir {} is group/other-accessible (mode {:o})",
            path.display(),
            meta.mode() & 0o777
        )));
    }
    Ok(())
}

/// True only when the socket is genuinely dead — connect is refused or the
/// path is gone — as opposed to a live daemon that was merely slow to reply.
/// `pub(crate)` so `remote::agent` reuses the same liveness probe for symlink
/// takeover and dead-`srv-*.sock` GC (github #15 distinction).
pub(crate) fn socket_is_dead(path: &Path) -> bool {
    match UnixStream::connect(path) {
        Ok(_) => false,
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
        ),
    }
}

/// Removes a socket file, but only when the daemon behind it is genuinely
/// gone. A transient probe timeout against a live-but-busy daemon must not
/// orphan a running session. Returns whether the file was removed. github #15.
pub(crate) fn cleanup_stale_socket(path: &Path) -> bool {
    if !socket_is_dead(path) {
        util::log_write(
            "warn",
            &format!(
                "{} did not answer a probe but still accepts connections; not removing",
                path.display()
            ),
        );
        return false;
    }
    util::log_write(
        "warn",
        &format!("stale socket found, cleaning up {}", path.display()),
    );
    let _ = std::fs::remove_file(path);
    true
}

// ---------------------------------------------------------------------------
// list

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ListFormat {
    Default,
    Short,
    Json,
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
            Ok(probe) => {
                let cmd = probe.info.cmd_display();
                sessions.push(SessionEntry {
                    name,
                    pid: Some(probe.info.pid),
                    clients: Some(probe.info.clients),
                    error: None,
                    cmd: (!cmd.is_empty()).then_some(cmd),
                    cwd: (!probe.info.cwd.is_empty()).then_some(probe.info.cwd),
                })
            }
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

    // The interactive default is the styled table (the `sc list` look);
    // a piped default stays the legacy tab-separated lines, and --short/
    // --json are untouched — scripts and the completion probe parse those.
    let pretty = format == ListFormat::Default && util::is_tty(libc::STDOUT_FILENO);

    if sessions.is_empty() {
        match format {
            ListFormat::Default if pretty => print!("{}", list_table::render_empty(&cfg.socket_dir)),
            ListFormat::Default => println!("{}", list_table::empty_message(&cfg.socket_dir)),
            ListFormat::Json => println!("[]"),
            ListFormat::Short => {}
        }
        return Ok(());
    }

    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    if format == ListFormat::Json {
        println!("{}", json_list(&sessions, current.as_deref()));
        return Ok(());
    }
    if pretty {
        let (_, cols) = crate::pty::term_size(libc::STDOUT_FILENO);
        print!(
            "{}",
            list_table::render(
                &sessions,
                current.as_deref(),
                cols as usize,
                std::env::var("HOME").ok().as_deref(),
            )
        );
        return Ok(());
    }
    for s in &sessions {
        print_session_line(s, format, current.as_deref());
    }
    Ok(())
}

/// JSON list output, field-for-field compatible with zmx's `list --json`.
fn json_list(sessions: &[SessionEntry], current: Option<&str>) -> String {
    let mut out = String::from("[");
    for (i, s) in sessions.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        out.push_str(&json_string(&s.name));
        if let Some(err) = &s.error {
            out.push_str(",\"error\":true,\"status\":");
            out.push_str(&json_string(err));
        } else {
            let is_current = current == Some(s.name.as_str());
            out.push_str(&format!(
                ",\"pid\":{},\"clients\":{}",
                s.pid.unwrap_or(0),
                s.clients.unwrap_or(0)
            ));
            if let Some(cwd) = &s.cwd {
                out.push_str(",\"cwd\":");
                out.push_str(&json_string(cwd));
            }
            if let Some(cmd) = &s.cmd {
                out.push_str(",\"cmd\":");
                out.push_str(&json_string(cmd));
            }
            out.push_str(&format!(",\"current\":{is_current}"));
        }
        out.push('}');
    }
    out.push(']');
    out
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
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
        return Err(Error::Msg(format!(
            "cannot kill session because it does not exist session_name={name}"
        )));
    }
    match probe_session(&path) {
        Ok(probe) => {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&probe.stream);
            let _ = ipc::send(fd, Tag::Kill, b"");
            println!("killed session {name}");
        }
        Err(e) => {
            if cleanup_stale_socket(&path) {
                println!("cleaned up stale session {name}");
            } else {
                return Err(Error::Msg(format!("session {name} is unresponsive: {e}")));
            }
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
        return Err(Error::Msg(format!("session does not exist session_name={name}")));
    }
    match probe_session(&path) {
        Ok(probe) => {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&probe.stream);
            let _ = ipc::send(fd, Tag::DetachAll, b"");
            Ok(())
        }
        Err(e) => {
            cleanup_stale_socket(&path);
            Err(Error::Msg(format!("session unresponsive: {e}")))
        }
    }
}

/// Detaches all clients from every session in the group.
pub fn cmd_detach_all(cfg: &Config) -> Result<()> {
    for entry in std::fs::read_dir(&cfg.socket_dir)? {
        let Ok(entry) = entry else { continue };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_socket() {
            continue;
        }
        let path = entry.path();
        match probe_session(&path) {
            Ok(probe) => {
                let fd = std::os::fd::AsRawFd::as_raw_fd(&probe.stream);
                let _ = ipc::send(fd, Tag::DetachAll, b"");
            }
            Err(_) => {
                cleanup_stale_socket(&path);
            }
        }
    }
    Ok(())
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
        if util::is_tty(0) {
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
    let probe = probe_session(&path).map_err(|e| Error::Msg(format!("session not ready: {e}")))?;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&probe.stream);
    ipc::send(fd, Tag::Run, text.as_bytes())?;

    probe
        .stream
        .set_read_timeout(Some(Duration::from_secs(5)))?;
    wait_for_frame(&probe.stream, Tag::Ack, "ack")?;
    println!("command sent");
    Ok(())
}

// ---------------------------------------------------------------------------
// fork / groups / history (zmx parity)

/// `posh fork [name]`: clone the current session's command and working
/// directory into a new detached session. Without a name, the first free
/// "<current>-N" is used.
pub fn cmd_fork(cfg: &Config, target: Option<&str>) -> Result<()> {
    let source = std::env::var("POSH_SESSION").map_err(|_| {
        Error::from("POSH_SESSION env var not found: are you inside a posh session?")
    })?;

    // Probe the source session for its command and cwd.
    let source_path = cfg.socket_path(&source)?;
    let probe = match probe_session(&source_path) {
        Ok(p) => p,
        Err(e) => {
            cleanup_stale_socket(&source_path);
            return Err(Error::Msg(format!("source session unresponsive: {e}")));
        }
    };
    let info = probe.info;
    drop(probe.stream);

    let target_name = match target {
        Some(name) => name.to_string(),
        None => next_fork_name(cfg, &source)?,
    };
    let target_path = cfg.socket_path(&target_name)?;
    if session_socket_exists(&target_path) {
        return Err(Error::Msg(format!("session already exists: {target_name}")));
    }

    let args = info.cmd_argv();
    let command = (!args.is_empty()).then_some(args);

    // chdir so the new daemon inherits the source session's cwd.
    if !info.cwd.is_empty() {
        if let Err(e) = std::env::set_current_dir(&info.cwd) {
            util::log_write("warn", &format!("could not chdir to {}: {e}", info.cwd));
        }
    }

    let created = daemon::ensure_session(cfg, &target_name, command)?;
    if created {
        println!("forked session \"{source}\" into \"{target_name}\"");
    }
    Ok(())
}

fn next_fork_name(cfg: &Config, base: &str) -> Result<String> {
    for i in 1..1000u32 {
        let candidate = format!("{base}-{i}");
        let Ok(path) = cfg.socket_path(&candidate) else {
            continue;
        };
        if std::fs::symlink_metadata(&path).is_err() {
            return Ok(candidate);
        }
    }
    Err(Error::from("too many sessions"))
}

/// `posh groups`: list groups (socket-base subdirectories with at least one
/// socket in them), sorted.
pub fn cmd_groups() -> Result<()> {
    let env = |k: &str| std::env::var(k).ok();
    let uid = util::uid();
    let base = resolve_socket_base(
        env("POSH_DIR").as_deref(),
        env("XDG_RUNTIME_DIR").as_deref(),
        env("TMPDIR").as_deref(),
        uid,
    );
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Ok(()); // no base directory yet: no groups
    };
    let mut groups: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Ok(group_entries) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        let has_sessions = group_entries.flatten().any(|e| {
            e.file_type()
                .map(|t| t.is_socket() || t.is_file())
                .unwrap_or(false)
        });
        if has_sessions {
            groups.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    groups.sort();
    for group in groups {
        println!("{group}");
    }
    Ok(())
}

/// `posh history <name> [--vt]`: fetch the session's scrollback through the
/// History IPC message (payload byte 1 = vt escape stream, 0 = plain text).
pub fn cmd_history(cfg: &Config, name: &str, vt: bool) -> Result<()> {
    let path = cfg.socket_path(name)?;
    if !session_socket_exists(&path) {
        return Err(Error::Msg(format!("session does not exist session_name={name}")));
    }
    let probe = match probe_session(&path) {
        Ok(p) => p,
        Err(e) => {
            cleanup_stale_socket(&path);
            return Err(Error::Msg(format!("session unresponsive: {e}")));
        }
    };
    let fd = std::os::fd::AsRawFd::as_raw_fd(&probe.stream);
    ipc::send(fd, Tag::History, &ipc::encode_history_format(vt))?;

    probe
        .stream
        .set_read_timeout(Some(Duration::from_secs(5)))?;
    let frame = wait_for_frame(&probe.stream, Tag::History, "history response")?;
    use std::io::Write;
    std::io::stdout().write_all(&frame.payload)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_list_shape_matches_zmx() {
        let sessions = vec![
            SessionEntry {
                name: "alpha".to_string(),
                pid: Some(1234),
                clients: Some(2),
                error: None,
                cmd: Some("htop -d 10".to_string()),
                cwd: Some("/home/user".to_string()),
            },
            SessionEntry {
                name: "broken".to_string(),
                pid: None,
                clients: None,
                error: Some("ConnectionRefused".to_string()),
                cmd: None,
                cwd: None,
            },
            SessionEntry {
                name: "minimal".to_string(),
                pid: Some(9),
                clients: Some(0),
                error: None,
                cmd: None,
                cwd: None,
            },
        ];
        let json = json_list(&sessions, Some("minimal"));
        assert_eq!(
            json,
            concat!(
                "[",
                "{\"name\":\"alpha\",\"pid\":1234,\"clients\":2,",
                "\"cwd\":\"/home/user\",\"cmd\":\"htop -d 10\",\"current\":false},",
                "{\"name\":\"broken\",\"error\":true,\"status\":\"ConnectionRefused\"},",
                "{\"name\":\"minimal\",\"pid\":9,\"clients\":0,\"current\":true}",
                "]"
            )
        );
    }

    #[test]
    fn json_string_escaping() {
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(json_string("with \"quotes\""), "\"with \\\"quotes\\\"\"");
        assert_eq!(json_string("back\\slash"), "\"back\\\\slash\"");
        assert_eq!(json_string("tab\there"), "\"tab\\there\"");
        assert_eq!(json_string("nl\n"), "\"nl\\n\"");
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
        assert_eq!(json_string("uni→ok"), "\"uni→ok\"");
    }

    #[test]
    fn json_list_empty_current() {
        let sessions = vec![SessionEntry {
            name: "x".to_string(),
            pid: Some(1),
            clients: Some(0),
            error: None,
            cmd: None,
            cwd: None,
        }];
        let json = json_list(&sessions, None);
        assert!(json.contains("\"current\":false"));
    }

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

    #[test]
    fn validate_session_dir_distinguishes_absent_from_unreadable() {
        // #120: only NotFound is the benign "not created yet" case; a stat
        // that fails for another reason (here: an unsearchable parent →
        // PermissionDenied) must surface instead of silently validating.
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("posh-vsd-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let uid = util::uid();

        assert!(
            validate_session_dir(&base.join("missing"), uid, true).is_ok(),
            "absent dir is the caller-creates-it case"
        );

        let locked = base.join("locked");
        let child = locked.join("dir");
        std::fs::create_dir_all(&child).unwrap();
        let mut perms = std::fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&locked, perms.clone()).unwrap();
        let result = validate_session_dir(&child, uid, true);
        // (Root can stat through 0o000, so only assert when the stat failed.)
        if let Err(err) = result {
            assert_eq!(
                err.kind(),
                Some(std::io::ErrorKind::PermissionDenied),
                "unreadable parent must surface as PermissionDenied"
            );
        }
        perms.set_mode(0o700);
        std::fs::set_permissions(&locked, perms).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }
}
