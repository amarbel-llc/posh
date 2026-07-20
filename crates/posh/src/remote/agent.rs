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
/// Read-syscall buffer for draining a channel socket. The drain loop reads
/// repeatedly until `WouldBlock`, so this only bounds bytes-per-`read()`, not
/// per-channel throughput; kept modest (16 KiB) so the stack buffer stays small
/// even with `MAX_AGENT_CHANNELS` channels read in one pass. Agent messages are
/// typically well under 1 KiB, and `AGENT_DATA` chunks the stream to ≤247 bytes
/// regardless, so a larger read buffer buys nothing.
const CHANNEL_READ_BUF: usize = 16 * 1024;
/// Cadence for the symlink-liveness / takeover check and dead-`srv-*.sock` GC.
const AGENT_SLOW_TICK_MS: u64 = 5_000;
/// Peer-silence window after which the endpoint fast-fails outstanding agent
/// requests (stricter than the loop's 60 s `PEER_TIMEOUT`): a `git push` gets
/// `SSH_AGENT_FAILURE` rather than hanging when the peer has roamed away. The
/// `server_loop` computes the gate against this and passes it to [`tick`].
pub(crate) const AGENT_PEER_ACTIVE: u64 = 15_000; // ms

/// One forwarded agent connection: the `u32` id matches it to a record-stream
/// channel, the `stream` is the live unix socket. On the server end the stream
/// is an accepted connection from an agent client (`git`, `ssh`, …); on the
/// client end it is an outbound connection to the user's local agent. The
/// channel machinery is otherwise identical, so both ends share it.
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
    /// The identity this endpoint's socket is keyed by. In production it is the
    /// server process's pid (`srv-<pid>.sock`), which is what makes the
    /// `gc_dead_sockets` / `socket_is_dead` liveness probes meaningful. It is a
    /// field rather than a re-read of `own_pid()` so tests can stand up two
    /// COEXISTING endpoints in one process — otherwise both bind the same path
    /// and the second silently clobbers the first, which is why the multi-
    /// connection handoff (posh#136 / FDR 0014) had no in-process coverage.
    id: i32,
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
    /// with a tempdir), keyed by this process's pid.
    pub fn new(base: &Path) -> Result<AgentEndpoint> {
        AgentEndpoint::build(base, own_pid())
    }

    /// [`new`](Self::new) with an explicit socket identity instead of this
    /// process's pid, so a test can build two COEXISTING endpoints under one
    /// base dir — otherwise both bind `srv-<own_pid()>.sock` and the second
    /// clobbers the first (see the `id` field).
    ///
    /// Test-only, and gated so it cannot be reached from production: the id is
    /// not free-form. `gc_dead_sockets` reaps any `srv-<id>.sock` whose `id` is
    /// not a live pid, so a caller passing an arbitrary integer has its own
    /// socket unlinked by the next sibling sweep. Callers MUST pass a live pid;
    /// the handoff test uses `1` (init) for exactly this reason.
    #[cfg(test)]
    pub fn new_with_id(base: &Path, id: i32) -> Result<AgentEndpoint> {
        AgentEndpoint::build(base, id)
    }

    /// The real constructor behind [`new`](Self::new): creates `<base>/agent/`
    /// 0700, hardens it with the shared #7 check, binds `srv-<id>.sock`, and
    /// claims `agent/sock`.
    fn build(base: &Path, id: i32) -> Result<AgentEndpoint> {
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

        let own_sock = dir.join(format!("srv-{id}.sock"));
        // A stale socket for our own pid (pid reuse after an unclean exit)
        // would make bind fail with EADDRINUSE; clear it first.
        let _ = std::fs::remove_file(&own_sock);
        let listener = UnixListener::bind(&own_sock)?;
        listener.set_nonblocking(true)?;

        let endpoint = AgentEndpoint {
            dir: dir.clone(),
            id,
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
        let target = format!("srv-{}.sock", self.id);
        let tmp = self.dir.join(format!(".sock.{}.tmp", self.id));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&target, &tmp)?;
        std::fs::rename(&tmp, &self.well_known)?;
        Ok(())
    }

    /// True when `agent/sock` is absent, dangling, or points at a dead
    /// `srv-*.sock` — i.e. nobody live owns the endpoint and we should claim
    /// it. A live link pointing at *another* live server is left alone.
    ///
    /// Liveness is decided by the target's OWNING PID (`kill(pid, 0)`), never by
    /// connecting to it. The obvious probe — `session::socket_is_dead`, which
    /// dials the socket — is wrong here in a way it is not for session sockets:
    /// an `AgentEndpoint` listener treats every accepted connection as an agent
    /// request, so probing by connect opens a phantom channel. In the ordinary
    /// single-connection case the link points at OUR OWN socket, so the endpoint
    /// probed itself every slow tick, emitted an `Open`, made the client dial the
    /// user's real `$SSH_AUTH_SOCK`, and saturated the once-a-minute agent-use
    /// notice with a request that never happened (posh#147).
    ///
    /// That last part was security-relevant, not cosmetic. `AgentNotice` advances
    /// its rate-limit clock only when it fires, so a phantom at t=0 spent the
    /// minute's slot and a GENUINE agent use at t=30s was silently suppressed —
    /// with a 5 s probe against a 60 s window, real uses routinely went
    /// unannounced. The notice is what justifies FDR 0004 forwarding by default,
    /// so it has to mean something.
    ///
    /// A pid check is also strictly cheaper, and it is what `gc_dead_sockets`
    /// already uses to reap the same files — the two now agree by construction.
    fn symlink_needs_takeover(&self) -> bool {
        match std::fs::read_link(&self.well_known) {
            Err(_) => true, // absent or not a symlink
            Ok(target) => {
                // Targets are stored relative to `dir` (e.g. "srv-123.sock").
                let resolved = self.dir.join(&target);
                match srv_sock_pid(&resolved) {
                    // A name we do not recognise as `srv-<pid>.sock` is not
                    // something we can prove live, and nothing we wrote. Treat
                    // it as takeable rather than deferring to it forever.
                    None => true,
                    Some(pid) => !pid_alive(pid),
                }
            }
        }
    }

    /// Whether `agent/sock` currently resolves to *our own* socket — the healthy
    /// post-`claim_symlink` state. False means another server took it over (a
    /// roam or takeover), or the link is missing/dangling. (FDR 0004.)
    fn symlink_points_to_self(&self) -> bool {
        match std::fs::read_link(&self.well_known) {
            Ok(target) => self.dir.join(target) == self.own_sock,
            Err(_) => false,
        }
    }

    /// Give up `agent/sock` if we own it (same unlink as `Drop`, but while the
    /// endpoint keeps running). Called when OUR peer goes inactive: our
    /// `srv-<pid>.sock` is still bound, so `socket_is_dead` reports us "alive"
    /// and no other endpoint would ever take over — starving a sibling
    /// connection whose client IS active (posh#136). Releasing the link lets the
    /// next active endpoint's `symlink_needs_takeover()` fire (absent ⇒ true).
    /// We reclaim it on a later tick once our peer is active again.
    fn release_symlink(&self) {
        if self.symlink_points_to_self() {
            let _ = std::fs::remove_file(&self.well_known);
        }
    }

    /// A snapshot of this endpoint's state for the server→client agent-forwarding
    /// diagnostic (FDR 0004): the live channel count, the next channel id
    /// to be assigned, and whether we still own the well-known symlink. Rides the
    /// `CAP_DIAG` `ServerDiag` v2 payload; only built in a debug/agent posture on
    /// a paced frame stream, so its one `read_link` is not a hot path.
    /// `bytes_sent`/`bytes_queued` come from the connection's `AgentStream`,
    /// which `server_loop` owns separately from the endpoint — the endpoint knows
    /// about channels, the stream about bytes, and the diagnostic joins them.
    pub fn diag(&self, bytes_sent: u64, bytes_queued: u64) -> crate::remote::caps::AgentDiag {
        crate::remote::caps::AgentDiag {
            live_channels: self.live_channel_count() as u32,
            next_channel_id: self.next_channel_id,
            symlink_ok: self.symlink_points_to_self(),
            bytes_sent,
            bytes_queued,
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
        read_channel_data(&mut self.channels)
    }

    /// Dispatches records decoded from the client's agent stream to their
    /// channel sockets: `Data` writes through; `Close`/`Fail` tear the channel
    /// down (the agent client's read then sees EOF, i.e. a failed request). An
    /// `Open` on this (server) end is a peer bug — OPEN only flows
    /// remote->client — and is ignored.
    pub fn apply_records(&mut self, records: &[AgentRecord]) {
        for rec in records {
            apply_data_or_close(&mut self.channels, rec);
            // OPEN reaching the server end is ignored by apply_data_or_close.
        }
        reap_closed(&mut self.channels);
    }

    /// Periodic maintenance, gated to `AGENT_SLOW_TICK_MS`. Returns any
    /// `Close` records produced (e.g. by the peer-inactive fast-fail) for the
    /// caller to forward. `peer_active` is the loop's existing liveness gate.
    pub fn tick(&mut self, peer_active: bool, now: u64) -> Vec<AgentRecord> {
        if now.saturating_sub(self.last_tick) < AGENT_SLOW_TICK_MS {
            return Vec::new();
        }
        self.last_tick = now;

        if peer_active {
            // Own the endpoint only while OUR client is active. Reclaim a link
            // whose owner died or went stale — but only when we can actually
            // serve it (an active peer). Claiming it while our own peer is
            // inactive is exactly the posh#136 starvation: we'd hold `agent/sock`
            // pointing at a socket that fast-fails every request.
            if self.symlink_needs_takeover() {
                let _ = self.claim_symlink();
            }
        } else {
            // Our peer is gone: relinquish `agent/sock` if we hold it, so a
            // sibling endpoint whose client IS active can take over (its
            // `symlink_needs_takeover()` sees the link absent). Without this the
            // link stays pinned to us — `socket_is_dead` reports our still-bound
            // listener "alive" — and active siblings are starved (posh#136).
            self.release_symlink();
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
        live_count(&self.channels)
    }

    fn reap_closed(&mut self) {
        reap_closed(&mut self.channels);
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

/// The client side of agent forwarding (FDR 0004 work item 4): the mirror of
/// [`AgentEndpoint`]. Where the endpoint *accepts* connections on the remote
/// host and the user's agent lives at the far end, the client *connects* —
/// each `Open` record from the server opens a fresh connection to the user's
/// local agent socket (`$SSH_AUTH_SOCK` or a `--forward-agent=PATH` override),
/// and bytes are proxied back over the same record stream. No symlink, no
/// listener, no GC: the client owns no shared filesystem endpoint, just the
/// outbound connections it dials on demand.
pub struct AgentClient {
    /// The local agent socket every channel dials. Resolved once at startup;
    /// a path that dies mid-session degrades to per-`Open` `Fail` (design §1).
    source: PathBuf,
    channels: Vec<Channel>,
    /// Per-channel first-request classifiers, keyed by channel id. An entry is
    /// created on `Open`, drained once the header completes, and dropped on
    /// close — so this holds at most `MAX_AGENT_CHANNELS` short-lived buffers.
    sniffers: Vec<(u32, OpSniffer)>,
    /// Classifications produced since the last [`take_ops`](Self::take_ops).
    /// Returned out-of-band rather than through `apply_records`, whose return
    /// value is the outbound record stream and should stay that.
    ops: Vec<AgentOp>,
}

impl AgentClient {
    /// Builds a proxy that forwards the agent at `source` — the local socket
    /// resolved by the CLI/env policy (`--forward-agent=PATH`, `$SSH_AUTH_SOCK`,
    /// …) and dialed afresh on each `Open`.
    pub fn new(source: PathBuf) -> AgentClient {
        AgentClient {
            source,
            channels: Vec::new(),
            sniffers: Vec::new(),
            ops: Vec::new(),
        }
    }

    /// Drains the agent operations classified since the last call — what the
    /// remote actually asked for, for the use-notice (FDR 0004). Empty until a
    /// channel's first request header has fully arrived, which is deliberately
    /// LATER than its `Open`: an open alone says nothing about intent, and
    /// notifying on it is what let a channel that never carried a request
    /// masquerade as agent use (posh#147).
    pub fn take_ops(&mut self) -> Vec<AgentOp> {
        std::mem::take(&mut self.ops)
    }

    /// The local agent socket every channel dials (FDR 0004 diagnostics).
    pub fn source(&self) -> &std::path::Path {
        &self.source
    }

    /// Channel fds for `client_loop`'s poll set (no listener — the client only
    /// has its outbound connections).
    pub fn pollfds(&self) -> Vec<libc::pollfd> {
        self.channels
            .iter()
            .filter(|c| !c.closed)
            .map(|c| util::pollfd(c.stream.as_raw_fd(), libc::POLLIN))
            .collect()
    }

    /// Reads readable channels into `Data`/`Close` records (shared with the
    /// endpoint). The caller frames these onto the outbound `AgentStream`.
    pub fn read_channels(&mut self) -> Vec<AgentRecord> {
        read_channel_data(&mut self.channels)
    }

    /// Applies records decoded from the server's agent stream. `Open` dials the
    /// local agent and opens a channel (or replies `Fail` if it can't connect,
    /// or the channel cap is hit); `Data` writes through; `Close`/`Fail` tears
    /// the channel down. Returns any records to send back to the server (the
    /// `Fail` replies). Connect uses a blocking connect then switches the
    /// socket to non-blocking — agent sockets are local, so the connect is
    /// effectively immediate.
    pub fn apply_records(&mut self, records: &[AgentRecord]) -> Vec<AgentRecord> {
        let mut out = Vec::new();
        for rec in records {
            match rec.kind {
                RecordKind::Open => {
                    if live_count(&self.channels) >= MAX_AGENT_CHANNELS {
                        out.push(fail_record(rec.channel));
                        continue;
                    }
                    match self.connect_channel(rec.channel) {
                        Ok(()) => self.sniffers.push((rec.channel, OpSniffer::new())),
                        Err(_) => out.push(fail_record(rec.channel)),
                    }
                }
                RecordKind::Data => {
                    // Classify the channel's requests before proxying them on
                    // (read-only; the bytes are forwarded unchanged either way).
                    if let Some(i) = self.sniffers.iter().position(|(id, _)| *id == rec.channel) {
                        let ops = self.sniffers[i].1.push(&rec.payload);
                        self.ops.extend(ops);
                    }
                    apply_data_or_close(&mut self.channels, rec);
                }
                _ => apply_data_or_close(&mut self.channels, rec),
            }
        }
        reap_closed(&mut self.channels);
        // A channel torn down leaves no sniffer behind. One sweep covers every
        // way a channel can end — a Close/Fail record, or a local write error in
        // `apply_data_or_close` — so no per-record removal is needed.
        self.sniffers
            .retain(|(id, _)| self.channels.iter().any(|c| c.id == *id && !c.closed));
        out
    }

    fn connect_channel(&mut self, id: u32) -> std::io::Result<()> {
        let stream = UnixStream::connect(&self.source)?;
        stream.set_nonblocking(true)?;
        self.channels.push(Channel {
            id,
            stream,
            closed: false,
        });
        Ok(())
    }

    /// Count of currently-open forwarded channels (FDR 0004 diagnostics).
    pub fn live_channel_count(&self) -> usize {
        live_count(&self.channels)
    }
}

// ---------------------------------------------------------------------------
// Forwarding-policy resolution (FDR 0004 §Interface). Pure: maps the CLI flag,
// $POSH_FORWARD_AGENT, and $SSH_AUTH_SOCK to a decision, so the precedence is
// unit-tested without touching the environment or spawning anything. The CLI
// parses argv into a `ForwardFlag`; the caller reads the two env vars; this
// function applies `flag > env > default`.

/// The forwarding flag as parsed from argv (the highest-precedence input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardFlag {
    /// No `-a`/`-A`/`--forward-agent` given — fall through to env/default.
    Unset,
    /// `-a` / `--no-forward-agent`: disable for this connection.
    Disable,
    /// Bare `-A` / `--forward-agent`: explicit enable — warn loudly if no agent.
    ExplicitOn,
    /// `--forward-agent=PATH`: forward a specific socket instead of the default.
    Path(PathBuf),
}

/// The resolved decision for a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardPolicy {
    Off,
    /// Forward the agent socket at `source`.
    On { source: PathBuf },
}

/// Applies the `flag > env > default` precedence (FDR 0004 Interface table).
/// `env` is `$POSH_FORWARD_AGENT` (None when unset/empty); `auth_sock` is
/// `$SSH_AUTH_SOCK` (None when unset/empty). Returns the policy plus an optional
/// loud-warning string — set only for the explicit `-A`-but-no-usable-agent
/// case, which the FDR singles out as the difference between `-A` and the
/// silent best-effort default.
pub fn resolve_forward_policy(
    flag: &ForwardFlag,
    env: Option<&str>,
    auth_sock: Option<&str>,
) -> (ForwardPolicy, Option<String>) {
    let on = |p: &str| ForwardPolicy::On {
        source: PathBuf::from(p),
    };
    let usable_sock = auth_sock.filter(|s| !s.is_empty());
    // The env var is overloaded: `no`/`0` is the profile opt-out, the empty
    // string is "unset", and any other value names a custom source socket.
    // Classify it once; the flag decides how the classification is used.
    let env_optout = matches!(env, Some("no") | Some("0"));
    let env_path = env.filter(|p| !p.is_empty() && !env_optout);
    // The forwarding SOURCE resolves env-path-then-default ($SSH_AUTH_SOCK).
    // The opt-out only suppresses the source on the default path (no flag); an
    // explicit `-A` overrides it (flag > env) and falls through to the socket.
    let source_for_explicit = env_path.or(usable_sock);
    let source_for_default = if env_optout { None } else { source_for_explicit };

    match flag {
        // `-a` always wins.
        ForwardFlag::Disable => (ForwardPolicy::Off, None),
        // `--forward-agent=PATH`: forward exactly that socket, no questions.
        ForwardFlag::Path(p) => (
            ForwardPolicy::On {
                source: p.clone(),
            },
            None,
        ),
        // Bare `-A`: explicit enable, overriding an env opt-out. Forward the
        // resolved source ($POSH_FORWARD_AGENT path, else $SSH_AUTH_SOCK);
        // unlike the silent default, complain loudly and stay off when none
        // resolves.
        ForwardFlag::ExplicitOn => match source_for_explicit {
            Some(s) => (on(s), None),
            None => (
                ForwardPolicy::Off,
                Some(
                    "posh: -A given but no usable agent ($POSH_FORWARD_AGENT / \
                     $SSH_AUTH_SOCK); forwarding off"
                        .to_string(),
                ),
            ),
        },
        // No flag: best-effort default — forward the resolved source when one
        // exists and the env did not opt out, else proceed silently.
        ForwardFlag::Unset => match source_for_default {
            Some(s) => (on(s), None),
            None => (ForwardPolicy::Off, None),
        },
    }
}

// ---------------------------------------------------------------------------
// Per-request agent-use notice (FDR 0004 §Limitations; github #96). With
// default-on forwarding, a one-line client banner — "agent forwarding
// requested by <host>" — is the only ambient signal that the remote host is
// exercising the local agent. Rate-limited to one line per minute so heavy
// `git` use doesn't flood the notify line, silenced entirely by
// POSH_AGENT_NOTICE=no. The rate-limit + silence logic is pure here so it is
// unit-tested without the loop or the NotificationEngine.

/// Minimum gap between notices (FDR 0004: "one line per host per minute"). The
/// roaming client has a single peer, so this is effectively one timestamp gate.
const AGENT_NOTICE_INTERVAL_MS: u64 = 60_000;

// ---------------------------------------------------------------------------
// Agent-request classification (FDR 0004 notice context).
//
// The forwarded stream is the SSH agent protocol: `[u32 BE length][u8 type][…]`.
// posh proxies it opaquely and MUST keep doing so — but the client end peeks at
// the first request's TYPE byte, because the difference between "listed your
// keys" and "signed with your key" is the whole point of the notice. The peek is
// read-only, happens on the client (where the user's own agent lives), and
// touches no key material, so RFC 0008's "the daemon never brokers keys"
// boundary is untouched.

/// `SSH_AGENTC_REQUEST_IDENTITIES` — "list my keys". Low sensitivity: every ssh
/// connection issues one before it does anything interesting.
const AGENTC_REQUEST_IDENTITIES: u8 = 11;
/// `SSH_AGENTC_SIGN_REQUEST` — the private key is actually being USED. This is
/// the event worth interrupting the user for.
const AGENTC_SIGN_REQUEST: u8 = 13;
/// Bytes needed to classify: the 4-byte length prefix plus the type byte.
const AGENT_REQUEST_HEADER: usize = 5;

/// What the remote asked the forwarded agent to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOp {
    /// Enumerate the available public keys.
    ListKeys,
    /// Sign with a private key — a real use of the user's credential.
    Sign,
    /// Anything else in the agent protocol (add/remove/lock/extension).
    Other(u8),
}

