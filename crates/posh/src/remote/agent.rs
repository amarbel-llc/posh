//! Remote endpoint for SSH agent forwarding (FDR 0004 work item 3).
//!
//! The server side of agent forwarding: a unix-domain socket on the remote
//! host that `git push` / `ssh` / `scp` inside a posh session connect to as
//! their `SSH_AUTH_SOCK`. Each accepted connection becomes a channel whose
//! opaque bytes are proxied — via the reliable agent byte stream
//! ([`crate::remote::sync::AgentStream`]) over the roaming UDP transport — to
//! the posh *client*, which relays them to the user's real local agent. No
//! agent-message parsing happens here; channels are protocol-agnostic byte
//! pipes (the agent and its clients do the parsing).
//!
//! "Forwarded once" (design §4): every agent-capable server binds its own
//! `agent/srv-<pid>.sock` and atomically repoints the well-known
//! `agent/sock` symlink at itself — newest forwarding-active connection wins,
//! the proven tmux pattern, no lock and no election protocol. `SSH_AUTH_SOCK`
//! is always the stable `agent/sock`, valid across detach/reattach.
//!
//! Everything here is `poll`/unix-socket/`rename` (ADR 0001): no async
//! runtime, no new dependency. The `server_loop` splices this endpoint's fds
//! into its existing poll set; this module owns no event loop of its own.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::remote::sync::{AgentRecord, RecordKind};
use crate::util::{self, Result};

// Tuning levers (FDR 0004). See the feature record's "Tuning Levers" table for
// the change signals on each.
/// Max concurrent agent channels per connection: bounds clients and memory.
const MAX_AGENT_CHANNELS: usize = 8;
/// Per-channel buffered-bytes cap (OpenSSH's max agent message). A read that
/// would exceed it is split across `Data` records, never grown unboundedly.
const CHANNEL_READ_CHUNK: usize = 256 * 1024;
/// Cadence for the symlink-liveness / takeover check and dead-`srv-*.sock` GC.
const AGENT_SLOW_TICK_MS: u64 = 5_000;
/// Peer-silence window after which the endpoint fast-fails outstanding agent
/// requests (stricter than the loop's 60 s `PEER_TIMEOUT`): a `git push` gets
/// `SSH_AGENT_FAILURE` rather than hanging when the peer has roamed away. The
/// `server_loop` computes the gate against this and passes it to [`tick`].
pub(crate) const AGENT_PEER_ACTIVE: u64 = 15_000; // ms

/// One forwarded agent connection accepted on `agent/srv-<pid>.sock`. The
/// `u32` id matches it to a record-stream channel; the `stream` is the live
/// unix socket to the agent client (`git`, `ssh`, …).
struct Channel {
    id: u32,
    stream: UnixStream,
    /// Set once the peer (or a local error) has closed the channel; the
    /// server stops polling a closed channel and reaps it next sweep.
    closed: bool,
}

/// The remote agent-forwarding endpoint: the per-pid listener, the live
/// channels, and ownership of the stable `agent/sock` symlink.
pub struct AgentEndpoint {
    /// `<base>/agent/` — created 0700, validated self-owned + no-symlink.
    dir: PathBuf,
    /// `<base>/agent/srv-<pid>.sock` — this server's own socket.
    own_sock: PathBuf,
    /// `<base>/agent/sock` — the stable, symlinked `SSH_AUTH_SOCK` target.
    well_known: PathBuf,
    listener: UnixListener,
    channels: Vec<Channel>,
    next_channel_id: u32,
    last_tick: u64,
}

impl AgentEndpoint {
    /// Builds the endpoint under the resolved session-dir base (production
    /// path): the same `POSH_DIR > XDG_RUNTIME_DIR/posh > TMPDIR/posh-{uid} >
    /// /tmp/posh-{uid}` precedence as session sockets.
    pub fn from_env() -> Result<AgentEndpoint> {
        let env = |k: &str| std::env::var(k).ok();
        let uid = util::uid();
        let base = crate::session::resolve_socket_base(
            env("POSH_DIR").as_deref(),
            env("XDG_RUNTIME_DIR").as_deref(),
            env("TMPDIR").as_deref(),
            uid,
        );
        AgentEndpoint::new(&base)
    }

