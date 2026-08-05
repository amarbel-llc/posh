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
//! posh#152 interim, layered on that election (throwaway once the mux
//! endpoint — M1 of docs/plans/2026-07-28-connection-mux-endpoint-design.md —
//! makes ownership structural): each endpoint keeps a `srv-<pid>.active`
//! activity marker fresh while its peer is active, notices its peer's
//! activity edges on every `tick` call (not just the slow tick), and on
//! release REPOINTS `agent/sock` at the freshest active sibling instead of
//! unlinking — closing FDR 0014's measured 9.9 s handoff outage.
//!
//! M1 of that mux plan adds a second endpoint NAMING: the agent-only
//! `posh-server agent` remote binds `mux-<client-id>.sock` (+
//! `mux-<client-id>.active`, + a `mux-<client-id>.pid` liveness record — see
//! [`endpoint_pid`]) and participates in the same election as a full sibling
//! of the srv-named endpoints. One election, two name shapes.
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
/// posh#152 interim (throwaway once the mux endpoint — M1 of
/// docs/plans/2026-07-28-connection-mux-endpoint-design.md — lands): how fresh
/// a sibling's `srv-<pid>.active` activity marker must be for
/// repoint-on-release to hand it `agent/sock`. Three slow ticks: an active
/// endpoint refreshes its marker every slow tick, so anything older has missed
/// two refreshes and its "active" claim is not current.
const AGENT_MARKER_FRESH_MS: u64 = 3 * AGENT_SLOW_TICK_MS;
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
    /// The file-name stem this endpoint's socket, marker (and pid file) are
    /// keyed by: `srv-<pid>` for a per-connection session server (FDR 0004,
    /// where the owning pid is derivable from the name itself and what makes
    /// the `gc_dead_sockets` liveness probes meaningful), or
    /// `mux-<client-id>` for the M1 agent-only mux remote
    /// (docs/plans/2026-07-28-mux-endpoint-m1-impl.md), whose deterministic,
    /// respawn-surviving name carries its owning pid in `<stem>.pid` instead
    /// — see [`endpoint_pid`]. A field rather than a re-derivation so tests
    /// can stand up COEXISTING endpoints in one process (see
    /// [`new_with_id`](Self::new_with_id)).
    stem: String,
    /// `<base>/agent/<stem>.sock` — this server's own socket.
    own_sock: PathBuf,
    /// `<base>/agent/<stem>.active` — this endpoint's peer-activity marker
    /// (posh#152 interim): its mtime is refreshed while OUR peer is active, so
    /// a sibling releasing `agent/sock` can repoint it at someone who can
    /// actually serve. Best-effort throughout; throwaway once the mux
    /// endpoint (M1 of docs/plans/2026-07-28-connection-mux-endpoint-design.md)
    /// makes ownership structural.
    own_marker: PathBuf,
    /// `<base>/agent/<stem>.pid` for a mux-named endpoint: the owning pid
    /// the liveness probes (`kill(pid, 0)` in takeover/GC/repoint) read,
    /// since a `mux-<client-id>` name carries none. Written BEFORE the
    /// socket binds and removed AFTER it unlinks, so a mux socket without a
    /// readable pid file is always a crash leftover. `None` for srv-named
    /// endpoints.
    own_pidfile: Option<PathBuf>,
    /// `<base>/agent/sock` — the stable, symlinked `SSH_AUTH_SOCK` target.
    well_known: PathBuf,
    listener: UnixListener,
    channels: Vec<Channel>,
    next_channel_id: u32,
    last_tick: u64,
    /// The `peer_active` value the previous [`tick`](Self::tick) call saw —
    /// the state behind the every-call ACTIVE⇄INACTIVE edge detection
    /// (posh#152 interim). `None` until the first call, so the first call is
    /// itself an edge and settles the link/marker into the right state.
    last_peer_active: Option<bool>,
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

    /// [`from_env`](Self::from_env) for a MUX-NAMED endpoint — the
    /// `posh-server agent` verb's production path (M1 Task 2,
    /// docs/plans/2026-07-28-mux-endpoint-m1-impl.md).
    pub fn from_env_mux(client_id: &str) -> Result<AgentEndpoint> {
        let env = |k: &str| std::env::var(k).ok();
        let uid = util::uid();
        let base = crate::session::resolve_socket_base(
            env("POSH_DIR").as_deref(),
            env("XDG_RUNTIME_DIR").as_deref(),
            env("TMPDIR").as_deref(),
            uid,
        );
        AgentEndpoint::new_mux(&base, client_id)
    }

    /// Builds the endpoint under an explicit base dir (the seam the tests use
    /// with a tempdir), keyed by this process's pid.
    pub fn new(base: &Path) -> Result<AgentEndpoint> {
        AgentEndpoint::build(base, own_pid())
    }

    /// The mux-named endpoint variant (M1, FDR 0014 election): binds
    /// `agent/mux-<client-id>.sock` with marker `mux-<client-id>.active` —
    /// deterministic and respawn-surviving, unlike the pid-keyed srv names —
    /// plus a `mux-<client-id>.pid` liveness record (see the field docs). A
    /// full #152 election sibling: it claims, releases, repoints, and is
    /// repointed-at exactly like an srv endpoint. The id must already be
    /// sanitized (the client's `mux::client_id()` guarantees `[A-Za-z0-9._-]`);
    /// anything else is REJECTED rather than rewritten — the id lands in a
    /// socket file name, and a silent rewrite would desync the name the
    /// election reports from the id the client asked for.
    ///
    /// A same-name collision (a second endpoint for one client id — the
    /// pathological shared-client-id case) gets no new arbitration: a DEAD
    /// recorded owner's leftovers are taken over exactly like a stale srv
    /// socket, and a LIVE owner keeps the name (the bind fails EADDRINUSE).
    pub fn new_mux(base: &Path, client_id: &str) -> Result<AgentEndpoint> {
        let safe = !client_id.is_empty()
            && client_id
                .bytes()
                .all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'));
        if !safe {
            return Err(util::Error::Msg(format!(
                "invalid mux client id ({client_id}): must be non-empty [A-Za-z0-9._-]"
            )));
        }
        AgentEndpoint::build_named(base, format!("mux-{client_id}"), Some(own_pid()))
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

    /// The srv-named constructor behind [`new`](Self::new): keyed by `id`,
    /// which callers MUST pass as a live pid (see [`new_with_id`](Self::new_with_id)).
    fn build(base: &Path, id: i32) -> Result<AgentEndpoint> {
        AgentEndpoint::build_named(base, format!("srv-{id}"), None)
    }

    /// The real constructor: creates `<base>/agent/` 0700, hardens it with
    /// the shared #7 check, binds `<stem>.sock`, and claims `agent/sock`.
    /// `mux_pid` is `Some(owning pid)` for mux-named endpoints — recorded in
    /// `<stem>.pid` BEFORE the bind so a concurrently GC'ing sibling never
    /// sees a live mux socket without its liveness record.
    fn build_named(base: &Path, stem: String, mux_pid: Option<i32>) -> Result<AgentEndpoint> {
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

        let own_sock = dir.join(format!("{stem}.sock"));
        let own_pidfile = mux_pid.map(|_| dir.join(format!("{stem}.pid")));
        match mux_pid {
            // srv names embed the pid, so a same-name leftover is a previous
            // life of OUR pid (pid reuse after an unclean exit) — dead by
            // construction. Clear it, or bind fails with EADDRINUSE.
            None => {
                let _ = std::fs::remove_file(&own_sock);
            }
            // A mux name is deterministic, so the same name may belong to a
            // LIVE sibling (two client hosts sharing a client id — the
            // pathological case). The existing takeover-if-dead gate applies,
            // no new arbitration: clear only a dead (or self-pid-recorded,
            // i.e. previous-life) owner's leftovers; a live foreign owner
            // keeps the name and our bind fails EADDRINUSE below.
            Some(pid) => {
                let dead_or_previous_life = match endpoint_pid(&dir, &stem) {
                    Some(recorded) => recorded == pid || !pid_alive(recorded),
                    None => true, // no liveness record ⇒ crash leftover
                };
                if dead_or_previous_life {
                    let _ = std::fs::remove_file(&own_sock);
                    std::fs::write(
                        own_pidfile.as_ref().expect("mux_pid implies pidfile"),
                        pid.to_string(),
                    )?;
                }
            }
        }
        let listener = UnixListener::bind(&own_sock)?;
        listener.set_nonblocking(true)?;
        // Same reuse hygiene for the activity marker (posh#152): a leftover
        // from a previous life must not advertise us active.
        let own_marker = dir.join(format!("{stem}.active"));
        let _ = std::fs::remove_file(&own_marker);

        let endpoint = AgentEndpoint {
            dir: dir.clone(),
            stem,
            own_sock,
            own_marker,
            own_pidfile,
            well_known: dir.join("sock"),
            listener,
            channels: Vec::new(),
            next_channel_id: 1,
            last_tick: 0,
            last_peer_active: None,
        };
        endpoint.claim_symlink()?;
        Ok(endpoint)
    }

    /// The stable `SSH_AUTH_SOCK` path to export into the session shell (C5).
    pub fn sock_path(&self) -> &Path {
        &self.well_known
    }

    /// Atomically points `agent/sock` at our own `<stem>.sock`.
    fn claim_symlink(&self) -> Result<()> {
        self.point_symlink_at(&format!("{}.sock", self.stem))
    }

    /// Atomically points `agent/sock` at `target` (a dir-relative socket
    /// name): create a uniquely-named temp symlink in the (validated,
    /// private) dir and `rename` it over the well-known name. rename(2) is
    /// atomic, so a concurrent reader never sees a missing or half-written
    /// link. Shared by the self-claim and the posh#152 repoint-on-release.
    fn point_symlink_at(&self, target: &str) -> Result<()> {
        let tmp = self.dir.join(format!(".sock.{}.tmp", self.stem));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(target, &tmp)?;
        std::fs::rename(&tmp, &self.well_known)?;
        Ok(())
    }

    /// Refreshes our `srv-<pid>.active` marker's mtime (posh#152 interim).
    /// A one-byte write is the cheapest portable touch on the ADR 0001
    /// poll/unix-only budget. Best-effort: a failure only degrades sibling
    /// repoint selection, never the endpoint itself.
    fn touch_active_marker(&self) {
        let _ = std::fs::write(&self.own_marker, b"1");
    }

    /// Drops our activity marker (posh#152 interim): called on the
    /// ACTIVE→INACTIVE edge (and at exit) so a sibling's repoint-on-release
    /// stops considering us the moment we can no longer serve. Best-effort.
    fn remove_active_marker(&self) {
        let _ = std::fs::remove_file(&self.own_marker);
    }

    /// The dir-relative socket name of the best repoint target (posh#152
    /// interim): the sibling — srv- or mux-named, the two are full election
    /// peers — with the FRESHEST `.active` marker whose owning pid is alive
    /// (the same `kill(pid, 0)` probe as `gc_dead_sockets` — never a
    /// connect, posh#147), whose marker mtime is within
    /// [`AGENT_MARKER_FRESH_MS`] of the wall clock, and whose `<stem>.sock`
    /// actually exists. Self is skipped by marker-path identity, not pid —
    /// an srv and a mux endpoint in one process share a pid, and a pid
    /// comparison could not tell self from that sibling. `None` when nobody
    /// qualifies.
    fn freshest_active_sibling(&self) -> Option<String> {
        let entries = std::fs::read_dir(&self.dir).ok()?;
        let now = std::time::SystemTime::now();
        let mut best: Option<(std::time::SystemTime, String)> = None;
        for entry in entries.flatten() {
            let path = entry.path();
            if path == self.own_marker {
                continue;
            }
            let Some(stem) = marker_stem(&path) else {
                continue;
            };
            match endpoint_pid(&self.dir, &stem) {
                Some(pid) if pid_alive(pid) => {}
                _ => continue,
            }
            let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
                continue;
            };
            // An mtime in the future (clock step) errors here; treat it as
            // fresh rather than discarding a viable sibling.
            if let Ok(age) = now.duration_since(mtime) {
                if age.as_millis() as u64 > AGENT_MARKER_FRESH_MS {
                    continue;
                }
            }
            if !self.dir.join(format!("{stem}.sock")).exists() {
                continue;
            }
            let fresher = match &best {
                None => true,
                Some((t, _)) => mtime > *t,
            };
            if fresher {
                best = Some((mtime, stem));
            }
        }
        best.map(|(_, stem)| format!("{stem}.sock"))
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
                // Targets are stored relative to `dir` (e.g. "srv-123.sock",
                // "mux-<client-id>.sock").
                let resolved = self.dir.join(&target);
                match sock_stem(&resolved).and_then(|stem| endpoint_pid(&self.dir, &stem)) {
                    // A name we cannot resolve to an owning pid — an
                    // unrecognised target, or a mux socket whose pid file is
                    // gone — is not something we can prove live. Treat it as
                    // takeable rather than deferring to it forever.
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

    /// Give up `agent/sock` if we own it. Called when OUR peer goes inactive:
    /// our `srv-<pid>.sock` is still bound, so `socket_is_dead` reports us
    /// "alive" and no other endpoint would ever take over — starving a sibling
    /// connection whose client IS active (posh#136). We reclaim the link once
    /// our peer is active again (the reclaim edge in [`tick`](Self::tick)).
    ///
    /// posh#152 interim (throwaway once the mux endpoint — M1 of
    /// docs/plans/2026-07-28-connection-mux-endpoint-design.md — makes
    /// ownership structural): prefer HANDING THE LINK OFF over dropping it.
    /// When a qualifying sibling exists (fresh `srv-<pid>.active`, live pid,
    /// bound socket — [`freshest_active_sibling`](Self::freshest_active_sibling))
    /// the link is atomically REPOINTED at that sibling's socket with the same
    /// temp+rename as `claim_symlink`, so the stable path is never stale or
    /// absent across the handoff. Only when nobody qualifies does this fall
    /// back to the plain unlink, which lets the next active endpoint's
    /// `symlink_needs_takeover()` fire (absent ⇒ true).
    ///
    /// The repointed-to sibling never "claimed" the link itself; that is fine
    /// by construction: ownership is always re-derived from `read_link`
    /// (`symlink_points_to_self` / `symlink_needs_takeover`), never cached, so
    /// discovering it owns the link is indistinguishable from having claimed
    /// it.
    fn release_symlink(&self) {
        if !self.symlink_points_to_self() {
            return;
        }
        match self.freshest_active_sibling() {
            Some(target) => {
                let _ = self.point_symlink_at(&target);
            }
            None => {
                let _ = std::fs::remove_file(&self.well_known);
            }
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

    /// Periodic maintenance, gated to `AGENT_SLOW_TICK_MS` — plus the posh#152
    /// interim edge logic, which runs on EVERY call: `server_loop` invokes
    /// tick each iteration, so an ACTIVE⇄INACTIVE flip of our peer is acted on
    /// within one poll wake instead of waiting out the slow tick (the measured
    /// 9.9 s handoff outage of FDR 0014's Limitations). Throwaway once the mux
    /// endpoint (M1 of docs/plans/2026-07-28-connection-mux-endpoint-design.md)
    /// makes ownership structural. Returns any `Close` records produced (e.g.
    /// by the peer-inactive fast-fail) for the caller to forward. `peer_active`
    /// is the loop's existing liveness gate.
    pub fn tick(&mut self, peer_active: bool, now: u64) -> Vec<AgentRecord> {
        // posh#152 edge check, deliberately BEFORE the slow gate. On the
        // ACTIVE→INACTIVE edge the release below repoints `agent/sock` at the
        // freshest active sibling (or unlinks when nobody qualifies); on the
        // →ACTIVE edge we reclaim without the old one-tick claim latency and
        // stand our marker up so siblings' releases can pick us. The initial
        // None→inactive transition is NOT an activity edge: a fresh endpoint
        // keeps its construction-time claim through the startup window where
        // its client has yet to send a datagram, and the slow tick below
        // still releases if the peer never shows up (the pre-#152 behavior).
        let prev = self.last_peer_active;
        self.last_peer_active = Some(peer_active);
        if prev != Some(peer_active) {
            if peer_active {
                self.touch_active_marker();
                if self.symlink_needs_takeover() {
                    let _ = self.claim_symlink();
                }
            } else if prev == Some(true) {
                self.remove_active_marker();
                self.release_symlink();
            }
        }

        if now.saturating_sub(self.last_tick) < AGENT_SLOW_TICK_MS {
            return Vec::new();
        }
        self.last_tick = now;

        if peer_active {
            // Keep the posh#152 activity marker fresh: a releasing sibling
            // judges us serviceable by its mtime staying within
            // AGENT_MARKER_FRESH_MS.
            self.touch_active_marker();
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
            // sibling endpoint whose client IS active can take over (repointed
            // directly at it when its posh#152 marker qualifies, else its
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

    /// Unlinks endpoint files — `srv-*`/`mux-*` sockets, their posh#152
    /// `.active` activity markers, and mux `.pid` liveness records — in
    /// `agent/` whose owning pid is dead. A server unlinks its own files on
    /// exit, so these are crash leftovers.
    fn gc_dead_sockets(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == self.own_sock
                || path == self.own_marker
                || Some(&path) == self.own_pidfile.as_ref()
            {
                continue;
            }
            let Some(stem) = sock_stem(&path)
                .or_else(|| marker_stem(&path))
                .or_else(|| stem_with_suffix(&path, ".pid").filter(|s| s.starts_with("mux-")))
            else {
                continue;
            };
            let dead = match endpoint_pid(&self.dir, &stem) {
                Some(pid) => !pid_alive(pid),
                // An unparseable srv name is not ours to reap (unrelated
                // files are never GC'd); a mux stem with no readable pid
                // file is a crash leftover by the write-before-bind ordering.
                None => stem.starts_with("mux-"),
            };
            if dead {
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
        // The posh#152 activity marker goes with the socket: a dead endpoint
        // must not advertise itself as a repoint target.
        self.remove_active_marker();
        // The mux pid file goes LAST: while the socket exists its liveness
        // record must too (write-before-bind, remove-after-unlink).
        if let Some(pf) = &self.own_pidfile {
            let _ = std::fs::remove_file(pf);
        }
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

    /// Close every live proxied channel NOW, returning the `Close` records to
    /// queue toward the peer so the wire channels terminate too. The FDR 0014
    /// M1 unref-to-zero sweep: when the mux daemon's session refcount reaches
    /// zero, open agent channels must not outlive the last session ref
    /// (RFC 0011 §5's exposure bound, client-enforced). Idempotent.
    pub fn close_all(&mut self) -> Vec<AgentRecord> {
        let mut out = Vec::new();
        for c in &mut self.channels {
            if !c.closed {
                c.closed = true;
                out.push(close_record(c.id));
            }
        }
        reap_closed(&mut self.channels);
        // Match the apply_records sweep: a closed channel leaves no sniffer.
        self.sniffers
            .retain(|(id, _)| self.channels.iter().any(|c| c.id == *id && !c.closed));
        out
    }
}

// ---------------------------------------------------------------------------
// RFC 0011 §5 agent channels: the enveloped wire carriage. On enveloped
// connections each forwarded agent connection is its own `agent`-kind mux
// channel with per-channel cumulative offsets, replacing the retired
// CAP_AGENT_DATA/CAP_AGENT_ACK record stream. The endpoint/proxy machinery
// above is REUSED underneath, untouched: `AgentChannelMux` is a thin adapter
// between its `u32` `AgentRecord` event model and the per-channel
// `AgentPayload` instructions, holding one cumulative outbox/inbox pair per
// channel. Baseline connections keep the CAP_AGENT_* path bit-for-bit.

/// §3.3: how many times a terminal (CLOSE/FAIL) instruction is (re)sent from
/// a live channel before the channel retires to a tombstone. The flag itself
/// has no acknowledgement, so delivery is best-effort-with-retries here plus
/// the tombstone re-answer below (a peer still sending on the identifier is
/// re-answered with the terminal, which closes the 2-generals gap whenever
/// the peer has anything in flight).
const TERM_RETRANSMITS: u32 = 4;

/// §3.3 closed-identifier memory: "a receiver MUST discard instructions on a
/// closed identifier". Identifiers are never reused (§3.1), so tombstones
/// only ever answer stragglers of their own channel; the cap bounds memory
/// on a long-lived connection (oldest first — by then its stragglers are
/// long gone).
const CLOSED_CHANNEL_MEMORY: usize = 256;

/// RFC 0011 §9.2 (RESOLVED 2026-08-05, posh#155): the per-channel
/// exponential-backoff cap on the retransmission interval. A channel whose
/// re-offers go unacked across consecutive RTOs doubles its retx interval
/// per streak step, up to `rto << 3` = 8× (≤8 s at the 1 s RTO clamp).
/// Fresh state (`send_due` — new data, due acks, partial-ack progress) is
/// NEVER delayed by backoff, and any ack progress resets the streak with a
/// prompt resumption, so the cap trades dead-link reprobe latency for flood
/// reduction only. Side effect, accepted: an unacked terminal's
/// TERM_RETRANSMITS quota can stretch to ~15× rto before tombstoning (GC
/// latency, not correctness — the tombstone re-answer still closes the
/// 2-generals gap).
const BACKOFF_SHIFT_MAX: u32 = 3;

/// The §9.2 backed-off retransmission interval for a channel `streak`
/// consecutive unacked RTOs deep. Pure so the pacing tests pin it directly.
fn backed_off(rto: u64, streak: u32) -> u64 {
    rto.saturating_mul(1u64 << streak.min(BACKOFF_SHIFT_MAX))
}

/// RFC 0011 §9.2 (posh#155): the per-connection AIMD bound on aggregate
/// agent data offered per RTO window. `CWND_MAX` is today's implicit bound
/// (every channel's full per-instruction window) — the connection STARTS
/// there and only leaves it when an unacked-data retransmission fires, so an
/// uncongested connection is byte-identical to the pre-§9.2 sender and the
/// clean-path ceiling cannot regress by construction. Multiplicative
/// decrease halves toward `CWND_FLOOR` (one full instruction, so forward
/// progress and the terminal-rides-full-tail rule never deadlock on
/// budget); additive recovery restores one instruction quantum per clean
/// progressed window (floor→max in 7 windows). Learned cwnd survives idle
/// windows rather than resetting.
const CWND_MAX: usize = MAX_AGENT_CHANNELS * AGENT_INSTRUCTION_DATA_MAX;
const CWND_FLOOR: usize = AGENT_INSTRUCTION_DATA_MAX;
const CWND_INCREMENT: usize = AGENT_INSTRUCTION_DATA_MAX;

/// The §9.2 congestion response's rollback switch: `POSH_CONGESTION` is
/// DEFAULT ON (the response is a normative MUST); `=0`/`false`/`off`/`no`
/// restores the pre-§9.2 sender byte-for-byte — the streak never grows and
/// cwnd never leaves max, so backoff and budget are both inert. The same
/// off-switch shape as `POSH_MUX`/`POSH_SESSION_FRAMES`
/// ([`util::parse_default_on_gate`]).
fn parse_congestion_gate(value: Option<&str>) -> bool {
    crate::util::parse_default_on_gate(value)
}

/// Reads the [`parse_congestion_gate`] decision from the environment.
fn congestion_selected() -> bool {
    parse_congestion_gate(std::env::var("POSH_CONGESTION").ok().as_deref())
}

use crate::remote::channel::{
    AgentPayload, ChannelAllocator, ChannelId, Role, AGENT_FLAG_CLOSE, AGENT_FLAG_FAIL,
    AGENT_FLAG_OPEN, AGENT_INSTRUCTION_DATA_MAX, KIND_AGENT, SESSION_CHANNEL,
};
use crate::remote::sync::{InputInbox, InputOutbox};

/// One live mux channel: the identifier ↔ `u32` record-id mapping and the §5
/// per-direction cumulative streams.
struct MuxChannel {
    id: ChannelId,
    /// The `u32` the endpoint/proxy machinery addresses this channel by: on
    /// the server it is the `AgentEndpoint`'s own record id; on the client a
    /// locally-assigned one (the wire never carries it — the mux identifier
    /// replaced it, RFC 0011 §5).
    rec_id: u32,
    outbox: InputOutbox,
    inbox: InputInbox,
    /// §3.3: we opened this channel and no instruction from the peer has
    /// confirmed it yet. Every instruction we send carries FLAG_OPEN until
    /// one does — the peer can only have learned the identifier from an
    /// OPEN-bearing instruction, so its first reply IS the confirmation, and
    /// a duplicate OPEN is by definition a retransmission on its side.
    open_unconfirmed: bool,
    /// Locally-queued terminal flag (AGENT_FLAG_CLOSE or AGENT_FLAG_FAIL).
    term_flag: Option<u8>,
    /// How often the terminal flag has ridden an instruction (TERM_RETRANSMITS).
    term_sends: u32,
    /// Fresh state to emit now (new data / a due ack / the OPEN / a terminal),
    /// as opposed to the RTO-paced retransmission of old state.
    send_due: bool,
    last_send: u64,
    /// §9.2: consecutive retransmissions with no ack progress — the exponent
    /// behind [`backed_off`]. Bumped only by the retx branch (never by
    /// `send_due` emissions); reset to 0 when `outbox.base()` advances or the
    /// peer confirms our OPEN.
    retx_streak: u32,
    /// §9.2: this channel's data was truncated by an exhausted window
    /// budget. Promoted back to `send_due` at the next window roll (refill),
    /// so a budget denial costs at most one window — never a backoff
    /// interval.
    budget_starved: bool,
}

impl MuxChannel {
    fn new(id: ChannelId, rec_id: u32, opener: bool) -> MuxChannel {
        MuxChannel {
            id,
            rec_id,
            outbox: InputOutbox::new(),
            inbox: InputInbox::new(),
            open_unconfirmed: opener,
            term_flag: None,
            term_sends: 0,
            send_due: true,
            last_send: 0,
            retx_streak: 0,
            budget_starved: false,
        }
    }
}

/// §3.3 tombstone for a closed identifier: subsequent instructions are
/// discarded, and a peer still SENDING on the identifier (it missed our
/// terminal) is re-answered with it so it stops.
struct Tombstone {
    id: ChannelId,
    /// The terminal flag this channel closed with (what a re-answer carries).
    flag: u8,
    /// One terminal instruction owed to the peer.
    echo_due: bool,
    /// Final stream offsets, so re-answers stay well-formed §5 payloads.
    final_base: u64,
    final_ack: u64,
}

/// The RFC 0011 §5 adapter between the `u32` `AgentRecord` event model
/// (AgentEndpoint / AgentClient above) and per-channel `agent` instructions:
/// local records queue onto per-channel cumulative outboxes
/// ([`queue_records`](Self::queue_records)), inbound instructions come back
/// out as records ([`on_instruction`](Self::on_instruction)), and
/// [`outgoing`](Self::outgoing) drains what is due — fresh state promptly,
/// unacked tails on the caller's RTO cadence.
pub struct AgentChannelMux {
    role: Role,
    alloc: ChannelAllocator,
    /// Client-side record ids handed to the AgentClient machinery.
    next_rec_id: u32,
    channels: Vec<MuxChannel>,
    closed: Vec<Tombstone>,
    /// §9.2 AIMD state: the aggregate data budget per RTO window. See
    /// [`CWND_MAX`]. `window_used` is the data spent in the current window
    /// (the emission budget's meter); `window_cut`/`window_progress` gate
    /// one MD cut and the AI recovery decision per window.
    cwnd: usize,
    window_start: u64,
    window_used: usize,
    window_cut: bool,
    window_progress: bool,
    /// Telemetry (mux StatusReply): cumulative MD cuts and the deepest
    /// backoff streak observed.
    cuts: u64,
    streak_hwm: u32,
    /// The `POSH_CONGESTION` gate, read once at construction. Off freezes
    /// the streak at 0 (backoff inert) which also keeps cwnd at max
    /// (budget inert): the pre-§9.2 sender byte-for-byte.
    congestion: bool,
}

impl AgentChannelMux {
    /// The opener end (§3.2: `agent` channels are server-initiated).
    pub fn new_server() -> AgentChannelMux {
        AgentChannelMux::new(Role::Server)
    }

    /// The adopter end: channels open on inbound OPEN-flagged instructions.
    pub fn new_client() -> AgentChannelMux {
        AgentChannelMux::new(Role::Client)
    }

    fn new(role: Role) -> AgentChannelMux {
        AgentChannelMux {
            role,
            alloc: ChannelAllocator::new(role),
            next_rec_id: 1,
            channels: Vec::new(),
            closed: Vec::new(),
            cwnd: CWND_MAX,
            window_start: 0,
            window_used: 0,
            window_cut: false,
            window_progress: false,
            cuts: 0,
            streak_hwm: 0,
            congestion: congestion_selected(),
        }
    }

    /// Routes records produced by the LOCAL machinery (the endpoint's
    /// accepts/reads, the proxy's reads and FAIL replies) onto per-channel
    /// outboxes. Only the server end sees `Open` records — an accepted
    /// connection allocates the channel identifier here (§3.1).
    pub fn queue_records(&mut self, records: &[AgentRecord]) {
        for rec in records {
            match rec.kind {
                RecordKind::Open => {
                    debug_assert_eq!(
                        self.role,
                        Role::Server,
                        "only the server opens agent channels (§3.2)"
                    );
                    if self.role != Role::Server || self.by_rec(rec.channel).is_some() {
                        continue;
                    }
                    self.channels
                        .push(MuxChannel::new(self.alloc.next(KIND_AGENT), rec.channel, true));
                }
                RecordKind::Data => {
                    if let Some(ch) = self.by_rec(rec.channel) {
                        ch.outbox.push(&rec.payload);
                        ch.send_due = true;
                    }
                }
                RecordKind::Close | RecordKind::Fail => {
                    if let Some(ch) = self.by_rec(rec.channel) {
                        if ch.term_flag.is_none() {
                            ch.term_flag = Some(match rec.kind {
                                RecordKind::Fail => AGENT_FLAG_FAIL,
                                _ => AGENT_FLAG_CLOSE,
                            });
                            ch.send_due = true;
                        }
                    }
                }
            }
        }
    }

    /// Applies one inbound `agent` instruction (already envelope-validated by
    /// `channel::open_any_instruction`), returning the records to feed the
    /// local machinery. Malformed or unknown-flag payloads are discarded (§5:
    /// ignore, don't guess); instructions on closed identifiers are discarded
    /// with a terminal re-answer when the peer is clearly still sending.
    pub fn on_instruction(&mut self, id: ChannelId, payload: &[u8]) -> Vec<AgentRecord> {
        let Ok(p) = AgentPayload::decode(payload) else {
            return Vec::new();
        };
        if p.has_unknown_flags() {
            return Vec::new();
        }
        let peer_term = p.flags & (AGENT_FLAG_CLOSE | AGENT_FLAG_FAIL) != 0;
        if let Some(t) = self.closed.iter_mut().find(|t| t.id == id) {
            if !peer_term {
                t.echo_due = true;
            }
            return Vec::new();
        }
        let mut out = Vec::new();
        let idx = match self.channels.iter().position(|c| c.id == id) {
            Some(i) => i,
            None => {
                // §3.3: the first instruction on a not-yet-seen identifier
                // from the peer's space opens the channel — but only toward
                // the adopter end, and only OPEN-flagged (the opener keeps
                // FLAG_OPEN on everything until confirmed, so a bare data
                // instruction on an unknown identifier is a post-close
                // straggler, not an open).
                if self.role != Role::Client || p.flags & AGENT_FLAG_OPEN == 0 {
                    return Vec::new();
                }
                // §3.4: MAX_AGENT_CHANNELS is the `agent` kind's
                // per-connection bound — refuse with FAIL, never allocate
                // past it. (The AgentClient enforces the same bound on its
                // socket table; refusing here keeps the wire object bounded
                // even before the records reach it.)
                if self.channels.len() >= MAX_AGENT_CHANNELS {
                    self.tombstone(id, AGENT_FLAG_FAIL, 0, 0, true);
                    return Vec::new();
                }
                let rec_id = self.next_rec_id;
                self.next_rec_id += 1;
                self.channels.push(MuxChannel::new(id, rec_id, false));
                out.push(AgentRecord {
                    channel: rec_id,
                    kind: RecordKind::Open,
                    payload: Vec::new(),
                });
                self.channels.len() - 1
            }
        };
        let ch = &mut self.channels[idx];
        // Any instruction from the peer on this identifier proves our OPEN
        // arrived (§3.3): it could only know the identifier from it — which
        // also ends any OPEN-retx backoff streak (§9.2).
        if ch.open_unconfirmed {
            ch.retx_streak = 0;
        }
        ch.open_unconfirmed = false;
        let base_before = ch.outbox.base();
        ch.outbox.ack(p.recv_ack);
        if ch.outbox.base() > base_before {
            // Ack progress: the peer is receiving — the §9.2 backoff streak
            // ends and re-offers resume at the plain RTO cadence, and the
            // AIMD window counts as progressed (additive recovery at roll).
            ch.retx_streak = 0;
            self.window_progress = true;
        }
        if !ch.outbox.is_empty() {
            // A partially-acked tail (e.g. past the §4.1 per-instruction
            // bound): keep pumping promptly rather than waiting out the RTO.
            ch.send_due = true;
        }
        if let Some(fresh) = ch.inbox.accept(p.send_base, &p.data) {
            out.push(AgentRecord {
                channel: ch.rec_id,
                kind: RecordKind::Data,
                payload: fresh.to_vec(),
            });
            ch.send_due = true; // ack the delivery promptly
        }
        if peer_term {
            // §5: CLOSE and FAIL are terminal — surface to the local socket,
            // discard the channel, and owe the peer one terminal echo (its
            // own terminal retransmits until something confirms).
            out.push(AgentRecord {
                channel: ch.rec_id,
                kind: if p.flags & AGENT_FLAG_FAIL != 0 {
                    RecordKind::Fail
                } else {
                    RecordKind::Close
                },
                payload: Vec::new(),
            });
            let flag = ch.term_flag.unwrap_or(AGENT_FLAG_CLOSE);
            let (fb, fa) = (ch.outbox.end_offset(), ch.inbox.next_offset());
            self.channels.remove(idx);
            self.tombstone(id, flag, fb, fa, true);
        }
        out
    }

    /// Drains every instruction due now: fresh state (`send_due`) at once,
    /// unacked tails / unconfirmed OPENs / unsettled terminals on the
    /// caller's RTO cadence. Each instruction carries at most
    /// [`AGENT_INSTRUCTION_DATA_MAX`] data bytes (§4.1); a terminal flag
    /// rides only an instruction carrying the entire remaining tail, so the
    /// receiver never closes the socket with bytes still owed.
    pub fn outgoing(&mut self, now: u64, rto: u64) -> Vec<(ChannelId, Vec<u8>)> {
        // §9.2 window roll: settle the previous RTO window's AIMD decision
        // (additive recovery iff progressed and uncut) and refill the data
        // budget. Idle windows (no progress, no cut) change nothing — the
        // learned cwnd survives idle rather than resetting.
        if now.saturating_sub(self.window_start) >= rto {
            if !self.window_cut && self.window_progress {
                self.cwnd = (self.cwnd + CWND_INCREMENT).min(CWND_MAX);
            }
            self.window_cut = false;
            self.window_progress = false;
            self.window_used = 0;
            self.window_start = now;
            // Refill re-arms budget-starved channels promptly.
            for ch in &mut self.channels {
                if std::mem::take(&mut ch.budget_starved) {
                    ch.send_due = true;
                }
            }
        }
        // Fairness under a binding budget: rotate the service order one
        // step per drain so a reduced cwnd is shared across channels
        // instead of pinning to the head. Channel order carries no
        // protocol semantics (lookups are id-keyed).
        if self.channels.len() > 1 {
            self.channels.rotate_left(1);
        }
        let mut out = Vec::new();
        for t in &mut self.closed {
            if std::mem::take(&mut t.echo_due) {
                out.push((
                    t.id,
                    AgentPayload {
                        flags: t.flag,
                        send_base: t.final_base,
                        recv_ack: t.final_ack,
                        data: Vec::new(),
                    }
                    .encode(),
                ));
            }
        }
        let mut i = 0;
        while i < self.channels.len() {
            let ch = &mut self.channels[i];
            let unacked = !ch.outbox.is_empty();
            let term_unsettled = ch.term_flag.is_some() && ch.term_sends < TERM_RETRANSMITS;
            let wants_retx = unacked || ch.open_unconfirmed || term_unsettled;
            // §9.2: retransmissions pace on the backed-off interval; fresh
            // state (`send_due`) is never delayed by it.
            let retx_fires =
                wants_retx && now.saturating_sub(ch.last_send) >= backed_off(rto, ch.retx_streak);
            if retx_fires && !ch.send_due && self.congestion {
                ch.retx_streak = ch.retx_streak.saturating_add(1);
                self.streak_hwm = self.streak_hwm.max(ch.retx_streak);
                // MD: an unacked-DATA retransmission is the congestion
                // signal — halve the window budget, at most once per
                // window, immediately (not at settle). OPEN/terminal
                // retransmissions never cut: a lost handshake instruction
                // is a poor congestion signal (they still back off above).
                if unacked && !self.window_cut {
                    self.window_cut = true;
                    self.cwnd = (self.cwnd / 2).max(CWND_FLOOR);
                    self.cuts += 1;
                }
            }
            if ch.send_due || retx_fires {
                let pending = ch.outbox.pending();
                let desired = pending.len().min(AGENT_INSTRUCTION_DATA_MAX);
                // §9.2: the window budget gates DATA BYTES only, and only
                // while cwnd sits below max — at max, emission is
                // byte-identical to the ungated sender. A truncated (even
                // to zero) instruction still goes out: acks, OPENs, and
                // terminal echoes are never budget-blocked.
                let take = if self.cwnd < CWND_MAX {
                    let t = desired.min(self.cwnd.saturating_sub(self.window_used));
                    if t < desired {
                        ch.budget_starved = true;
                    }
                    t
                } else {
                    desired
                };
                self.window_used += take;
                let mut flags = 0u8;
                if ch.open_unconfirmed {
                    flags |= AGENT_FLAG_OPEN;
                }
                if let Some(f) = ch.term_flag {
                    if take == pending.len() {
                        flags |= f;
                        ch.term_sends += 1;
                    }
                }
                out.push((
                    ch.id,
                    AgentPayload {
                        flags,
                        send_base: ch.outbox.base(),
                        recv_ack: ch.inbox.next_offset(),
                        data: pending[..take].to_vec(),
                    }
                    .encode(),
                ));
                ch.send_due = false;
                ch.last_send = now;
            }
            // Terminal settled (all data acked, the flag sent its quota):
            // retire to a tombstone. No echo owed — the close was ours.
            if let Some(flag) = ch.term_flag {
                if ch.outbox.is_empty() && ch.term_sends >= TERM_RETRANSMITS {
                    let id = ch.id;
                    let (fb, fa) = (ch.outbox.end_offset(), ch.inbox.next_offset());
                    self.channels.remove(i);
                    self.tombstone(id, flag, fb, fa, false);
                    continue;
                }
            }
            i += 1;
        }
        out
    }

    /// Earliest moment [`outgoing`](Self::outgoing) would emit — the loops
    /// fold this into their poll deadline so retransmissions fire without fd
    /// activity. `None` when fully idle.
    pub fn next_deadline(&self, rto: u64) -> Option<u64> {
        let mut due: Option<u64> = None;
        let mut fold = |t: u64| due = Some(due.map_or(t, |d| d.min(t)));
        if self.closed.iter().any(|t| t.echo_due) {
            fold(0);
        }
        for ch in &self.channels {
            if ch.send_due {
                fold(0);
                continue;
            }
            if ch.budget_starved {
                // §9.2: a starved channel resumes at the window REFILL —
                // never 0 (that would busy-spin the poll loop against an
                // exhausted budget) and never the backoff interval (a
                // budget denial is not a loss signal).
                fold(self.window_start + rto);
                continue;
            }
            let wants = !ch.outbox.is_empty()
                || ch.open_unconfirmed
                || (ch.term_flag.is_some() && ch.term_sends < TERM_RETRANSMITS);
            if wants {
                // §9.2: the poll deadline mirrors the backed-off retx
                // interval, or retransmissions would fire early off the
                // caller's wakeup.
                fold(ch.last_send + backed_off(rto, ch.retx_streak));
            }
        }
        due
    }

    /// §9.2 telemetry for the mux `StatusReply` one-liner: the live window
    /// budget, cumulative MD cuts, and the deepest backoff streak observed.
    pub fn congestion_summary(&self) -> (usize, u64, u32) {
        (self.cwnd, self.cuts, self.streak_hwm)
    }

    fn by_rec(&mut self, rec_id: u32) -> Option<&mut MuxChannel> {
        self.channels.iter_mut().find(|c| c.rec_id == rec_id)
    }

    fn tombstone(&mut self, id: ChannelId, flag: u8, final_base: u64, final_ack: u64, echo_due: bool) {
        if self.closed.len() >= CLOSED_CHANNEL_MEMORY {
            self.closed.remove(0);
        }
        self.closed.push(Tombstone {
            id,
            flag,
            echo_due,
            final_base,
            final_ack,
        });
    }
}

/// One poll iteration's enveloped sends in RFC 0011 §4.1 order: the pending
/// `session` instruction (if any) precedes bulk `agent` data, so a keystroke
/// frame never waits behind an agent burst. Payloads are unsealed; the
/// caller wraps each in the §2 envelope for its channel and fragments.
pub fn iteration_sends(
    session: Option<Vec<u8>>,
    mux: Option<&mut AgentChannelMux>,
    now: u64,
    rto: u64,
) -> Vec<(ChannelId, Vec<u8>)> {
    let mut out = Vec::new();
    if let Some(s) = session {
        out.push((SESSION_CHANNEL, s));
    }
    if let Some(m) = mux {
        out.extend(m.outgoing(now, rto));
    }
    out
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

/// The endpoint stem (`srv-<pid>` / `mux-<client-id>`) behind an
/// endpoint-owned file name with the given suffix, or `None` for anything
/// else in `agent/` (so unrelated files are never GC'd or trusted).
fn stem_with_suffix(path: &Path, suffix: &str) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(suffix)?;
    (stem.starts_with("srv-") || stem.starts_with("mux-")).then(|| stem.to_string())
}

/// [`stem_with_suffix`] for the socket files themselves.
fn sock_stem(path: &Path) -> Option<String> {
    stem_with_suffix(path, ".sock")
}

/// [`stem_with_suffix`] for the posh#152 `.active` activity markers.
fn marker_stem(path: &Path) -> Option<String> {
    stem_with_suffix(path, ".active")
}

/// The liveness pid of the endpoint owning `stem`: parsed from the name for
/// `srv-<pid>`, read from `<stem>.pid` for `mux-<client-id>` — either way
/// the #152 takeover/GC/repoint probes reduce to the same `kill(pid, 0)`.
/// `None` when undeterminable: an unparseable srv name, or a mux stem whose
/// pid file is missing or garbled (which, by the write-before-bind /
/// remove-after-unlink ordering, marks a crash leftover).
fn endpoint_pid(dir: &Path, stem: &str) -> Option<i32> {
    if let Some(pid) = stem.strip_prefix("srv-") {
        return pid.parse().ok();
    }
    std::fs::read_to_string(dir.join(format!("{stem}.pid")))
        .ok()?
        .trim()
        .parse()
        .ok()
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

    /// Sets a file's mtime `ago_ms` (plus a second of slack) into the past, so
    /// a test can construct a STALE activity marker without waiting real time
    /// (marker freshness is judged against the wall clock, not the loops'
    /// virtual `now`).
    fn set_mtime_ago(path: &Path, ago_ms: u64) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let then = now.as_secs() - ago_ms.div_ceil(1000) - 1;
        let tv = libc::timeval {
            tv_sec: then as libc::time_t,
            tv_usec: 0,
        };
        let times = [tv, tv];
        // SAFETY: utimes(2) with a valid NUL-terminated path and a 2-element
        // timeval array; touches no memory beyond its arguments.
        let rc = unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes failed: {}", std::io::Error::last_os_error());
    }

    // posh#136 / posh#152 / FDR 0014: the interim repoint-on-release. The
    // previous interim (relinquish-on-inactive alone) left a measured ~9.9 s
    // outage per handoff — two independent slow ticks: the owner released on
    // ITS next tick (the link STALE, fast-failing requests, until then), and
    // the active sibling claimed on ITS next tick after that (the link ABSENT
    // in between). Pinned by this test's predecessor,
    // `handoff_between_two_endpoints_leaves_a_multi_tick_outage`, whose bound
    // this supersedes. Two mechanics close it:
    //
    //   - activity markers: an endpoint with an ACTIVE peer keeps a fresh
    //     `srv-<pid>.active` beside its socket, so a releasing owner can see
    //     which siblings could actually serve;
    //   - event-driven release: the owner notices its peer's ACTIVE→INACTIVE
    //     edge on the very next `tick` CALL (the slow gate applies only to
    //     maintenance), and instead of unlinking, atomically REPOINTS
    //     `agent/sock` at the freshest active sibling.
    //
    // The honest bound the mechanics give: the repoint happens synchronously
    // inside the owner's first `tick` call after the edge, and the temp+rename
    // is atomic — so measured at ANY granularity after that call the link is
    // neither stale nor absent (asserted as exactly 0 ms below). In production
    // the residual is `server_loop`'s latency in making that call (at most one
    // poll wake, which the heartbeat cadence bounds), not a slow tick.
    #[test]
    fn handoff_repoints_to_the_active_sibling_on_the_inactivity_edge() {
        const STEP_MS: u64 = 100;
        let base = temp_base();
        let agent_dir = base.join("agent");

        // Two coexisting endpoints under one base. `a` is the sibling whose
        // client stays ACTIVE; `b` is the newest connection, so it owns the
        // link. `b`'s id is pid 1, which is always a live process — otherwise
        // `a`'s `gc_dead_sockets` would reap `b`'s socket and confound the
        // measurement.
        let mut a = AgentEndpoint::new_with_id(&base, own_pid()).unwrap();
        let mut b = AgentEndpoint::new_with_id(&base, 1).unwrap();
        let a_target = PathBuf::from(format!("srv-{}.sock", own_pid()));
        let b_target = PathBuf::from("srv-1.sock");
        assert_eq!(
            std::fs::read_link(a.sock_path()).unwrap(),
            b_target,
            "the newest endpoint owns agent/sock"
        );

        // Both endpoints last ticked at t=0; the clock advances from there.
        a.last_tick = 0;
        b.last_tick = 0;

        // Warm-up: both peers active across two slow ticks. Both endpoints
        // stand up (and refresh) their activity markers; b keeps the link.
        let mut t = 0;
        while t < AGENT_SLOW_TICK_MS * 2 {
            t += STEP_MS;
            a.tick(true, t);
            b.tick(true, t);
        }
        assert!(
            agent_dir.join(format!("srv-{}.active", own_pid())).exists(),
            "an active endpoint maintains its srv-<pid>.active marker (posh#152)"
        );
        assert!(agent_dir.join("srv-1.active").exists());
        assert_eq!(std::fs::read_link(agent_dir.join("sock")).unwrap(), b_target);

        // b's peer roams away. The flip lands mid-slow-tick (b's gate is not
        // due for another ~AGENT_SLOW_TICK_MS), so any handoff observed below
        // is the EDGE logic, not tick-paced maintenance.
        let flip = t;
        let (mut stale_ms, mut absent_ms, mut served_ms) = (0u64, 0u64, 0u64);
        while t < flip + AGENT_SLOW_TICK_MS * 2 {
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
            "posh#152 handoff: stale={stale_ms}ms absent={absent_ms}ms \
             (unusable={}ms) served={served_ms}ms after the edge",
            stale_ms + absent_ms
        );

        assert_eq!(
            stale_ms, 0,
            "the owner hands the link off within the tick call that sees the \
             inactivity edge — it never lingers on the fast-failing owner"
        );
        assert_eq!(
            absent_ms, 0,
            "the handoff is an atomic repoint, never an unlink-then-reclaim — \
             agent/sock never goes absent while a qualifying sibling exists"
        );
        assert_eq!(
            served_ms,
            AGENT_SLOW_TICK_MS * 2,
            "the active sibling serves for the entire post-edge window"
        );
        assert_eq!(
            std::fs::read_link(agent_dir.join("sock")).unwrap(),
            a_target,
            "and the link points at the active sibling's socket"
        );
        assert!(
            !agent_dir.join("srv-1.active").exists(),
            "the roamed-away owner drops its own marker on the edge, so \
             siblings' releases stop considering it"
        );

        drop(b);
        drop(a);
        std::fs::remove_dir_all(&base).ok();
    }

    // posh#152: a release with NO qualifying sibling falls back to today's
    // plain unlink — a dead sibling pid disqualifies its marker even when the
    // marker is fresh and its socket file is present. Also pins that the
    // release is EVENT-driven: both tick calls land far inside the slow gate
    // (now=1, 2), so only the ACTIVE→INACTIVE edge logic can have released.
    #[test]
    fn release_with_no_qualifying_sibling_falls_back_to_unlink() {
        let base = temp_base();
        let agent_dir = base.join("agent");
        let mut ep = AgentEndpoint::new_with_id(&base, 1).unwrap();
        assert!(ep.symlink_points_to_self(), "fresh endpoint owns agent/sock");

        // A dead "sibling": fresh marker + socket file, but pid 999999 is not
        // a live process, so it must not be chosen.
        std::fs::write(agent_dir.join("srv-999999.active"), b"1").unwrap();
        std::fs::write(agent_dir.join("srv-999999.sock"), b"").unwrap();

        ep.tick(true, 1); // peer attaches…
        ep.tick(false, 2); // …then roams away: the edge, mid-slow-tick
        assert!(
            std::fs::symlink_metadata(ep.sock_path()).is_err(),
            "no qualifying sibling: the inactivity edge unlinks agent/sock \
             (the pre-posh#152 fallback), rather than repointing at a dead pid"
        );

        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    // posh#152: the initial None→inactive transition is NOT an activity edge.
    // A fresh endpoint keeps its construction-time claim through the startup
    // window where its client has yet to send a datagram (the relay/server
    // loops tick with peer_active=false until the first datagram arrives);
    // only the slow tick may release it, exactly as before posh#152.
    #[test]
    fn startup_inactive_ticks_keep_the_construction_claim() {
        let base = temp_base();
        let mut ep = AgentEndpoint::new_with_id(&base, 1).unwrap();
        ep.last_tick = 0;
        // Fast (gated) ticks before any client has ever been heard from.
        for t in 1..10 {
            ep.tick(false, t);
        }
        assert!(
            ep.symlink_points_to_self(),
            "a never-yet-active endpoint holds its claim until the slow tick"
        );
        // The slow tick still applies the pre-#152 inactive release.
        ep.tick(false, AGENT_SLOW_TICK_MS + 1);
        assert!(
            std::fs::symlink_metadata(ep.sock_path()).is_err(),
            "the slow tick releases a peer that never appeared"
        );
        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    // posh#152: a sibling whose activity marker has gone STALE (older than
    // AGENT_MARKER_FRESH_MS) is not chosen for the repoint, even though its
    // process is alive and its socket is bound — a marker that stopped being
    // refreshed means that endpoint's peer went quiet too.
    #[test]
    fn a_stale_sibling_marker_is_not_chosen_for_repoint() {
        let base = temp_base();
        let agent_dir = base.join("agent");
        // A live sibling with a bound socket…
        let a = AgentEndpoint::new_with_id(&base, own_pid()).unwrap();
        let mut b = AgentEndpoint::new_with_id(&base, 1).unwrap();
        // …whose marker exists but is well past the freshness window.
        let marker = agent_dir.join(format!("srv-{}.active", own_pid()));
        std::fs::write(&marker, b"1").unwrap();
        set_mtime_ago(&marker, AGENT_MARKER_FRESH_MS + AGENT_SLOW_TICK_MS);

        b.tick(true, 1); // b's peer attaches…
        b.tick(false, 2); // …then roams away: the edge, mid-slow-tick
        assert!(
            std::fs::symlink_metadata(b.sock_path()).is_err(),
            "a stale marker must not attract the repoint — the release falls \
             back to the unlink so takeover liveness rules stay in charge"
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
    fn endpoint_stems_and_pids_parse_only_matching_names() {
        let base = temp_base();
        let dir = base.join("agent");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(sock_stem(Path::new("/x/srv-123.sock")).as_deref(), Some("srv-123"));
        assert_eq!(sock_stem(Path::new("/x/mux-host.sock")).as_deref(), Some("mux-host"));
        assert_eq!(sock_stem(Path::new("/x/sock")), None);
        assert_eq!(sock_stem(Path::new("/x/other.sock")), None);
        assert_eq!(endpoint_pid(&dir, "srv-123"), Some(123));
        assert_eq!(endpoint_pid(&dir, "srv-abc"), None);
        // A mux stem's pid lives in its `<stem>.pid` file; absent ⇒ None.
        assert_eq!(endpoint_pid(&dir, "mux-host"), None);
        std::fs::write(dir.join("mux-host.pid"), b"321").unwrap();
        assert_eq!(endpoint_pid(&dir, "mux-host"), Some(321));
        std::fs::remove_dir_all(&base).ok();
    }

    // --- mux-named endpoints (M1 Task 2, docs/plans/2026-07-28-mux-endpoint-m1-impl.md) ---

    #[test]
    fn mux_endpoint_binds_named_socket_marker_and_pidfile() {
        let base = temp_base();
        let dir = base.join("agent");
        let ep = AgentEndpoint::new_mux(&base, "clienthost").unwrap();
        // The socket + the #152 election files are keyed by the client id.
        assert_eq!(ep.own_sock, dir.join("mux-clienthost.sock"));
        assert!(ep.own_sock.exists());
        assert_eq!(
            std::fs::read_link(ep.sock_path()).unwrap().to_str().unwrap(),
            "mux-clienthost.sock",
            "a mux endpoint claims agent/sock at construction like any sibling"
        );
        // The owning pid is discoverable beside the socket: a mux name
        // carries no pid, and the takeover/GC/repoint probes all reduce to
        // kill(pid, 0).
        assert_eq!(
            std::fs::read_to_string(dir.join("mux-clienthost.pid")).unwrap(),
            own_pid().to_string()
        );
        drop(ep);
        assert!(!dir.join("mux-clienthost.sock").exists());
        assert!(!dir.join("mux-clienthost.pid").exists());
        assert!(std::fs::symlink_metadata(dir.join("sock")).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn mux_endpoint_rejects_unsafe_client_id() {
        // The id lands in a socket file name: refuse anything outside the
        // sanitized [A-Za-z0-9._-] set the client promises (mux::client_id),
        // rather than silently rewriting it.
        let base = temp_base();
        assert!(AgentEndpoint::new_mux(&base, "").is_err());
        assert!(AgentEndpoint::new_mux(&base, "a/b").is_err());
        assert!(AgentEndpoint::new_mux(&base, "a b").is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn gc_reaps_dead_mux_endpoint_files() {
        let base = temp_base();
        let ep = AgentEndpoint::new(&base).unwrap();
        let dir = base.join("agent");
        // A crashed mux endpoint: socket + marker + a pid file naming a dead
        // pid — all three are leftovers.
        std::fs::write(dir.join("mux-dead.sock"), b"").unwrap();
        std::fs::write(dir.join("mux-dead.active"), b"1").unwrap();
        std::fs::write(dir.join("mux-dead.pid"), b"999999").unwrap();
        // A mux socket with NO pid file is unprovably live (the pid file is
        // written before the bind): a crash leftover, reaped too.
        std::fs::write(dir.join("mux-orphan.sock"), b"").unwrap();
        // A LIVE mux sibling survives.
        std::fs::write(dir.join("mux-live.sock"), b"").unwrap();
        std::fs::write(dir.join("mux-live.pid"), own_pid().to_string()).unwrap();
        ep.gc_dead_sockets();
        assert!(!dir.join("mux-dead.sock").exists());
        assert!(!dir.join("mux-dead.active").exists());
        assert!(!dir.join("mux-dead.pid").exists());
        assert!(!dir.join("mux-orphan.sock").exists());
        assert!(dir.join("mux-live.sock").exists(), "a live mux sibling is not reaped");
        assert!(dir.join("mux-live.pid").exists());
        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn takeover_judges_mux_link_target_by_pidfile_liveness() {
        let base = temp_base();
        let ep = AgentEndpoint::new(&base).unwrap();
        let dir = base.join("agent");
        // agent/sock points at a mux sibling whose recorded pid is alive.
        std::fs::write(dir.join("mux-x.sock"), b"").unwrap();
        std::fs::write(dir.join("mux-x.pid"), own_pid().to_string()).unwrap();
        let _ = std::fs::remove_file(dir.join("sock"));
        std::os::unix::fs::symlink("mux-x.sock", dir.join("sock")).unwrap();
        assert!(
            !ep.symlink_needs_takeover(),
            "a live mux owner is not taken over from"
        );
        // A dead recorded pid makes it takeable...
        std::fs::write(dir.join("mux-x.pid"), b"999999").unwrap();
        assert!(ep.symlink_needs_takeover());
        // ...and so does a missing pid file (unprovably live).
        std::fs::remove_file(dir.join("mux-x.pid")).unwrap();
        assert!(ep.symlink_needs_takeover());
        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    // M1 Task 2 (docs/plans/2026-07-28-mux-endpoint-m1-impl.md): the
    // agent-only remote's mux-named endpoint is a FULL sibling in the #152
    // election — it claims `agent/sock`, hands it to an srv-named sibling on
    // its own peer's inactivity edge, and receives it back on the reverse
    // edge. Same tick-driven virtual-time pattern as
    // `handoff_repoints_to_the_active_sibling_on_the_inactivity_edge`.
    #[test]
    fn agent_only_server_claims_and_repoints_like_a_sibling() {
        let base = temp_base();
        let agent_dir = base.join("agent");
        // The srv sibling runs on pid 1 (always live, the new_with_id
        // convention); the mux endpoint records our own pid in mux-cid.pid.
        let mut srv = AgentEndpoint::new_with_id(&base, 1).unwrap();
        let mut mux = AgentEndpoint::new_mux(&base, "cid").unwrap();
        srv.last_tick = 0;
        mux.last_tick = 0;
        assert_eq!(
            std::fs::read_link(agent_dir.join("sock")).unwrap().to_str().unwrap(),
            "mux-cid.sock",
            "the mux endpoint claims agent/sock at construction (newest wins)"
        );

        // Warm-up: both peers active across two slow ticks — both markers up.
        let mut t = 0;
        while t < AGENT_SLOW_TICK_MS * 2 {
            t += 100;
            srv.tick(true, t);
            mux.tick(true, t);
        }
        assert!(agent_dir.join("mux-cid.active").exists());
        assert!(agent_dir.join("srv-1.active").exists());

        // The mux endpoint's peer roams away: the inactivity edge repoints
        // agent/sock at the srv sibling — never stale, never absent.
        t += 100;
        srv.tick(true, t);
        mux.tick(false, t);
        assert_eq!(
            std::fs::read_link(agent_dir.join("sock")).unwrap().to_str().unwrap(),
            "srv-1.sock",
            "mux → srv: the inactivity edge hands the link to the srv sibling"
        );

        // And back: the mux peer returns (its marker stands up), then the
        // srv peer roams away — its edge repoints at the mux sibling.
        t += 100;
        mux.tick(true, t);
        srv.tick(false, t);
        assert_eq!(
            std::fs::read_link(agent_dir.join("sock")).unwrap().to_str().unwrap(),
            "mux-cid.sock",
            "srv → mux: the reverse edge hands the link back"
        );

        drop(mux);
        drop(srv);
        std::fs::remove_dir_all(&base).ok();
    }

    // RFC 0011 §8 / FDR 0014 (the two-client-host policy, ratified
    // 2026-07-28): the same remote account reached from TWO client hosts is
    // two mux-named endpoints with distinct client ids sharing one agent
    // dir — and `agent/sock` resolves by the most-recently-active election
    // on the #152 marker machinery. What makes this election tractable
    // where the per-connection one was not (posh#136): participants are one
    // per client host, deterministically named, and each endpoint knows its
    // own peer's activity edges authoritatively. Same tick-driven
    // virtual-time harness as
    // `handoff_repoints_to_the_active_sibling_on_the_inactivity_edge`.
    #[test]
    fn two_mux_endpoints_elect_the_most_recently_active_client_host() {
        let base = temp_base();
        let agent_dir = base.join("agent");
        // Two client hosts: "hosta" and "hostb". Both endpoints live in this
        // process (one pid, two pidfiles); self-skip in the election is by
        // marker-path identity, so they are full siblings to each other.
        let mut a = AgentEndpoint::new_mux(&base, "hosta").unwrap();
        let mut b = AgentEndpoint::new_mux(&base, "hostb").unwrap();
        a.last_tick = 0;
        b.last_tick = 0;
        assert_eq!(
            std::fs::read_link(agent_dir.join("sock")).unwrap().to_str().unwrap(),
            "mux-hostb.sock",
            "construction order seeds ownership (newest wins), as today"
        );

        // Warm-up: both hosts' peers active across two slow ticks — both
        // per-client-host sockets bound, both markers fresh.
        let mut t = 0;
        while t < AGENT_SLOW_TICK_MS * 2 {
            t += 100;
            a.tick(true, t);
            b.tick(true, t);
        }
        assert!(agent_dir.join("mux-hosta.active").exists());
        assert!(agent_dir.join("mux-hostb.active").exists());

        // The user walks away from host b: its endpoint sees the inactivity
        // edge and hands `agent/sock` to the most recently active sibling —
        // host a — atomically (never stale, never absent; the agent that
        // answers is the one nearest the user's attention).
        t += 100;
        a.tick(true, t);
        b.tick(false, t);
        assert_eq!(
            std::fs::read_link(agent_dir.join("sock")).unwrap().to_str().unwrap(),
            "mux-hosta.sock",
            "hostb inactive: the election repoints at the active hosta"
        );

        // And walks back to host b: hosta's edge returns the link.
        t += 100;
        b.tick(true, t);
        a.tick(false, t);
        assert_eq!(
            std::fs::read_link(agent_dir.join("sock")).unwrap().to_str().unwrap(),
            "mux-hostb.sock",
            "hosta inactive: the election repoints back at hostb"
        );

        drop(b);
        drop(a);
        std::fs::remove_dir_all(&base).ok();
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
    fn client_close_all_returns_close_records_and_drops_channels() {
        // The FDR 0014 M1 unref-to-zero sweep: refs hit 0 => every proxied
        // channel closes NOW, and the Close records go back to the peer so
        // the wire channels terminate too (RFC 0011 §5).
        let sock = temp_sock();
        std::fs::remove_file(&sock).ok();
        let _listener = UnixListener::bind(&sock).unwrap();
        let mut client = AgentClient::new(sock.clone());
        client.apply_records(&[
            rec_open(1),
            rec_open(2),
        ]);
        assert_eq!(client.live_channel_count(), 2);
        let mut closes = client.close_all();
        closes.sort_by_key(|r| r.channel);
        assert_eq!(closes.len(), 2);
        for (r, want) in closes.iter().zip([1u32, 2]) {
            assert_eq!(r.kind, RecordKind::Close);
            assert_eq!(r.channel, want);
        }
        assert_eq!(client.live_channel_count(), 0);
        // Idempotent: nothing left to close.
        assert!(client.close_all().is_empty());
        std::fs::remove_file(&sock).ok();
    }

    fn rec_open(channel: u32) -> AgentRecord {
        AgentRecord {
            channel,
            kind: RecordKind::Open,
            payload: Vec::new(),
        }
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

    // --- AgentChannelMux (RFC 0011 §5 agent channels over the envelope) -----

    use crate::remote::channel::{
        AgentPayload, ChannelId, AGENT_FLAG_FAIL, AGENT_FLAG_OPEN, AGENT_INSTRUCTION_DATA_MAX,
        KIND_AGENT, SESSION_CHANNEL,
    };

    fn rec(channel: u32, kind: RecordKind, payload: &[u8]) -> AgentRecord {
        AgentRecord {
            channel,
            kind,
            payload: payload.to_vec(),
        }
    }

    /// Ferries every instruction `from` has due across to `to` (a lossless
    /// in-memory wire), returning the records `to` surfaced for its local
    /// machinery.
    fn ferry(from: &mut AgentChannelMux, to: &mut AgentChannelMux, now: u64) -> Vec<AgentRecord> {
        let mut out = Vec::new();
        for (id, wire) in from.outgoing(now, 50) {
            assert!(id.server_initiated(), "agent channels live in the server space");
            assert_eq!(id.kind(), KIND_AGENT);
            out.extend(to.on_instruction(id, &wire));
        }
        out
    }

    /// The §5 mirror of `channel_open_data_close_lifecycle`: the same
    /// endpoint + proxy machinery underneath, but the wire carriage is
    /// per-channel `AgentPayload` instructions on server-allocated kind-1
    /// identifiers instead of the retired CAP_AGENT_* record stream.
    #[test]
    fn enveloped_agent_channel_open_data_close_lifecycle() {
        let base = temp_base();
        let mut ep = AgentEndpoint::new(&base).unwrap();
        let mut mux_s = AgentChannelMux::new_server();

        let sock = temp_sock();
        std::fs::remove_file(&sock).ok();
        let listener = UnixListener::bind(&sock).unwrap();
        let mut proxy = AgentClient::new(sock.clone());
        let mut mux_c = AgentChannelMux::new_client();

        // A consumer connects on the remote end; the accepted channel and its
        // request become ONE instruction: OPEN-flagged (§3.3), offset 0.
        let mut consumer = UnixStream::connect(&ep.own_sock).unwrap();
        mux_s.queue_records(&ep.accept_pending());
        consumer.write_all(b"ssh-agent-request").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        mux_s.queue_records(&ep.read_channels());

        let sends = mux_s.outgoing(1_000, 50);
        assert_eq!(sends.len(), 1, "one channel, one instruction");
        let (id, wire) = &sends[0];
        assert!(id.server_initiated());
        assert_eq!(id.kind(), KIND_AGENT);
        assert_eq!(id.ordinal(), 1, "§3.1: ordinals start at 1");
        let p = AgentPayload::decode(wire).unwrap();
        assert_ne!(p.flags & AGENT_FLAG_OPEN, 0, "first instruction carries OPEN");
        assert_eq!(p.send_base, 0);
        assert_eq!(p.data, b"ssh-agent-request");

        // Client side: the OPEN dials the local agent, the data writes through.
        let recs = mux_c.on_instruction(*id, wire);
        assert!(recs.iter().any(|r| r.kind == RecordKind::Open));
        let replies = proxy.apply_records(&recs);
        assert!(replies.is_empty(), "reachable agent: no FAIL");
        let (mut agent_side, _) = listener.accept().unwrap();
        let mut got = [0u8; 17];
        agent_side.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ssh-agent-request");

        // Agent reply -> a data instruction back -> written to the consumer.
        agent_side.write_all(b"signature").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        mux_c.queue_records(&proxy.read_channels());
        let back = ferry(&mut mux_c, &mut mux_s, 1_100);
        ep.apply_records(&back);
        let mut sig = [0u8; 9];
        consumer.read_exact(&mut sig).unwrap();
        assert_eq!(&sig, b"signature");

        // Consumer closes -> a CLOSE-flagged terminal instruction -> the
        // proxy tears its side down (§5: CLOSE is terminal).
        drop(consumer);
        std::thread::sleep(std::time::Duration::from_millis(20));
        mux_s.queue_records(&ep.read_channels());
        let closes = ferry(&mut mux_s, &mut mux_c, 1_200);
        assert!(closes.iter().any(|r| r.kind == RecordKind::Close));
        proxy.apply_records(&closes);
        assert_eq!(proxy.live_channel_count(), 0);
        assert_eq!(ep.live_channel_count(), 0);

        drop(ep);
        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_file(&sock).ok();
    }

    /// §5 reliability: after a lost instruction, the sender's next emission
    /// carries the whole unacked tail from its cumulative base, the receiver
    /// delivers in offset order exactly once, and the peer's cumulative
    /// `recv_ack` finally drains the outbox.
    #[test]
    fn enveloped_agent_retransmits_unacked_tail_across_loss() {
        let mut mux_s = AgentChannelMux::new_server();
        mux_s.queue_records(&[rec(1, RecordKind::Open, b"")]);
        mux_s.queue_records(&[rec(1, RecordKind::Data, b"first-chunk|")]);
        let lost = mux_s.outgoing(0, 50);
        assert_eq!(lost.len(), 1);
        // ...dropped on the floor (never delivered).

        mux_s.queue_records(&[rec(1, RecordKind::Data, b"second-chunk")]);
        let sends = mux_s.outgoing(10, 50);
        assert_eq!(sends.len(), 1);
        let p = AgentPayload::decode(&sends[0].1).unwrap();
        assert_eq!(p.send_base, 0, "retransmission restarts at the acked base");
        assert_eq!(p.data, b"first-chunk|second-chunk");
        assert_ne!(
            p.flags & AGENT_FLAG_OPEN,
            0,
            "§3.3: the OPEN retransmits until the peer confirms it"
        );

        let mut mux_c = AgentChannelMux::new_client();
        let recs = mux_c.on_instruction(sends[0].0, &sends[0].1);
        let delivered: Vec<u8> = recs
            .iter()
            .filter(|r| r.kind == RecordKind::Data)
            .flat_map(|r| r.payload.clone())
            .collect();
        assert_eq!(delivered, b"first-chunk|second-chunk", "offset order, once");
        // A duplicate retransmission delivers nothing new (§3.3: dup OPEN is
        // a retransmission, not an error).
        assert!(mux_c.on_instruction(sends[0].0, &sends[0].1).is_empty());

        // The receiver's ack instruction drains the sender's outbox: nothing
        // further to send even far past the RTO.
        for (id, wire) in mux_c.outgoing(20, 50) {
            mux_s.on_instruction(id, &wire);
        }
        assert!(mux_s.outgoing(10_000, 50).is_empty());
    }

    /// §9.2: consecutive unacked retransmissions double the retx interval
    /// (rto ≪ streak) up to the 8× cap — the flood-reduction half of the
    /// congestion response.
    #[test]
    fn backoff_doubles_retx_interval_across_unacked_rtos() {
        let mut mux = AgentChannelMux::new_server();
        mux.queue_records(&[rec(1, RecordKind::Open, b""), rec(1, RecordKind::Data, b"x")]);
        assert_eq!(mux.outgoing(0, 50).len(), 1, "initial send_due emission");
        // Streak 0: first retx at +rto.
        assert!(mux.outgoing(49, 50).is_empty());
        assert_eq!(mux.outgoing(50, 50).len(), 1); // streak -> 1
        // Streak 1: next at +2*rto.
        assert!(mux.outgoing(149, 50).is_empty());
        assert_eq!(mux.outgoing(150, 50).len(), 1); // streak -> 2
        // Streak 2: next at +4*rto.
        assert!(mux.outgoing(349, 50).is_empty());
        assert_eq!(mux.outgoing(350, 50).len(), 1); // streak -> 3
        // Streak 3 (the cap): +8*rto, and it stays 8x forever after.
        assert!(mux.outgoing(749, 50).is_empty());
        assert_eq!(mux.outgoing(750, 50).len(), 1); // streak -> 4, capped
        assert!(mux.outgoing(1149, 50).is_empty());
        assert_eq!(mux.outgoing(1150, 50).len(), 1, "capped at 8x, not 16x");
    }

    /// §9.2: ack progress ends the streak — the next retransmission after
    /// recovery paces at the plain RTO again.
    #[test]
    fn backoff_resets_on_ack_progress() {
        let mut mux_s = AgentChannelMux::new_server();
        let mut mux_c = AgentChannelMux::new_client();
        mux_s.queue_records(&[rec(1, RecordKind::Open, b""), rec(1, RecordKind::Data, b"a")]);
        let first = mux_s.outgoing(0, 50);
        assert_eq!(mux_s.outgoing(50, 50).len(), 1); // streak -> 1
        assert_eq!(mux_s.outgoing(150, 50).len(), 1); // streak -> 2
        // The peer finally receives and acks — base advances, streak resets.
        mux_c.on_instruction(first[0].0, &first[0].1);
        for (id, wire) in mux_c.outgoing(151, 50) {
            mux_s.on_instruction(id, &wire);
        }
        // Fresh data goes out promptly (send_due), then its retx fires at
        // the PLAIN rto — the streak is gone.
        mux_s.queue_records(&[rec(1, RecordKind::Data, b"b")]);
        assert_eq!(mux_s.outgoing(200, 50).len(), 1, "send_due, no delay");
        assert!(mux_s.outgoing(249, 50).is_empty());
        assert_eq!(mux_s.outgoing(250, 50).len(), 1, "plain rto after reset");
    }

    /// §9.2: the peer's first instruction on the identifier confirms our
    /// OPEN and ends an OPEN-retx streak even with an empty outbox.
    #[test]
    fn backoff_resets_on_open_confirmation() {
        let mut mux_s = AgentChannelMux::new_server();
        let mut mux_c = AgentChannelMux::new_client();
        mux_s.queue_records(&[rec(1, RecordKind::Open, b"")]);
        let first = mux_s.outgoing(0, 50);
        assert_eq!(first.len(), 1);
        assert_eq!(mux_s.outgoing(50, 50).len(), 1); // OPEN retx, streak -> 1
        assert_eq!(mux_s.outgoing(150, 50).len(), 1); // streak -> 2
        // Confirmation arrives (the client's reply on the identifier).
        mux_c.on_instruction(first[0].0, &first[0].1);
        for (id, wire) in mux_c.outgoing(151, 50) {
            mux_s.on_instruction(id, &wire);
        }
        // Later data retransmits at the plain rto — the streak died with
        // the confirmation, not with an ack (the outbox was empty).
        mux_s.queue_records(&[rec(1, RecordKind::Data, b"z")]);
        assert_eq!(mux_s.outgoing(500, 50).len(), 1, "send_due emission");
        assert!(mux_s.outgoing(549, 50).is_empty());
        assert_eq!(mux_s.outgoing(550, 50).len(), 1, "plain rto cadence");
    }

    /// §9.2: the poll deadline mirrors the backed-off interval, so a loop
    /// sleeping on next_deadline neither fires retransmits early nor spins.
    #[test]
    fn next_deadline_honors_backoff() {
        let mut mux = AgentChannelMux::new_server();
        mux.queue_records(&[rec(1, RecordKind::Open, b""), rec(1, RecordKind::Data, b"x")]);
        let _ = mux.outgoing(0, 50);
        assert_eq!(mux.next_deadline(50), Some(50), "streak 0: plain rto");
        let _ = mux.outgoing(50, 50); // streak -> 1
        assert_eq!(mux.next_deadline(50), Some(150), "streak 1: last_send + 2*rto");
        let _ = mux.outgoing(150, 50); // streak -> 2
        assert_eq!(mux.next_deadline(50), Some(350), "streak 2: last_send + 4*rto");
    }

    /// §9.2: backoff delays only retransmissions of old state — fresh data
    /// (`send_due`) goes out immediately at any streak depth, without
    /// bumping the streak.
    #[test]
    fn send_due_is_never_delayed_by_backoff() {
        let mut mux = AgentChannelMux::new_server();
        mux.queue_records(&[rec(1, RecordKind::Open, b""), rec(1, RecordKind::Data, b"x")]);
        let _ = mux.outgoing(0, 50);
        let _ = mux.outgoing(50, 50); // streak -> 1
        let _ = mux.outgoing(150, 50); // streak -> 2
        // Fresh data at t=151, far inside the 4*rto backoff window.
        mux.queue_records(&[rec(1, RecordKind::Data, b"y")]);
        assert_eq!(mux.outgoing(151, 50).len(), 1, "send_due bypasses backoff");
        // And that emission did not deepen the streak: the next retx fires
        // at 151 + 4*rto (streak still 2), not 151 + 8*rto.
        assert!(mux.outgoing(350, 50).is_empty());
        assert_eq!(mux.outgoing(351, 50).len(), 1);
    }

    /// §9.2 MD: an unacked-data RTO fire halves cwnd — but at most once per
    /// window, even when several channels' RTOs coincide in one drain.
    #[test]
    fn cwnd_halves_at_most_once_per_window_on_rto_retx() {
        let mut mux = AgentChannelMux::new_server();
        mux.queue_records(&[rec(1, RecordKind::Open, b""), rec(1, RecordKind::Data, b"a")]);
        mux.queue_records(&[rec(2, RecordKind::Open, b""), rec(2, RecordKind::Data, b"b")]);
        let _ = mux.outgoing(100, 50); // rolls the window, initial sends
        assert_eq!(mux.cwnd, CWND_MAX);
        // Both channels' retx fire in the same drain: exactly one cut.
        let fired = mux.outgoing(150, 50);
        assert_eq!(fired.len(), 2, "both channels retransmit");
        assert_eq!(mux.cwnd, CWND_MAX / 2, "one halving, not two");
        assert_eq!(mux.cuts, 1);
    }

    /// §9.2: MD floors at one full instruction (forward progress can never
    /// deadlock on budget) and AI ceilings at today's implicit max.
    #[test]
    fn cwnd_floor_and_ceiling_clamp() {
        let mut mux = AgentChannelMux::new_server();
        mux.queue_records(&[rec(1, RecordKind::Open, b""), rec(1, RecordKind::Data, b"x")]);
        let mut now = 100;
        let _ = mux.outgoing(now, 50);
        // Cut every window far past the floor: 256K -> 128K -> ... -> 32K.
        for _ in 0..10 {
            now += 1000; // new window, and past any backoff interval
            let _ = mux.outgoing(now, 50);
        }
        assert_eq!(mux.cwnd, CWND_FLOOR, "MD floors at one instruction");
        // Recovery can never exceed CWND_MAX: drain the retx source, then
        // force progressed clean windows well past the 7 needed.
        mux.channels.clear();
        for _ in 0..10 {
            now += 1000;
            mux.window_progress = true;
            let _ = mux.outgoing(now, 50); // roll applies AI
        }
        assert_eq!(mux.cwnd, CWND_MAX, "AI ceilings at the implicit max");
    }

    /// §9.2 AI: a clean progressed window restores one instruction quantum.
    #[test]
    fn cwnd_recovers_additively_on_clean_progressed_windows() {
        let mut mux_s = AgentChannelMux::new_server();
        let mut mux_c = AgentChannelMux::new_client();
        mux_s.queue_records(&[rec(1, RecordKind::Open, b""), rec(1, RecordKind::Data, b"d")]);
        let first = mux_s.outgoing(100, 50);
        let _ = mux_s.outgoing(150, 50); // unacked retx: cut to 128K
        assert_eq!(mux_s.cwnd, CWND_MAX / 2);
        // The peer's ack lands in the SAME window as the cut: no recovery
        // from it (a cut window is not clean) — pin that first.
        mux_c.on_instruction(first[0].0, &first[0].1);
        for (id, wire) in mux_c.outgoing(160, 50) {
            mux_s.on_instruction(id, &wire);
        }
        let _ = mux_s.outgoing(210, 50); // roll: progressed BUT cut => no AI
        assert_eq!(mux_s.cwnd, CWND_MAX / 2, "a cut window never recovers");
        // A fresh exchange in the NEW (clean) window: progress marked, and
        // the next roll restores one quantum.
        mux_s.queue_records(&[rec(1, RecordKind::Data, b"e")]);
        let second = mux_s.outgoing(211, 50); // send_due emission
        mux_c.on_instruction(second[0].0, &second[0].1);
        for (id, wire) in mux_c.outgoing(220, 50) {
            mux_s.on_instruction(id, &wire);
        }
        assert!(mux_s.window_progress);
        let _ = mux_s.outgoing(270, 50); // clean + progressed => +32K
        assert_eq!(mux_s.cwnd, CWND_MAX / 2 + CWND_INCREMENT);
    }

    /// §9.2: OPEN and terminal retransmissions back off individually but
    /// never cut cwnd — a lost handshake is a poor congestion signal.
    #[test]
    fn open_and_terminal_retx_do_not_cut_cwnd() {
        // OPEN-only channel (empty outbox).
        let mut mux = AgentChannelMux::new_server();
        mux.queue_records(&[rec(1, RecordKind::Open, b"")]);
        let _ = mux.outgoing(100, 50);
        let fired = mux.outgoing(150, 50);
        assert_eq!(fired.len(), 1, "OPEN retransmits");
        assert_eq!(mux.cwnd, CWND_MAX, "no cut for an OPEN retx");
        // Terminal with a drained outbox.
        let mut mux2 = AgentChannelMux::new_server();
        mux2.queue_records(&[rec(1, RecordKind::Open, b"")]);
        let first = mux2.outgoing(100, 50);
        let mut mux_c = AgentChannelMux::new_client();
        mux_c.on_instruction(first[0].0, &first[0].1);
        for (id, wire) in mux_c.outgoing(101, 50) {
            mux2.on_instruction(id, &wire); // confirms the OPEN
        }
        mux2.queue_records(&[rec(1, RecordKind::Close, b"")]);
        let _ = mux2.outgoing(110, 50); // terminal rides (send_due)
        let terminal_retx = mux2.outgoing(200, 50);
        assert_eq!(terminal_retx.len(), 1, "terminal retransmits");
        assert_eq!(mux2.cwnd, CWND_MAX, "no cut for a terminal retx");
    }

    /// §9.2: idle windows neither cut nor recover — the learned cwnd
    /// survives idle instead of resetting.
    #[test]
    fn idle_windows_leave_cwnd_unchanged() {
        let mut mux = AgentChannelMux::new_server();
        mux.queue_records(&[rec(1, RecordKind::Open, b""), rec(1, RecordKind::Data, b"q")]);
        let _ = mux.outgoing(100, 50);
        let _ = mux.outgoing(150, 50); // cut to 128K
        let cut_to = mux.cwnd;
        assert_eq!(cut_to, CWND_MAX / 2);
        // Drain the channel so nothing wants sending, then roll many idle
        // windows.
        mux.channels.clear();
        for i in 0..20 {
            let _ = mux.outgoing(1_000 + i * 1_000, 50);
        }
        assert_eq!(mux.cwnd, cut_to, "idle windows change nothing");
    }

    /// §9.2: at CWND_MAX (the uncongested steady state) the budget is
    /// inert — a maximal 8-channel drain emits every channel's full
    /// per-instruction window exactly as the pre-§9.2 sender did.
    #[test]
    fn cwnd_at_max_is_byte_identical_to_ungated_drain() {
        let mut mux = AgentChannelMux::new_server();
        let big = vec![0xAAu8; AGENT_INSTRUCTION_DATA_MAX + 8 * 1024];
        for c in 1..=MAX_AGENT_CHANNELS as u32 {
            mux.queue_records(&[rec(c, RecordKind::Open, b"")]);
            mux.queue_records(&[rec(c, RecordKind::Data, &big)]);
        }
        let sends = mux.outgoing(100, 50);
        assert_eq!(sends.len(), MAX_AGENT_CHANNELS);
        for (_, wire) in &sends {
            let p = AgentPayload::decode(wire).unwrap();
            assert_eq!(
                p.data.len(),
                AGENT_INSTRUCTION_DATA_MAX,
                "full per-instruction window at cwnd max — no truncation"
            );
        }
        assert_eq!(mux.cwnd, CWND_MAX);
        assert!(
            mux.channels.iter().all(|c| !c.budget_starved),
            "nothing starves at max"
        );
    }

    /// §9.2 fairness: under a binding budget the rotate-by-one service
    /// order plus the refill re-arm spread the window across all channels —
    /// every channel's data lands within a few windows, none starves out.
    #[test]
    fn drain_rotation_serves_all_channels_under_reduced_cwnd() {
        let mut mux_s = AgentChannelMux::new_server();
        let mut mux_c = AgentChannelMux::new_client();
        let chunk = vec![0x55u8; AGENT_INSTRUCTION_DATA_MAX];
        for c in 1..=MAX_AGENT_CHANNELS as u32 {
            mux_s.queue_records(&[rec(c, RecordKind::Open, b"")]);
            mux_s.queue_records(&[rec(c, RecordKind::Data, &chunk)]);
        }
        // Congested: two instructions per window.
        mux_s.cwnd = 2 * AGENT_INSTRUCTION_DATA_MAX;
        mux_s.window_start = 100;
        let mut delivered: std::collections::HashMap<u32, usize> =
            std::collections::HashMap::new();
        let mut now = 100;
        for _ in 0..8 {
            for (id, wire) in mux_s.outgoing(now, 50) {
                let p = AgentPayload::decode(&wire).unwrap();
                *delivered.entry(id.ordinal() as u32).or_default() += p.data.len();
                // The peer receives and acks promptly (uncongested return
                // path), so served channels drain instead of re-offering.
                mux_c.on_instruction(id, &wire);
            }
            for (id, wire) in mux_c.outgoing(now + 1, 50) {
                mux_s.on_instruction(id, &wire);
            }
            now += 50; // next window
        }
        let served = delivered.values().filter(|&&v| v >= AGENT_INSTRUCTION_DATA_MAX).count();
        assert_eq!(
            served, MAX_AGENT_CHANNELS,
            "every channel's chunk fully delivered under the reduced cwnd: {delivered:?}"
        );
    }

    /// §9.2: an exhausted budget truncates data, never acks — a channel
    /// owing the peer an ack emits it as a zero-data instruction even at
    /// budget zero.
    #[test]
    fn zero_data_acks_bypass_exhausted_budget() {
        let mut mux_s = AgentChannelMux::new_server();
        let mut mux_c = AgentChannelMux::new_client();
        // ch1 carries bulk; ch2 exists to owe the peer an ack.
        let bulk = vec![0x77u8; AGENT_INSTRUCTION_DATA_MAX + 1024];
        mux_s.queue_records(&[rec(1, RecordKind::Open, b""), rec(2, RecordKind::Open, b"")]);
        let opens = mux_s.outgoing(100, 50);
        // Emission order rotates, so identify ch2 by its ordinal (server
        // allocation order matches queue_records order) and learn the
        // client's rec id from the Open record its adoption returns.
        let (ch2_id, ch2_wire) = opens
            .iter()
            .find(|(id, _)| id.ordinal() == 2)
            .map(|(id, w)| (*id, w.clone()))
            .unwrap();
        let mut ch2_rec = 0u32;
        for (id, wire) in &opens {
            for r in mux_c.on_instruction(*id, wire) {
                if r.kind == RecordKind::Open && *id == ch2_id {
                    ch2_rec = r.channel;
                }
            }
        }
        assert_ne!(ch2_rec, 0, "the client adopted ch2");
        let _ = ch2_wire;
        mux_c.queue_records(&[rec(ch2_rec, RecordKind::Data, b"payload-needing-ack")]);
        for (id, wire) in mux_c.outgoing(101, 50) {
            mux_s.on_instruction(id, &wire);
        }
        mux_s.queue_records(&[rec(1, RecordKind::Data, &bulk)]);
        // Congested to the floor; same window (no roll before the drain).
        mux_s.cwnd = CWND_FLOOR;
        mux_s.window_start = 100;
        let sends = mux_s.outgoing(120, 50);
        let ack = sends
            .iter()
            .map(|(id, wire)| (id, AgentPayload::decode(wire).unwrap()))
            .find(|(id, _)| **id == ch2_id)
            .expect("the ack-owing channel must emit despite the exhausted budget");
        assert!(ack.1.data.is_empty(), "budget-blocked data, never the ack");
        assert_eq!(
            ack.1.recv_ack,
            b"payload-needing-ack".len() as u64,
            "the zero-data instruction carries the current ack"
        );
    }

    /// §9.2 × §3.3: a terminal flag only rides an instruction carrying the
    /// entire remaining tail — budget truncation defers it (exactly like an
    /// over-32KiB tail always has), and it lands once acks + refill expose
    /// the full remainder.
    #[test]
    fn terminal_flag_defers_under_budget_truncation_and_lands_with_full_tail() {
        let mut mux_s = AgentChannelMux::new_server();
        let mut mux_c = AgentChannelMux::new_client();
        let tail = vec![0x99u8; AGENT_INSTRUCTION_DATA_MAX + 4 * 1024];
        mux_s.queue_records(&[rec(1, RecordKind::Open, b"")]);
        mux_s.queue_records(&[rec(1, RecordKind::Data, &tail)]);
        mux_s.queue_records(&[rec(1, RecordKind::Close, b"")]);
        mux_s.cwnd = CWND_FLOOR;
        mux_s.window_start = 100;
        let first = mux_s.outgoing(100, 50);
        let p = AgentPayload::decode(&first[0].1).unwrap();
        assert_eq!(p.data.len(), AGENT_INSTRUCTION_DATA_MAX, "truncated to the budget");
        assert_eq!(
            p.flags & (AGENT_FLAG_CLOSE | AGENT_FLAG_FAIL),
            0,
            "the terminal defers while any tail is unsent"
        );
        // The peer acks the first window; the next window's refill exposes
        // the 4 KiB remainder, and the terminal rides it.
        mux_c.on_instruction(first[0].0, &first[0].1);
        for (id, wire) in mux_c.outgoing(120, 50) {
            mux_s.on_instruction(id, &wire);
        }
        let second = mux_s.outgoing(160, 50); // next window (refill + send_due)
        let p2 = second
            .iter()
            .map(|(_, w)| AgentPayload::decode(w).unwrap())
            .find(|p| !p.data.is_empty())
            .expect("the remainder must send after refill");
        assert_eq!(p2.data.len(), 4 * 1024, "the entire remaining tail");
        assert_ne!(p2.flags & AGENT_FLAG_CLOSE, 0, "the terminal rides the full tail");
    }

    /// §9.2: a budget denial is not a loss signal — the starved channel's
    /// next_deadline is the window refill, not its (possibly backed-off)
    /// retransmission timer.
    #[test]
    fn budget_starved_channel_resumes_at_window_refill_not_backoff() {
        let mut mux_s = AgentChannelMux::new_server();
        let mut mux_c = AgentChannelMux::new_client();
        // ch1: served and fully acked (folds nothing afterward).
        let chunk = vec![0x11u8; AGENT_INSTRUCTION_DATA_MAX];
        mux_s.queue_records(&[rec(1, RecordKind::Open, b"")]);
        mux_s.queue_records(&[rec(1, RecordKind::Data, &chunk)]);
        mux_s.cwnd = CWND_FLOOR;
        mux_s.window_start = 100;
        let first = mux_s.outgoing(100, 50); // ch1 eats the whole budget
        mux_c.on_instruction(first[0].0, &first[0].1);
        for (id, wire) in mux_c.outgoing(101, 50) {
            mux_s.on_instruction(id, &wire);
        }
        // ch2 arrives mid-window: send_due, but the budget is gone.
        mux_s.queue_records(&[rec(2, RecordKind::Open, b"")]);
        mux_s.queue_records(&[rec(2, RecordKind::Data, b"starved")]);
        let mid = mux_s.outgoing(110, 50); // no roll (within the window)
        assert!(
            mid.iter()
                .all(|(_, w)| AgentPayload::decode(w).unwrap().data.is_empty()),
            "ch2's data is budget-blocked this window"
        );
        // Refill at window_start + rto = 150; ch2's own retx timer would be
        // last_send + rto = 160. The deadline must be the refill.
        assert_eq!(mux_s.next_deadline(50), Some(150), "resume at refill, not backoff");
    }

    /// §9.2 × §3.3: tombstone terminal echoes are zero-data and emit
    /// through an exhausted budget — a closed channel's straggler answer is
    /// never congestion-blocked.
    #[test]
    fn tombstone_echoes_unaffected_by_budget() {
        let mut mux_s = AgentChannelMux::new_server();
        let mut mux_c = AgentChannelMux::new_client();
        mux_s.queue_records(&[rec(1, RecordKind::Open, b"")]);
        let opens = mux_s.outgoing(100, 50);
        mux_c.on_instruction(opens[0].0, &opens[0].1);
        // The client closes the channel: the server tombstones it with a
        // terminal echo owed.
        mux_c.queue_records(&[rec(1, RecordKind::Close, b"")]);
        for (id, wire) in mux_c.outgoing(101, 50) {
            mux_s.on_instruction(id, &wire);
        }
        // Bulk on another channel exhausts the floor budget in-window.
        let bulk = vec![0x33u8; AGENT_INSTRUCTION_DATA_MAX];
        mux_s.queue_records(&[rec(2, RecordKind::Open, b"")]);
        mux_s.queue_records(&[rec(2, RecordKind::Data, &bulk)]);
        mux_s.cwnd = CWND_FLOOR;
        mux_s.window_start = 100;
        // A straggler from the peer on the closed identifier re-owes the
        // echo (it was consumed by the close handshake already).
        mux_c.queue_records(&[rec(1, RecordKind::Data, b"straggler")]);
        for (id, wire) in mux_c.outgoing(110, 50) {
            mux_s.on_instruction(id, &wire);
        }
        let sends = mux_s.outgoing(120, 50);
        let echo = sends
            .iter()
            .map(|(id, w)| (id, AgentPayload::decode(w).unwrap()))
            .find(|(id, _)| **id == opens[0].0)
            .expect("the tombstone echo must emit despite the exhausted budget");
        assert!(echo.1.data.is_empty());
        assert_ne!(
            echo.1.flags & (AGENT_FLAG_CLOSE | AGENT_FLAG_FAIL),
            0,
            "the echo re-answers with the terminal"
        );
    }

    /// §9.2 rollback switch: default on, off only for the shared off
    /// spellings (no env mutation — the parser is pinned directly).
    #[test]
    fn congestion_gate_parses_default_on() {
        assert!(parse_congestion_gate(None), "unset selects the response");
        for v in ["", "1", "true", "on", "yes", "2"] {
            assert!(parse_congestion_gate(Some(v)), "{v:?} selects the response");
        }
        for v in ["0", "false", "off", "no", " OFF "] {
            assert!(!parse_congestion_gate(Some(v)), "{v:?} switches it off");
        }
    }

    /// §9.2 rollback: with the gate off, the streak never grows (plain-RTO
    /// retransmission cadence forever) and cwnd never leaves max (budget
    /// inert) — the pre-§9.2 sender byte-for-byte.
    #[test]
    fn gate_off_restores_ungated_emission() {
        let mut mux = AgentChannelMux::new_server();
        mux.congestion = false;
        mux.queue_records(&[rec(1, RecordKind::Open, b""), rec(1, RecordKind::Data, b"x")]);
        let _ = mux.outgoing(100, 50);
        // Plain-rto cadence at any depth of unacked retransmissions.
        for i in 1..=6u64 {
            assert!(mux.outgoing(100 + i * 50 - 1, 50).is_empty());
            assert_eq!(mux.outgoing(100 + i * 50, 50).len(), 1, "plain rto, no backoff");
        }
        assert_eq!(mux.cwnd, CWND_MAX, "no cuts with the gate off");
        assert_eq!(mux.cuts, 0);
        assert_eq!(mux.next_deadline(50), Some(400 + 50), "plain-rto deadline");
    }

    /// §5: FAIL surfaces to the far end's agent client as a CLOSED socket —
    /// a `git push` against an unreachable agent fails, it never hangs.
    #[test]
    fn enveloped_agent_fail_surfaces_as_closed_socket() {
        let base = temp_base();
        let mut ep = AgentEndpoint::new(&base).unwrap();
        let mut mux_s = AgentChannelMux::new_server();
        // No listener behind the client proxy: every OPEN fails to connect.
        let sock = temp_sock();
        std::fs::remove_file(&sock).ok();
        let mut proxy = AgentClient::new(sock);
        let mut mux_c = AgentChannelMux::new_client();

        let mut consumer = UnixStream::connect(&ep.own_sock).unwrap();
        consumer
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        mux_s.queue_records(&ep.accept_pending());

        // OPEN travels over; the dead agent turns it into a FAIL reply.
        let recs = ferry(&mut mux_s, &mut mux_c, 1_000);
        let fails = proxy.apply_records(&recs);
        assert!(fails.iter().any(|r| r.kind == RecordKind::Fail));
        mux_c.queue_records(&fails);

        // The FAIL instruction reaches the server end and closes the
        // consumer's socket: read returns EOF, not a block/timeout.
        let back = ferry(&mut mux_c, &mut mux_s, 1_100);
        assert!(back.iter().any(|r| r.kind == RecordKind::Fail));
        ep.apply_records(&back);
        let mut buf = [0u8; 8];
        use std::io::Read as _;
        assert_eq!(
            consumer.read(&mut buf).expect("EOF, never a hang"),
            0,
            "FAIL must surface as a closed socket"
        );
        assert_eq!(ep.live_channel_count(), 0);

        drop(ep);
        std::fs::remove_dir_all(&base).ok();
    }

    /// One instruction carries what the retired 247-byte CAP_AGENT_DATA
    /// entry budget never could: a ~4 KB payload delivered whole.
    #[test]
    fn enveloped_agent_instruction_exceeds_retired_cap_budget() {
        let mut mux_s = AgentChannelMux::new_server();
        let blob = vec![0x5a; 4096];
        mux_s.queue_records(&[rec(1, RecordKind::Open, b"")]);
        mux_s.queue_records(&[rec(1, RecordKind::Data, &blob)]);
        let sends = mux_s.outgoing(0, 50);
        assert_eq!(sends.len(), 1, "one instruction, no 247-byte chunking");
        let p = AgentPayload::decode(&sends[0].1).unwrap();
        assert_eq!(p.data.len(), 4096);

        let mut mux_c = AgentChannelMux::new_client();
        let recs = mux_c.on_instruction(sends[0].0, &sends[0].1);
        let delivered: Vec<u8> = recs
            .iter()
            .filter(|r| r.kind == RecordKind::Data)
            .flat_map(|r| r.payload.clone())
            .collect();
        assert_eq!(delivered, blob, "delivered whole in one instruction");
    }

    /// §3.4: MAX_AGENT_CHANNELS is the `agent` kind's per-connection bound —
    /// the receiver refuses the 9th concurrent channel with FAIL instead of
    /// allocating past it.
    #[test]
    fn enveloped_agent_channel_bound_refused_with_fail() {
        let mut mux_c = AgentChannelMux::new_client();
        let open = AgentPayload {
            flags: AGENT_FLAG_OPEN,
            send_base: 0,
            recv_ack: 0,
            data: Vec::new(),
        }
        .encode();
        for ord in 1..=MAX_AGENT_CHANNELS as u64 {
            let recs = mux_c.on_instruction(ChannelId::new(true, KIND_AGENT, ord), &open);
            assert!(recs.iter().any(|r| r.kind == RecordKind::Open));
        }
        let ninth = ChannelId::new(true, KIND_AGENT, MAX_AGENT_CHANNELS as u64 + 1);
        assert!(
            mux_c.on_instruction(ninth, &open).is_empty(),
            "the 9th concurrent channel must not open"
        );
        let refusal = mux_c
            .outgoing(0, 50)
            .into_iter()
            .find(|(id, _)| *id == ninth)
            .expect("a FAIL instruction must answer the refused OPEN");
        let p = AgentPayload::decode(&refusal.1).unwrap();
        assert_ne!(p.flags & AGENT_FLAG_FAIL, 0, "§3.4: refuse with FAIL");
    }

    /// §4.1 sender discipline at the send-scheduling seam: a pending session
    /// instruction precedes bulk agent data, and one agent instruction never
    /// carries more than AGENT_INSTRUCTION_DATA_MAX bytes, so a keystroke
    /// frame waits behind at most one maximal agent instruction.
    #[test]
    fn session_instructions_precede_bulk_agent_data() {
        let mut mux = AgentChannelMux::new_server();
        mux.queue_records(&[rec(1, RecordKind::Open, b"")]);
        mux.queue_records(&[rec(1, RecordKind::Data, &vec![0xab; 100 * 1024])]);

        let sends = iteration_sends(Some(b"session frame".to_vec()), Some(&mut mux), 0, 50);
        assert!(sends.len() >= 2);
        assert_eq!(sends[0].0, SESSION_CHANNEL, "session first (§4.1)");
        assert_eq!(sends[0].1, b"session frame");
        for (id, wire) in &sends[1..] {
            assert_eq!(id.kind(), KIND_AGENT);
            let p = AgentPayload::decode(wire).unwrap();
            assert!(
                p.data.len() <= AGENT_INSTRUCTION_DATA_MAX,
                "§4.1: one agent instruction's data stays within the bound"
            );
        }
        // And with nothing pending on the session channel, agent data flows
        // without inventing an empty session instruction.
        let follow_up = iteration_sends(None, Some(&mut mux), 1_000, 50);
        assert!(follow_up.iter().all(|(id, _)| id.kind() == KIND_AGENT));
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