/// Classifies EVERY request on one agent channel.
///
/// Deliberately not just the first. One agent connection commonly carries a
/// `REQUEST_IDENTITIES` to discover the available keys followed by a
/// `SIGN_REQUEST` to use one, so classifying only the opening request would
/// label such a channel a harmless listing and never report the signature —
/// reintroducing posh#147's "real key use goes unannounced" by another route.
///
/// It is a skipping parser, not a buffering one: it accumulates the 5-byte
/// header (per ADR-0003, which may be split across records), reads the type,
/// then *counts down* the request body without copying it. Payloads — which is
/// where key blobs and signed data live — are never retained.
#[derive(Default)]
pub struct OpSniffer {
    /// Partial `[u32 BE length][u8 type]` header being accumulated.
    head: Vec<u8>,
    /// Bytes of the current request's body still to be skipped.
    skip: u64,
}

impl OpSniffer {
    pub fn new() -> OpSniffer {
        OpSniffer::default()
    }

    /// Feeds channel bytes, returning one classification per complete request
    /// header seen. Usually empty or a single entry; a record carrying several
    /// small requests yields several.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<AgentOp> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            // Mid-body: discard as much of it as this record carries.
            if self.skip > 0 {
                let n = self.skip.min(rest.len() as u64) as usize;
                self.skip -= n as u64;
                rest = &rest[n..];
                continue;
            }
            let want = AGENT_REQUEST_HEADER - self.head.len();
            let take = want.min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.head.len() < AGENT_REQUEST_HEADER {
                break; // header split across records; resume next time
            }
            let len = u32::from_be_bytes([self.head[0], self.head[1], self.head[2], self.head[3]]);
            let kind = self.head[4];
            self.head.clear();
            // `len` covers the type byte plus the body; a zero length is
            // malformed, and saturating keeps it from wrapping into a huge skip.
            self.skip = (len as u64).saturating_sub(1);
            out.push(match kind {
                AGENTC_REQUEST_IDENTITIES => AgentOp::ListKeys,
                AGENTC_SIGN_REQUEST => AgentOp::Sign,
                other => AgentOp::Other(other),
            });
        }
        out
    }
}