    /// Builds the endpoint under an explicit base dir (the seam the tests use
    /// with a tempdir). Creates `<base>/agent/` 0700, hardens it with the
    /// shared #7 check, binds `srv-<pid>.sock`, and claims `agent/sock`.
    pub fn new(base: &Path) -> Result<AgentEndpoint> {
        use std::os::unix::fs::DirBuilderExt;

        let uid = util::uid();
        // The base itself must be a real, self-owned dir (no symlink redirect);
        // it may be group-readable like any /tmp intermediate. github #7.
        crate::session::validate_session_dir(base, uid, false)?;
        let dir = base.join("agent");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        // The leaf that holds the agent sockets must be private + self-owned —
        // reject an attacker-planted dir or a symlink. github #7.
        crate::session::validate_session_dir(&dir, uid, true)?;

        let pid = own_pid();
        let own_sock = dir.join(format!("srv-{pid}.sock"));
        // A stale socket for our own pid (pid reuse after an unclean exit)
        // would make bind fail with EADDRINUSE; clear it first.
        let _ = std::fs::remove_file(&own_sock);
        let listener = UnixListener::bind(&own_sock)?;
        listener.set_nonblocking(true)?;

        let endpoint = AgentEndpoint {
            dir: dir.clone(),
            own_sock,
            well_known: dir.join("sock"),
            listener,
            channels: Vec::new(),
            next_channel_id: 1,
            last_tick: 0,
        };
        endpoint.claim_symlink()?;
        Ok(endpoint)
    }

    /// The stable `SSH_AUTH_SOCK` path to export into the session shell (C5).
    pub fn sock_path(&self) -> &Path {
        &self.well_known
    }