/// Client-side rate limiter for the agent-use notice. Owns the silence flag,
/// the last-fired timestamp, and the host it names — the host is only
/// meaningful together with the notice, so they live and die as one (the
/// notice exists only while forwarding is active). `on_channel_open` is the
/// gate.
pub struct AgentNotice {
    silenced: bool,
    last_shown: Option<u64>,
    host: String,
}

impl AgentNotice {
    /// Builds the limiter for `host` from the environment: `POSH_AGENT_NOTICE=no`
    /// (or `0`) silences it; anything else (including unset) leaves it enabled —
    /// the FDR ships it on in v1.
    #[allow(dead_code)] // wired into the client loop alongside this type
    pub fn from_env(host: &str) -> AgentNotice {
        let silenced = matches!(
            std::env::var("POSH_AGENT_NOTICE").ok().as_deref(),
            Some("no") | Some("0")
        );
        AgentNotice {
            silenced,
            last_shown: None,
            host: host.to_string(),
        }
    }

    /// Builds a limiter with an explicit silence flag (the seam the tests use).
    #[cfg(test)]
    pub fn new(silenced: bool, host: &str) -> AgentNotice {
        AgentNotice {
            silenced,
            last_shown: None,
            host: host.to_string(),
        }
    }

    /// Called when a classified request arrives on a forwarded-agent channel.
    /// Returns the banner text, or `None` when silenced or rate-limited.
    ///
    /// The two cases are limited SEPARATELY, and that separation is the point.
    /// A signature is a real use of the user's private key and is **always**
    /// announced — no window, no sharing a slot with anything else. Key listings
    /// (which every ssh connection issues, and which reveal no secret) keep the
    /// old one-per-minute limit.
    ///
    /// Before this split, a single shared limiter meant an uninteresting open
    /// could spend the window and a genuine signature seconds later went
    /// unreported — which under posh#147 happened routinely, since a liveness
    /// probe opened a channel every 5s against a 60s window. A notice that can
    /// silently miss the event it exists to report is not a control at all, and
    /// FDR 0004 forwards by default *because* the notice exists.
    pub fn on_request(&mut self, op: AgentOp, now: u64) -> Option<String> {
        if self.silenced {
            return None;
        }
        match op {
            // Never rate-limited: each signature is a distinct use of a key. If
            // something signs in a loop, the user especially wants to know.
            AgentOp::Sign => Some(format!("{} SIGNED with your forwarded ssh key", self.host)),
            // Also never rate-limited, and never described as a listing. The
            // request types posh does not name include ones that MUTATE the
            // local agent — add/remove identity, remove-all, lock/unlock — which
            // are more notable than a listing, not less. Reporting them as "listed
            // your keys" would understate a key deletion. Announcing every one is
            // affordable precisely because ordinary traffic is only listings and
            // signatures, so this is rare by construction; and if it stops being
            // rare, that is itself worth seeing.
            AgentOp::Other(kind) => Some(format!(
                "{} made an unrecognised ssh-agent request (type {kind}) — \
                 this may modify your agent",
                self.host
            )),
            AgentOp::ListKeys => {
                let due = match self.last_shown {
                    Some(t) => now.saturating_sub(t) >= AGENT_NOTICE_INTERVAL_MS,
                    None => true,
                };
                if !due {
                    return None;
                }
                self.last_shown = Some(now);
                Some(format!("{} listed your forwarded ssh keys", self.host))
            }
        }
    }
}

/// An empty-payload control record. `close_record`/`fail_record` are the named
/// call sites — `Close` (orderly end) and `Fail` (the client end couldn't reach
/// the local agent) carry no bytes, only the channel and kind.
fn control_record(channel: u32, kind: RecordKind) -> AgentRecord {
    AgentRecord {
        channel,
        kind,
        payload: Vec::new(),
    }
}

fn close_record(channel: u32) -> AgentRecord {
    control_record(channel, RecordKind::Close)
}

fn fail_record(channel: u32) -> AgentRecord {
    control_record(channel, RecordKind::Fail)
}

// ---------------------------------------------------------------------------
// Channel-table machinery shared by both ends (AgentEndpoint accepts; the
// AgentClient connects). The byte pump and teardown are direction-agnostic;
// only how a channel is *created* (accept vs connect on OPEN) differs.

fn live_count(channels: &[Channel]) -> usize {
    channels.iter().filter(|c| !c.closed).count()
}

fn reap_closed(channels: &mut Vec<Channel>) {
    channels.retain(|c| !c.closed);
}

/// Reads every readable channel non-blocking, producing `Data` records for
/// fresh bytes and a `Close` on EOF/error. Reaps closed channels before
/// returning. Identical on both ends.
fn read_channel_data(channels: &mut Vec<Channel>) -> Vec<AgentRecord> {
    let mut out = Vec::new();
    for ch in channels.iter_mut() {
        if ch.closed {
            continue;
        }
        let mut buf = [0u8; CHANNEL_READ_BUF];
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
    reap_closed(channels);
    out
}

/// Applies one inbound record's `Data`/`Close`/`Fail` to its channel: `Data`
/// writes through (a failed write closes just that channel — a half-written
/// agent request is a failed request), `Close`/`Fail` tear it down. `Open` and
/// unknown-channel records are no-ops here; the OPEN-creates-a-channel step is
/// the per-end caller's job. Does not reap — the caller reaps after a batch.
fn apply_data_or_close(channels: &mut [Channel], rec: &AgentRecord) {
    let Some(ch) = channels.iter_mut().find(|c| c.id == rec.channel) else {
        return;
    };
    match rec.kind {
        RecordKind::Data => {
            if ch.stream.write_all(&rec.payload).is_err() {
                ch.closed = true;
            }
        }
        RecordKind::Open => {} // handled by the caller, never written through
        RecordKind::Close | RecordKind::Fail => ch.closed = true,
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

    // posh#136: an endpoint whose PEER goes inactive must relinquish `agent/sock`
    // (not keep it pinned to its still-bound-but-unserved socket), so a sibling
    // endpoint with an active client can take over. Without the release, the
    // link stays ours (`socket_is_dead` sees our listener "alive") and every
    // request routed here fast-fails — starving the active sibling.
    #[test]
    fn inactive_peer_releases_the_owned_symlink() {
        let base = temp_base();
        let mut ep = AgentEndpoint::new(&base).unwrap();
        assert!(ep.symlink_points_to_self(), "fresh endpoint owns agent/sock");
        // A tick with peer_active=false relinquishes the link (past the gate).
        ep.last_tick = 0;
        ep.tick(false, AGENT_SLOW_TICK_MS + 1);
        assert!(
            std::fs::symlink_metadata(ep.sock_path()).is_err(),
            "an inactive-peer tick must remove the symlink it owned (posh#136)"
        );
        assert!(ep.own_sock.exists(), "our listener socket itself stays bound");
        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    // posh#136: once released (or absent), an endpoint whose peer is ACTIVE
    // reclaims `agent/sock` on the next tick — so the stable path resolves to a
    // live, active endpoint again.
    #[test]
    fn active_peer_reclaims_a_released_symlink() {
        let base = temp_base();
        let mut ep = AgentEndpoint::new(&base).unwrap();
        // Release it (simulate our own earlier inactive-peer tick).
        ep.release_symlink();
        assert!(
            std::fs::symlink_metadata(ep.sock_path()).is_err(),
            "released link is gone"
        );
        // A tick with peer_active=true reclaims it.
        ep.last_tick = 0;
        ep.tick(true, AGENT_SLOW_TICK_MS + 1);
        assert!(ep.symlink_points_to_self(), "active peer reclaims the link");
        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    // posh#136 / FDR 0014: the shipped relinquish-on-inactive fix (option 1)
    // removed the STARVATION — a roamed-away owner no longer pins `agent/sock`
    // forever — but it did not close the window, and this is the first test with
    // two COEXISTING endpoints, so it is the first to show the handoff at all.
    //
    // The handoff costs TWO independent slow ticks: the owner releases on its
    // own next tick, and the active sibling claims on ITS next tick after that.
    // Across the whole interval `agent/sock` is unusable in two distinct ways,
    // measured separately here because they fail differently for a `git push`:
    //
    //   - STALE: the link still resolves to the inactive owner, whose listener is
    //     bound (so `socket_is_dead` is false and no sibling takes over) but whose
    //     `tick(peer_active=false)` fast-fails every channel => SSH_AGENT_FAILURE.
    //   - ABSENT: the owner has released and nobody has claimed yet => the
    //     connect(2) itself fails with ENOENT.
    //
    // This is the residual defect FDR 0014 exists to close by construction.
    #[test]
    fn handoff_between_two_endpoints_leaves_a_multi_tick_outage() {
        const STEP_MS: u64 = 100;
        let base = temp_base();
        let agent_dir = base.join("agent");

        // Two coexisting endpoints under one base. `a` is the sibling whose
        // client stays ACTIVE; `b` is the newest connection, so it owns the link,
        // and its client is INACTIVE for the whole run (roamed away). `b`'s id is
        // pid 1, which is always a live process — otherwise `a`'s
        // `gc_dead_sockets` would reap `b`'s socket and confound the measurement.
        let mut a = AgentEndpoint::new_with_id(&base, own_pid()).unwrap();
        let mut b = AgentEndpoint::new_with_id(&base, 1).unwrap();
        let b_target = PathBuf::from("srv-1.sock");
        assert_eq!(
            std::fs::read_link(a.sock_path()).unwrap(),
            b_target,
            "the newest endpoint owns agent/sock"
        );

        // Both endpoints last ticked at t=0; the clock advances from there.
        a.last_tick = 0;
        b.last_tick = 0;

        let (mut stale_ms, mut absent_ms, mut served_ms) = (0u64, 0u64, 0u64);
        let mut t = 0;
        while t < AGENT_SLOW_TICK_MS * 4 {
            t += STEP_MS;
            // Each server_loop ticks its own endpoint with its own peer state.
            a.tick(true, t);
            b.tick(false, t);
            match std::fs::read_link(agent_dir.join("sock")) {
                Err(_) => absent_ms += STEP_MS,
                Ok(target) if target == b_target => stale_ms += STEP_MS,
                Ok(_) => served_ms += STEP_MS,
            }
        }

        println!(
            "posh#136 handoff: stale={stale_ms}ms absent={absent_ms}ms \
             (unusable={}ms) served={served_ms}ms over {t}ms",
            stale_ms + absent_ms
        );

        assert!(
            stale_ms > 0,
            "agent/sock resolves to the inactive owner, which fast-fails requests"
        );
        assert!(
            absent_ms > 0,
            "and then vanishes entirely before the active sibling reclaims it"
        );
        assert!(
            stale_ms + absent_ms >= AGENT_SLOW_TICK_MS,
            "the outage spans more than a single slow tick: stale={stale_ms}ms \
             absent={absent_ms}ms"
        );
        assert!(
            served_ms > 0,
            "the active sibling does eventually take over (option 1 shipped)"
        );

        drop(b);
        drop(a);
        std::fs::remove_dir_all(&base).ok();
    }

    // posh#147: the takeover check MUST NOT probe by connecting. An
    // `AgentEndpoint` listener treats every accepted connection as an agent
    // request, and in the ordinary single-connection case `agent/sock` points at
    // our OWN socket — so a connecting probe made the endpoint open a phantom
    // channel against itself on every slow tick, which then made the client dial
    // the user's real `$SSH_AUTH_SOCK` and saturated the once-a-minute agent-use
    // notice with a request that never happened.
    //
    // Before the fix this test found exactly 1 channel; it is the regression
    // guard for using a pid check instead of a connect.
    #[test]
    fn takeover_check_does_not_open_a_channel_against_itself() {
        let base = temp_base();
        let mut ep = AgentEndpoint::new(&base).unwrap();
        assert!(ep.symlink_points_to_self(), "we own the link, so we are the probe target");
        assert_eq!(ep.accept_pending().len(), 0, "no channels before the tick");

        // A full slow tick with an active peer: runs the takeover check against
        // a link pointing at our own live socket.
        ep.last_tick = 0;
        ep.tick(true, AGENT_SLOW_TICK_MS + 1);

        assert_eq!(
            ep.accept_pending().len(),
            0,
            "the liveness probe must not land on our listener as an agent channel (posh#147)"
        );
        assert!(
            ep.symlink_points_to_self(),
            "and we must still own the link — a live owner is not taken over from"
        );

        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    // The general invariant behind posh#147, and the one worth guarding: an idle
    // forwarding connection — one whose peer is active but where nothing is
    // actually asking for the agent — MUST produce no agent channels at all,
    // ever. #147 violated it via the takeover probe, but the reason it mattered
    // was generic: every channel open is announced to the user as agent use, and
    // consumes the notice's rate-limit slot (see
    // `a_spurious_open_suppresses_a_real_one_for_a_full_window`). Any FUTURE
    // source of spurious opens would be just as harmful, so guard the property
    // rather than the one bug.
    #[test]
    fn an_idle_endpoint_opens_no_channels_over_many_ticks() {
        let base = temp_base();
        let mut ep = AgentEndpoint::new(&base).unwrap();

        // Ten minutes of virtual time at the slow-tick cadence, peer active
        // throughout, no agent client ever connecting.
        let mut now = 0u64;
        for _ in 0..120 {
            now += AGENT_SLOW_TICK_MS;
            ep.tick(true, now);
            assert_eq!(
                ep.accept_pending().len(),
                0,
                "an idle forwarding connection must open no agent channels (posh#147); \
                 every open is reported to the user as agent use"
            );
        }
        assert_eq!(ep.live_channel_count(), 0, "and none accumulated");

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

    // --- AgentClient (the local-agent proxy mirror) -----------------------

    /// A short path under /tmp for a fake-agent listener socket (SUN_LEN again).
    fn temp_sock() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        PathBuf::from(format!("/tmp/posh-fakeagt-{}-{}.sock", own_pid(), n))
    }

    #[test]
    fn client_open_connects_to_local_agent() {
        // Stand up a fake local agent; an OPEN record makes the client dial it.
        let sock = temp_sock();
        std::fs::remove_file(&sock).ok();
        let listener = UnixListener::bind(&sock).unwrap();
        let mut client = AgentClient::new(sock.clone());

        let fails = client.apply_records(&[AgentRecord {
            channel: 1,
            kind: RecordKind::Open,
            payload: Vec::new(),
        }]);
        assert!(fails.is_empty(), "a reachable agent must not FAIL");
        assert_eq!(client.live_channel_count(), 1);
        // The fake agent saw the connection.
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_ok());

        std::fs::remove_file(&sock).ok();
    }

    #[test]
    fn client_proxies_bytes_both_ways() {
        let sock = temp_sock();
        std::fs::remove_file(&sock).ok();
        let listener = UnixListener::bind(&sock).unwrap();
        let mut client = AgentClient::new(sock.clone());
        client.apply_records(&[AgentRecord {
            channel: 7,
            kind: RecordKind::Open,
            payload: Vec::new(),
        }]);
        let (mut agent_side, _) = listener.accept().unwrap();

        // Server-relayed request bytes -> written through to the fake agent.
        client.apply_records(&[AgentRecord {
            channel: 7,
            kind: RecordKind::Data,
            payload: b"request".to_vec(),
        }]);
        let mut got = [0u8; 7];
        agent_side.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"request");

        // Agent reply -> surfaces as a Data record headed back to the server.
        agent_side.write_all(b"reply").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let recs = client.read_channels();
        let joined: Vec<u8> = recs
            .iter()
            .filter(|r| r.kind == RecordKind::Data && r.channel == 7)
            .flat_map(|r| r.payload.clone())
            .collect();
        assert_eq!(joined, b"reply");

        std::fs::remove_file(&sock).ok();
    }

    #[test]
    fn client_open_to_dead_agent_replies_fail() {
        // No listener at the source: the OPEN connect fails and the client
        // answers FAIL on that channel rather than opening it.
        let sock = temp_sock();
        std::fs::remove_file(&sock).ok();
        let mut client = AgentClient::new(sock);
        let out = client.apply_records(&[AgentRecord {
            channel: 3,
            kind: RecordKind::Open,
            payload: Vec::new(),
        }]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, RecordKind::Fail);
        assert_eq!(out[0].channel, 3);
        assert_eq!(client.live_channel_count(), 0);
    }

    #[test]
    fn client_close_tears_down_channel() {
        let sock = temp_sock();
        std::fs::remove_file(&sock).ok();
        let _listener = UnixListener::bind(&sock).unwrap();
        let mut client = AgentClient::new(sock.clone());
        client.apply_records(&[AgentRecord {
            channel: 5,
            kind: RecordKind::Open,
            payload: Vec::new(),
        }]);
        assert_eq!(client.live_channel_count(), 1);
        client.apply_records(&[close_record(5)]);
        assert_eq!(client.live_channel_count(), 0);
        std::fs::remove_file(&sock).ok();
    }

    #[test]
    fn client_channel_count_is_capped() {
        let sock = temp_sock();
        std::fs::remove_file(&sock).ok();
        let _listener = UnixListener::bind(&sock).unwrap();
        let mut client = AgentClient::new(sock.clone());
        let opens: Vec<AgentRecord> = (0..MAX_AGENT_CHANNELS as u32)
            .map(|id| AgentRecord {
                channel: id,
                kind: RecordKind::Open,
                payload: Vec::new(),
            })
            .collect();
        assert!(client.apply_records(&opens).is_empty());
        assert_eq!(client.live_channel_count(), MAX_AGENT_CHANNELS);
        // One past the cap is refused with FAIL.
        let over = client.apply_records(&[AgentRecord {
            channel: 99,
            kind: RecordKind::Open,
            payload: Vec::new(),
        }]);
        assert_eq!(over.len(), 1);
        assert_eq!(over[0].kind, RecordKind::Fail);
        assert_eq!(client.live_channel_count(), MAX_AGENT_CHANNELS);
        std::fs::remove_file(&sock).ok();
    }

    // --- ForwardPolicy resolution (FDR 0004 Interface precedence table) -----

    fn on(p: &str) -> ForwardPolicy {
        ForwardPolicy::On {
            source: PathBuf::from(p),
        }
    }

    #[test]
    fn policy_default_on_when_auth_sock_present() {
        // No flag, no env: forward the standard agent when one exists.
        let (p, warn) = resolve_forward_policy(&ForwardFlag::Unset, None, Some("/run/agent.sock"));
        assert_eq!(p, on("/run/agent.sock"));
        assert!(warn.is_none());
    }

    #[test]
    fn policy_default_off_silently_when_no_agent() {
        // No flag, no env, no agent: proceed silently without forwarding.
        let (p, warn) = resolve_forward_policy(&ForwardFlag::Unset, None, None);
        assert_eq!(p, ForwardPolicy::Off);
        assert!(warn.is_none(), "the silent default must not warn");
    }

    #[test]
    fn policy_dash_a_disables_even_with_agent_and_env() {
        // -a wins over everything, including an env path.
        let (p, warn) = resolve_forward_policy(
            &ForwardFlag::Disable,
            Some("/env/path.sock"),
            Some("/run/agent.sock"),
        );
        assert_eq!(p, ForwardPolicy::Off);
        assert!(warn.is_none());
    }

    #[test]
    fn policy_explicit_on_warns_loudly_without_agent() {
        // Bare -A with no usable agent (no env path, no $SSH_AUTH_SOCK): stays
        // off AND warns (the distinguishing behavior vs the silent default).
        let (p, warn) = resolve_forward_policy(&ForwardFlag::ExplicitOn, None, None);
        assert_eq!(p, ForwardPolicy::Off);
        assert!(warn.unwrap().contains("-A given but no usable agent"));
        // With $SSH_AUTH_SOCK, -A just enables it, no warning.
        let (p, warn) =
            resolve_forward_policy(&ForwardFlag::ExplicitOn, None, Some("/run/agent.sock"));
        assert_eq!(p, on("/run/agent.sock"));
        assert!(warn.is_none());
    }

    #[test]
    fn policy_explicit_on_resolves_source_through_env_then_default() {
        // -A means "on, loudly"; the SOURCE still resolves env-then-default
        // (flag > env > default). A POSH_FORWARD_AGENT path satisfies -A even
        // with no $SSH_AUTH_SOCK — no warning, forward the env path.
        let (p, warn) =
            resolve_forward_policy(&ForwardFlag::ExplicitOn, Some("/gpg/agent.ssh"), None);
        assert_eq!(p, on("/gpg/agent.ssh"));
        assert!(warn.is_none(), "an env-provided source satisfies -A");
        // An env opt-out (no/0) with no socket is not a usable source, so -A
        // warns and stays off rather than treating "no" as a path.
        let (p, warn) =
            resolve_forward_policy(&ForwardFlag::ExplicitOn, Some("no"), None);
        assert_eq!(p, ForwardPolicy::Off);
        assert!(warn.is_some());
        // But -A overrides the env opt-out (flag > env): with $SSH_AUTH_SOCK
        // present, `-A` + POSH_FORWARD_AGENT=no still forwards the socket.
        let (p, warn) = resolve_forward_policy(
            &ForwardFlag::ExplicitOn,
            Some("no"),
            Some("/run/agent.sock"),
        );
        assert_eq!(p, on("/run/agent.sock"), "-A overrides the env opt-out");
        assert!(warn.is_none());
    }

    #[test]
    fn policy_flag_path_forwards_that_socket() {
        // --forward-agent=PATH ignores $SSH_AUTH_SOCK and the env.
        let (p, warn) = resolve_forward_policy(
            &ForwardFlag::Path(PathBuf::from("/gpg/agent.ssh")),
            Some("no"),
            Some("/run/agent.sock"),
        );
        assert_eq!(p, on("/gpg/agent.ssh"));
        assert!(warn.is_none());
    }

    #[test]
    fn policy_env_no_disables_and_env_path_forwards() {
        // POSH_FORWARD_AGENT=no (or 0) opts out by default.
        for off in ["no", "0"] {
            let (p, _) =
                resolve_forward_policy(&ForwardFlag::Unset, Some(off), Some("/run/agent.sock"));
            assert_eq!(p, ForwardPolicy::Off, "env {off} should disable");
        }
        // Any other env value is a socket path.
        let (p, warn) =
            resolve_forward_policy(&ForwardFlag::Unset, Some("/env/agent.sock"), None);
        assert_eq!(p, on("/env/agent.sock"));
        assert!(warn.is_none());
    }

    #[test]
    fn policy_empty_auth_sock_is_treated_as_unset() {
        // An empty $SSH_AUTH_SOCK is not a usable agent.
        let (p, _) = resolve_forward_policy(&ForwardFlag::Unset, None, Some(""));
        assert_eq!(p, ForwardPolicy::Off);
        let (p, warn) = resolve_forward_policy(&ForwardFlag::ExplicitOn, None, Some(""));
        assert_eq!(p, ForwardPolicy::Off);
        assert!(warn.is_some());
    }

    // --- AgentNotice (per-request agent-use banner, github #96) -------------

    #[test]
    fn notice_fires_on_first_request_naming_host_and_operation() {
        let mut n = AgentNotice::new(false, "box");
        let msg = n
            .on_request(AgentOp::ListKeys, 1_000)
            .expect("the first request notifies");
        assert!(msg.contains("box"), "names the host: {msg}");
        // The operation, not just "the agent was used" — telling a key listing
        // apart from a signature is the whole point of the notice.
        assert!(msg.contains("listed"), "names the operation: {msg}");
        assert!(
            !msg.contains("SIGNED"),
            "a listing must not read as a key use: {msg}"
        );
    }

    #[test]
    fn notice_rate_limited_to_one_per_minute() {
        let mut n = AgentNotice::new(false, "box");
        assert!(n.on_request(AgentOp::ListKeys,0).is_some(), "first fires");
        // Within the window: suppressed.
        assert!(n.on_request(AgentOp::ListKeys,30_000).is_none(), "30s later suppressed");
        assert!(
            n.on_request(AgentOp::ListKeys,59_999).is_none(),
            "just under a minute suppressed"
        );
        // At/after the window: fires again, and the clock advances from there.
        assert!(n.on_request(AgentOp::ListKeys,60_000).is_some(), "a minute later fires");
        assert!(n.on_request(AgentOp::ListKeys,75_000).is_none(), "window restarts");
    }

    // posh#147, the half that was security-relevant rather than merely noisy.
    // The limiter is shared across ALL channel opens and cannot tell them apart,
    // so an open the user does not care about spends the window's single slot and
    // a GENUINE agent use moments later is never announced. Under #147 the
    // spurious open recurred every 5s against a 60s window, so real uses
    // routinely went unreported.
    //
    // Splitting the limits fixes that at the root: a signature is a distinct use
    // of the user's private key and is never rate-limited, so nothing else can
    // consume its slot. This assertion is the exact opposite of what the shared
    // limiter did, which is the point.
    #[test]
    fn a_listing_never_suppresses_a_real_signature() {
        let mut n = AgentNotice::new(false, "box");
        assert!(
            n.on_request(AgentOp::ListKeys, 0).is_some(),
            "the first listing is announced"
        );
        assert!(
            n.on_request(AgentOp::ListKeys, 5_000).is_none(),
            "a second listing inside the window stays rate-limited"
        );
        // ...but a REAL signature moments later is still announced.
        let msg = n
            .on_request(AgentOp::Sign, 5_001)
            .expect("a signature is never suppressed by an unrelated event");
        assert!(msg.contains("SIGNED"), "and it says so plainly: {msg}");
        assert!(msg.contains("box"), "naming the host: {msg}");
        // Signatures are not rate-limited against each other either: every use
        // of a private key is its own event worth reporting.
        assert!(n.on_request(AgentOp::Sign, 5_002).is_some());
        assert!(n.on_request(AgentOp::Sign, 5_003).is_some());
    }

    // An unnamed request type must not be described as a listing. The types posh
    // does not name include ones that MUTATE the local agent (add/remove
    // identity, remove-all, lock), so "listed your keys" would understate a key
    // deletion as a passive read.
    #[test]
    fn an_unrecognised_request_is_not_reported_as_a_listing() {
        let mut n = AgentNotice::new(false, "box");
        let msg = n
            .on_request(AgentOp::Other(19), 0)
            .expect("an unrecognised request is always announced");
        assert!(
            !msg.contains("listed"),
            "must not claim it was a listing: {msg}"
        );
        assert!(msg.contains("19"), "names the type so it can be looked up: {msg}");
        assert!(msg.contains("box"), "names the host: {msg}");
        // Not rate-limited: a possible agent mutation is not something to drop
        // on the floor because a listing happened in the same minute.
        assert!(n.on_request(AgentOp::Other(19), 1).is_some());
    }

    // Per ADR-0003 the 5-byte header may arrive split across records.
    #[test]
    fn op_sniffer_classifies_across_split_reads() {
        // [u32 BE len][type][body…]: len covers the type byte plus 4 body bytes.
        let wire = [0u8, 0, 0, 5, AGENTC_SIGN_REQUEST];
        let mut s = OpSniffer::new();
        for b in &wire[..4] {
            assert!(
                s.push(&[*b]).is_empty(),
                "no verdict before the header completes"
            );
        }
        assert_eq!(s.push(&[wire[4]]), vec![AgentOp::Sign]);
        // The body is skipped, not classified and not retained.
        assert!(s.push(b"body").is_empty());
    }

    #[test]
    fn op_sniffer_distinguishes_listing_from_signing() {
        let classify = |t: u8| OpSniffer::new().push(&[0, 0, 0, 1, t]);
        assert_eq!(classify(AGENTC_REQUEST_IDENTITIES), vec![AgentOp::ListKeys]);
        assert_eq!(classify(AGENTC_SIGN_REQUEST), vec![AgentOp::Sign]);
        // An unrecognised type is reported, not guessed at or dropped.
        assert_eq!(classify(200), vec![AgentOp::Other(200)]);
    }

    // The defect this parser exists to avoid: one agent connection commonly
    // lists the available keys and THEN signs with one. Classifying only the
    // channel's opening request would call that channel a harmless listing and
    // never report the signature — posh#147's "a real key use goes unannounced",
    // reintroduced by another route.
    #[test]
    fn op_sniffer_reports_a_signature_that_follows_a_listing() {
        let mut s = OpSniffer::new();
        // REQUEST_IDENTITIES with no body, then SIGN_REQUEST with a 3-byte body.
        let listing = [0u8, 0, 0, 1, AGENTC_REQUEST_IDENTITIES];
        let signing = [0u8, 0, 0, 4, AGENTC_SIGN_REQUEST, 0xaa, 0xbb, 0xcc];

        assert_eq!(s.push(&listing), vec![AgentOp::ListKeys]);
        assert_eq!(
            s.push(&signing),
            vec![AgentOp::Sign],
            "the signature after a listing must still be classified"
        );

        // And the same stream delivered as ONE record yields both, in order.
        let mut s = OpSniffer::new();
        let mut both = listing.to_vec();
        both.extend_from_slice(&signing);
        assert_eq!(s.push(&both), vec![AgentOp::ListKeys, AgentOp::Sign]);
    }

    // A hostile or corrupt length must not wrap the skip counter or stall the
    // parser: the peer is authenticated, so this is corruption, not an attack to
    // absorb — but it must degrade, never panic.
    #[test]
    fn op_sniffer_tolerates_a_zero_length_request() {
        let mut s = OpSniffer::new();
        assert_eq!(s.push(&[0, 0, 0, 0, AGENTC_SIGN_REQUEST]), vec![AgentOp::Sign]);
        // Zero length saturates to a zero skip, so the next header still parses.
        assert_eq!(
            s.push(&[0, 0, 0, 1, AGENTC_REQUEST_IDENTITIES]),
            vec![AgentOp::ListKeys]
        );
    }

    #[test]
    fn notice_silenced_never_fires() {
        let mut n = AgentNotice::new(true, "box");
        assert!(n.on_request(AgentOp::ListKeys,0).is_none());
        assert!(n.on_request(AgentOp::ListKeys,120_000).is_none(), "still silent past the window");
    }

    #[test]
    fn notice_suppressed_open_does_not_advance_the_clock() {
        // A suppressed in-window call must not consume the rate-limit slot: it
        // leaves last_shown at the first fire, so the next fire is still exactly
        // one window later, not pushed out by the calls between.
        let mut n = AgentNotice::new(false, "box");
        assert!(n.on_request(AgentOp::ListKeys,0).is_some());
        assert!(n.on_request(AgentOp::ListKeys,10_000).is_none());
        assert!(n.on_request(AgentOp::ListKeys,20_000).is_none());
        // 60s after the FIRST fire (not after the last suppressed call) fires.
        assert!(n.on_request(AgentOp::ListKeys,60_000).is_some());
    }
}