    /// Atomically points `agent/sock` at our own `srv-<pid>.sock`: create a
    /// uniquely-named temp symlink in the (validated, private) dir and
    /// `rename` it over the well-known name. rename(2) is atomic, so a
    /// concurrent reader never sees a missing or half-written link.
    fn claim_symlink(&self) -> Result<()> {
        let target = format!("srv-{}.sock", own_pid());
        let tmp = self.dir.join(format!(".sock.{}.tmp", own_pid()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&target, &tmp)?;
        std::fs::rename(&tmp, &self.well_known)?;
        Ok(())
    }

    /// True when `agent/sock` is absent, dangling, or points at a dead
    /// `srv-*.sock` — i.e. nobody live owns the endpoint and we should claim
    /// it. A live link pointing at *another* live server is left alone.
    fn symlink_needs_takeover(&self) -> bool {
        match std::fs::read_link(&self.well_known) {
            Err(_) => true, // absent or not a symlink
            Ok(target) => {
                // Targets are stored relative to `dir` (e.g. "srv-123.sock").
                let resolved = self.dir.join(&target);
                crate::session::socket_is_dead(&resolved)
            }
        }
    }

    /// fds to splice into `server_loop`'s poll set: the listener plus every
    /// open channel. The caller records the returned order to map `revents`
    /// back (the listener is always first).
    pub fn pollfds(&self) -> Vec<libc::pollfd> {
        let mut fds = vec![util::pollfd(self.listener.as_raw_fd(), libc::POLLIN)];
        for ch in &self.channels {
            if !ch.closed {
                fds.push(util::pollfd(ch.stream.as_raw_fd(), libc::POLLIN));
            }
        }
        fds
    }

    /// Accepts every pending connection on the listener (non-blocking).
    /// Returns an `Open` record per new channel. Connections past
    /// `MAX_AGENT_CHANNELS` are accepted and immediately closed so the client
    /// is not left hanging — its `connect` succeeds but the channel never
    /// opens, which the agent protocol treats as a failed request.
    pub fn accept_pending(&mut self) -> Vec<AgentRecord> {
        let mut out = Vec::new();
        loop {
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    if self.live_channel_count() >= MAX_AGENT_CHANNELS {
                        drop(stream); // at capacity: refuse by closing
                        continue;
                    }
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let id = self.next_channel_id;
                    self.next_channel_id += 1;
                    self.channels.push(Channel {
                        id,
                        stream,
                        closed: false,
                    });
                    out.push(AgentRecord {
                        channel: id,
                        kind: RecordKind::Open,
                        payload: Vec::new(),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        out
    }

    /// Reads from every readable channel, producing `Data` records for fresh
    /// bytes and a `Close` when a channel reaches EOF or errors. The caller
    /// feeds the returned records into the outbound `AgentStream`.
    pub fn read_channels(&mut self) -> Vec<AgentRecord> {
        let mut out = Vec::new();
        for ch in &mut self.channels {
            if ch.closed {
                continue;
            }
            let mut buf = [0u8; CHANNEL_READ_CHUNK];
            loop {
                match ch.stream.read(&mut buf) {
                    Ok(0) => {
                        ch.closed = true;
                        out.push(close_record(ch.id));
                        break;
                    }
                    Ok(n) => out.push(AgentRecord {
                        channel: ch.id,
                        kind: RecordKind::Data,
                        payload: buf[..n].to_vec(),
                    }),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        ch.closed = true;
                        out.push(close_record(ch.id));
                        break;
                    }
                }
            }
        }
        self.reap_closed();
        out
    }

    /// Dispatches records decoded from the client's agent stream to their
    /// channel sockets: `Data` writes through; `Close`/`Fail` tear the channel
    /// down (the agent client's read then sees EOF, i.e. a failed request).
    /// An unknown channel id is ignored — a `Close` may race ahead of our own.
    pub fn apply_records(&mut self, records: &[AgentRecord]) {
        for rec in records {
            let Some(ch) = self.channels.iter_mut().find(|c| c.id == rec.channel) else {
                continue;
            };
            match rec.kind {
                RecordKind::Data => {
                    // A short/failed write fails just this channel, not the
                    // session: the agent protocol is strict request/response,
                    // so a half-written request is a failed request.
                    if ch.stream.write_all(&rec.payload).is_err() {
                        ch.closed = true;
                    }
                }
                RecordKind::Open => {
                    // OPEN only flows remote->client; receiving one back is a
                    // peer bug. Ignore rather than trust it.
                }
                RecordKind::Close | RecordKind::Fail => ch.closed = true,
            }
        }
        self.reap_closed();
    }

    /// Periodic maintenance, gated to `AGENT_SLOW_TICK_MS`. Returns any
    /// `Close` records produced (e.g. by the peer-inactive fast-fail) for the
    /// caller to forward. `peer_active` is the loop's existing liveness gate.
    pub fn tick(&mut self, peer_active: bool, now: u64) -> Vec<AgentRecord> {
        if now.saturating_sub(self.last_tick) < AGENT_SLOW_TICK_MS {
            return Vec::new();
        }
        self.last_tick = now;

        // Reclaim the endpoint if its owner died or the link went stale.
        if self.symlink_needs_takeover() {
            let _ = self.claim_symlink();
        }
        self.gc_dead_sockets();

        // Peer gone: fast-fail outstanding channels rather than hang a
        // `git push` waiting on bytes that cannot arrive. The agent client
        // sees its socket close and reports a failed request.
        let mut out = Vec::new();
        if !peer_active {
            for ch in &mut self.channels {
                if !ch.closed {
                    ch.closed = true;
                    out.push(close_record(ch.id));
                }
            }
            self.reap_closed();
        }
        out
    }

    fn live_channel_count(&self) -> usize {
        self.channels.iter().filter(|c| !c.closed).count()
    }

    fn reap_closed(&mut self) {
        self.channels.retain(|c| !c.closed);
    }

    /// Unlinks `srv-*.sock` files in `agent/` whose owning pid is dead. A
    /// server unlinks its own socket on exit, so these are crash leftovers.
    fn gc_dead_sockets(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == self.own_sock {
                continue;
            }
            let Some(pid) = srv_sock_pid(&path) else {
                continue;
            };
            if !pid_alive(pid) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

impl Drop for AgentEndpoint {
    fn drop(&mut self) {
        // Unlink our own socket. If `agent/sock` still points at us, remove it
        // too — a later server's `tick` would otherwise see a dangling link
        // and have to take over, and a client would get one failed connect in
        // the meantime. Best-effort; a crash leaves it for GC + takeover.
        if let Ok(target) = std::fs::read_link(&self.well_known) {
            if self.dir.join(target) == self.own_sock {
                let _ = std::fs::remove_file(&self.well_known);
            }
        }
        let _ = std::fs::remove_file(&self.own_sock);
    }
}

fn close_record(channel: u32) -> AgentRecord {
    AgentRecord {
        channel,
        kind: RecordKind::Close,
        payload: Vec::new(),
    }
}

fn own_pid() -> i32 {
    // SAFETY: getpid(2) takes no arguments and cannot fail.
    unsafe { libc::getpid() }
}

/// True if a process with `pid` still exists. `kill(pid, 0)` performs the
/// permission/existence check without sending a signal; ESRCH means gone.
fn pid_alive(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only probes; it touches no memory.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    // EPERM means the process exists but is owned by another uid — still
    // "alive" for GC purposes (not ours to reason about). Only ESRCH is dead.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Extracts the pid from a `srv-<pid>.sock` file name, or `None` if the name
/// does not match (so unrelated files in `agent/` are never GC'd).
fn srv_sock_pid(path: &Path) -> Option<i32> {
    let name = path.file_name()?.to_str()?;
    name.strip_prefix("srv-")?.strip_suffix(".sock")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private 0700 base dir with a SHORT path, so the unix sockets bound
    /// under `<base>/agent/srv-<pid>.sock` stay within SUN_LEN (~104). The
    /// scratch `$TMPDIR` is too deep, so anchor at `/tmp` like the production
    /// `/tmp/posh-<uid>` fallback. A per-process atomic counter keeps parallel
    /// tests from colliding without a long timestamp suffix.
    fn temp_base() -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = PathBuf::from(format!("/tmp/posh-agt-{}-{}", own_pid(), n));
        std::fs::remove_dir_all(&base).ok();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)
            .unwrap();
        base
    }

    #[test]
    fn new_creates_private_dir_and_claims_symlink() {
        let base = temp_base();
        let ep = AgentEndpoint::new(&base).unwrap();
        // agent/ exists, 0700, and the well-known link points at our socket.
        let target = std::fs::read_link(ep.sock_path()).unwrap();
        assert_eq!(target.to_str().unwrap(), format!("srv-{}.sock", own_pid()));
        assert!(ep.own_sock.exists());
        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn rejects_symlinked_agent_dir() {
        // A pre-planted symlink at <base>/agent must be refused by the shared
        // #7 hardening rather than followed.
        let base = temp_base();
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, base.join("agent")).unwrap();
        assert!(AgentEndpoint::new(&base).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn newest_endpoint_wins_the_symlink() {
        let base = temp_base();
        let first = AgentEndpoint::new(&base).unwrap();
        // Same pid in-test, so distinguish by socket path rather than pid:
        // the second construction re-claims the link (idempotent here, but the
        // rename path is exercised). The link must resolve to a live socket.
        let second = AgentEndpoint::new(&base).unwrap();
        let target = std::fs::read_link(second.sock_path()).unwrap();
        assert!(base.join("agent").join(target).exists());
        drop(second);
        drop(first);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn takeover_reclaims_a_dangling_symlink() {
        let base = temp_base();
        let mut ep = AgentEndpoint::new(&base).unwrap();
        // Simulate a dead owner: point agent/sock at a nonexistent srv socket.
        let agent_dir = base.join("agent");
        let _ = std::fs::remove_file(agent_dir.join("sock"));
        std::os::unix::fs::symlink("srv-999999.sock", agent_dir.join("sock")).unwrap();
        assert!(ep.symlink_needs_takeover());
        // tick (forced past the slow-tick gate) reclaims it.
        ep.last_tick = 0;
        ep.tick(true, AGENT_SLOW_TICK_MS + 1);
        let target = std::fs::read_link(ep.sock_path()).unwrap();
        assert_eq!(target.to_str().unwrap(), format!("srv-{}.sock", own_pid()));
        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn channel_open_data_close_lifecycle() {
        let base = temp_base();
        let mut ep = AgentEndpoint::new(&base).unwrap();

        // A client connects to our srv socket.
        let mut client = UnixStream::connect(&ep.own_sock).unwrap();
        let opens = ep.accept_pending();
        assert_eq!(opens.len(), 1);
        assert_eq!(opens[0].kind, RecordKind::Open);
        let ch_id = opens[0].channel;

        // Client -> server bytes surface as a Data record.
        client.write_all(b"ssh-agent-request").unwrap();
        // Give the kernel a moment to deliver on the loopback socket.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let data = ep.read_channels();
        let joined: Vec<u8> = data
            .iter()
            .filter(|r| r.kind == RecordKind::Data)
            .flat_map(|r| r.payload.clone())
            .collect();
        assert_eq!(joined, b"ssh-agent-request");

        // apply_records with Data writes back through to the client.
        ep.apply_records(&[AgentRecord {
            channel: ch_id,
            kind: RecordKind::Data,
            payload: b"signature".to_vec(),
        }]);
        let mut got = [0u8; 9];
        client.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"signature");

        // Client closes -> a Close record, and the channel is reaped.
        drop(client);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let closes = ep.read_channels();
        assert!(closes.iter().any(|r| r.kind == RecordKind::Close));
        assert_eq!(ep.live_channel_count(), 0);

        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn channel_count_is_capped() {
        let base = temp_base();
        let mut ep = AgentEndpoint::new(&base).unwrap();
        // Open MAX_AGENT_CHANNELS connections; all accepted.
        let mut clients = Vec::new();
        for _ in 0..MAX_AGENT_CHANNELS {
            clients.push(UnixStream::connect(&ep.own_sock).unwrap());
        }
        let opens = ep.accept_pending();
        assert_eq!(opens.len(), MAX_AGENT_CHANNELS);
        assert_eq!(ep.live_channel_count(), MAX_AGENT_CHANNELS);

        // One more is refused (accepted then dropped, no Open record).
        let _over = UnixStream::connect(&ep.own_sock).unwrap();
        let more = ep.accept_pending();
        assert!(more.is_empty());
        assert_eq!(ep.live_channel_count(), MAX_AGENT_CHANNELS);

        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn gc_removes_dead_srv_sockets() {
        let base = temp_base();
        let ep = AgentEndpoint::new(&base).unwrap();
        let agent_dir = base.join("agent");
        // Plant a srv socket for a pid that is certainly dead.
        let dead = agent_dir.join("srv-999999.sock");
        UnixListener::bind(&dead).unwrap();
        assert!(dead.exists());
        ep.gc_dead_sockets();
        assert!(!dead.exists(), "dead srv socket should be GC'd");
        // Our own live socket is untouched.
        assert!(ep.own_sock.exists());
        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn peer_inactive_tick_closes_channels() {
        let base = temp_base();
        let mut ep = AgentEndpoint::new(&base).unwrap();
        let _client = UnixStream::connect(&ep.own_sock).unwrap();
        ep.accept_pending();
        assert_eq!(ep.live_channel_count(), 1);
        // Peer gone: the slow tick fast-fails the open channel.
        ep.last_tick = 0;
        let closes = ep.tick(false, AGENT_SLOW_TICK_MS + 1);
        assert!(closes.iter().any(|r| r.kind == RecordKind::Close));
        assert_eq!(ep.live_channel_count(), 0);
        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn drop_removes_own_socket_and_owned_symlink() {
        let base = temp_base();
        let ep = AgentEndpoint::new(&base).unwrap();
        let own = ep.own_sock.clone();
        let link = ep.sock_path().to_path_buf();
        assert!(own.exists());
        drop(ep);
        assert!(!own.exists(), "own socket unlinked on drop");
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "owned symlink removed on drop"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn srv_sock_pid_parses_only_matching_names() {
        assert_eq!(srv_sock_pid(Path::new("/x/srv-123.sock")), Some(123));
        assert_eq!(srv_sock_pid(Path::new("/x/sock")), None);
        assert_eq!(srv_sock_pid(Path::new("/x/srv-abc.sock")), None);
        assert_eq!(srv_sock_pid(Path::new("/x/other.sock")), None);
    }
}
