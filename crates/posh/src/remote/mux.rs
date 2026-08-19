//! The per-destination local mux endpoint (M1, agent-only): destination
//! keys, the hardened `<base>/mux/` socket directory, and the client id the
//! remote side names in the FDR 0014 election.
//!
//! Design: docs/plans/2026-07-28-connection-mux-endpoint-design.md ("Keying
//! and placement", "Remote side"). This file carries Tasks 1 and 3 of
//! docs/plans/2026-07-28-mux-endpoint-m1-impl.md — the pure helpers, the
//! refcount/linger state machine, the zmx-style IPC protocol, and the
//! double-forked daemon owning the agent-only connection; client
//! integration (Task 4) consumes [`run_daemon`].

use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::remote::agent::{AgentChannelMux, AgentClient};
use crate::remote::caps;
use crate::remote::channel;
use crate::remote::datagram::{Connection, Family};
use crate::remote::sync::{self, AgentRecord, RecordKind, HEARTBEAT_INTERVAL};
use crate::util::{self, now_ms, Result};

/// Canonicalized, filesystem-safe destination key: `user@host` + address
/// family + port range (#54), rendered as a slug safe to embed in
/// `mux/<key>.sock`. The host is case-folded; an explicit user is prefixed
/// `user@`-style with the joiner rendered slug-safe; the family suffix
/// appears only for an explicit `-4`/`-6`, the port-range suffix only when a
/// non-default range was given — so the common invocation stays a bare
/// hostname slug.
pub fn dest_key(user: Option<&str>, host: &str, family: Family, port_range: Option<&str>) -> String {
    let mut key = String::new();
    if let Some(user) = user {
        // `user@host` with the `@` joiner rendered slug-safe (its mapped
        // byte). The default (no user) carries no marker, so the common key
        // stays the bare host slug.
        key.push_str(&sanitize_id(user));
        key.push('-');
    }
    key.push_str(&sanitize_id(&host.to_lowercase()));
    match family {
        Family::Auto => {}
        Family::Inet => key.push_str("-4"),
        Family::Inet6 => key.push_str("-6"),
    }
    if let Some(range) = port_range {
        key.push('-');
        key.push_str(&sanitize_id(range));
    }
    key
}

/// `<base>/mux/` under the same base-dir resolution as session sockets
/// (`POSH_DIR > XDG_RUNTIME_DIR/posh > TMPDIR/posh-{uid} > /tmp/posh-{uid}`),
/// created 0700 and hardened with the shared #7 check (self-owned,
/// symlink-rejecting) exactly like `<base>/agent/`.
pub fn mux_dir() -> Result<PathBuf> {
    let env = |k: &str| std::env::var(k).ok();
    let base = crate::session::resolve_socket_base(
        env("POSH_DIR").as_deref(),
        env("XDG_RUNTIME_DIR").as_deref(),
        env("TMPDIR").as_deref(),
        util::uid(),
    );
    mux_dir_at(&base)
}

/// `<base>/mux/<key>.sock` — the endpoint socket for a destination key.
pub fn mux_socket_path(key: &str) -> Result<PathBuf> {
    Ok(mux_socket_path_in(&mux_dir()?, key))
}

/// The join behind [`mux_socket_path`], pure so tests pin the path shape
/// without touching the env-resolved base.
fn mux_socket_path_in(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.sock"))
}

/// The client id the remote `posh-server agent` binds
/// `agent/mux-<client-id>.sock` under and the FDR 0014 election names: the
/// sanitized local hostname, overridable via `POSH_CLIENT_ID` for the
/// shared-hostname pathological case (design doc "Remote side"). The override
/// is sanitized too — the id lands in remote socket names either way.
pub fn client_id() -> String {
    match std::env::var("POSH_CLIENT_ID") {
        Ok(id) if !id.is_empty() => sanitize_id(&id),
        _ => sanitize_id(&hostname()),
    }
}

/// The promoted `POSH_MUX` gate (FDR 0014 stable bar): the per-destination
/// mux endpoint is DEFAULT ON; `POSH_MUX=0` (or `false`/`off`/`no`) is the
/// rollback switch — off means no mux spawn and sessions keep their own
/// forwarding, byte-identical to the pre-M1 bootstrap. The off-switch shape
/// is the shared [`util::parse_default_on_gate`], the same contract as
/// `POSH_SESSION_FRAMES`. On ensure failure the invocation falls back to
/// per-connection forwarding ([`apply_mux_gate`]), so default-on never
/// strands the user agentless.
fn parse_mux_gate(value: Option<&str>) -> bool {
    util::parse_default_on_gate(value)
}

/// Reads the [`parse_mux_gate`] decision from the environment.
pub fn mux_selected() -> bool {
    parse_mux_gate(std::env::var("POSH_MUX").ok().as_deref())
}

/// The M2 session-sharing gate: OPT-IN (`POSH_MUX_SESSIONS`, the
/// `POSH_CHANNELS` truthy shape — NOT the default-on off-switch), per the
/// 2026-08-05 design revision's rollout arc. Off (the default) keeps the
/// per-invocation relay attach byte-identical; on, a `host:session` attach
/// rides the mux daemon's connection as a session channel, falling back
/// per-invocation on any failure. Promotion to default-on is a later dated
/// decision.
pub fn mux_sessions_selected() -> bool {
    std::env::var("POSH_MUX_SESSIONS")
        .map(|v| crate::remote::sshwrap::env_value_on(&v))
        .unwrap_or(false)
}

/// Maps every byte outside `[A-Za-z0-9._-]` to `-`. The one sanitizer behind
/// both [`dest_key`] and [`client_id`], pure so it is testable without env
/// mutation.
fn sanitize_id(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => b as char,
            _ => '-',
        })
        .collect()
}

/// [`mux_dir`] under an explicit base (the seam tests use with a tempdir):
/// validates the base like `agent/` does, creates `mux/` 0700, validates the
/// leaf private + self-owned. Reuses `session::validate_session_dir` — the
/// same hardening helper `AgentEndpoint::build` uses; no duplicate checks.
fn mux_dir_at(base: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let uid = util::uid();
    // The base itself must be a real, self-owned dir (no symlink redirect);
    // it may be group-readable like any /tmp intermediate. github #7.
    crate::session::validate_session_dir(base, uid, false)?;
    let dir = base.join("mux");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    // The leaf that holds the mux sockets must be private + self-owned —
    // reject an attacker-planted dir or a symlink. github #7.
    crate::session::validate_session_dir(&dir, uid, true)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// The refcount/linger state machine (M1 Task 3 of
// docs/plans/2026-07-28-mux-endpoint-m1-impl.md). Pure and virtual-time
// (`now: u64` ms), so ref/unref/linger transitions are unit-tested without a
// daemon, a socket, or a clock.

/// Default linger after the last session unref (design doc "Lifecycle",
/// decided 2026-07-28): 60 s, ControlMaster-ish, conservative until rekey
/// (posh#145) lands.
pub const DEFAULT_LINGER_MS: u64 = 60_000;

/// The spawn grace: the orphan deadline armed at construction is at least
/// this long, independent of the linger window. `POSH_MUX_PERSIST=0` means
/// "exit the moment the last session ref drops" — it must NOT mean the
/// daemon exits before its spawner's FIRST ref can land (the deferred-accept
/// loop structure guarantees a freshly accepted conn's ref arrives an
/// iteration after the accept, so a zero orphan deadline would always win).
pub const SPAWN_GRACE_MS: u64 = 5_000;

/// `POSH_MUX_PERSIST` in SECONDS (the `POSH_SERVER_*_TMOUT` / ssh
/// ControlPersist convention), converted to the internal ms clock. `0`
/// disables lingering; unset/unparsable falls back to the 60 s default.
pub fn linger_ms_from_env() -> u64 {
    parse_linger_ms(std::env::var("POSH_MUX_PERSIST").ok().as_deref())
}

/// The pure predicate behind [`linger_ms_from_env`], testable without env
/// mutation.
fn parse_linger_ms(value: Option<&str>) -> u64 {
    match value.map(str::trim).and_then(|v| v.parse::<u64>().ok()) {
        Some(secs) => secs.saturating_mul(1000),
        None => DEFAULT_LINGER_MS,
    }
}

/// The FDR 0014 M1 policy machine: the count of live local session
/// invocations holding a `MuxSessionRef` gates agent serviceability
/// (`refs > 0`), and unref-to-zero arms the linger clock — the endpoint keeps
/// the connection (agent service OFF) for `linger_ms`, then
/// [`should_exit`](Self::should_exit) signals shutdown. Construction arms an
/// orphan deadline of at least [`SPAWN_GRACE_MS`] — so a daemon whose
/// spawner dies before its first ref exits instead of idling forever,
/// without a zero linger racing the spawner's own first ref; the normal
/// first ref cancels it, and `linger_ms` alone governs every post-unref
/// window.
pub struct MuxState {
    refs: usize,
    linger_ms: u64,
    /// `Some(deadline)` exactly while `refs == 0` (the linger window).
    linger_deadline: Option<u64>,
}

impl MuxState {
    pub fn new(linger_ms: u64, now: u64) -> MuxState {
        MuxState {
            refs: 0,
            linger_ms,
            linger_deadline: Some(now.saturating_add(linger_ms.max(SPAWN_GRACE_MS))),
        }
    }

    /// A `MuxSessionRef` landed: agent service on, linger cancelled.
    pub fn add_ref(&mut self) {
        self.refs += 1;
        self.linger_deadline = None;
    }

    /// A ref dropped (explicitly or by its IPC connection closing). Reaching
    /// zero turns agent service off and starts the linger window from `now`.
    pub fn unref(&mut self, now: u64) {
        self.refs = self.refs.saturating_sub(1);
        if self.refs == 0 {
            self.linger_deadline = Some(now.saturating_add(self.linger_ms));
        }
    }

    pub fn refs(&self) -> usize {
        self.refs
    }

    /// The FDR 0014 M1 gate: agent channels are serviced iff a session ref is
    /// held. Enforced client-side — the side whose agent is exposed.
    pub fn serviceable(&self) -> bool {
        self.refs > 0
    }

    /// Whether the linger clock is armed (refs == 0, window not yet checked).
    pub fn lingering(&self) -> bool {
        self.linger_deadline.is_some()
    }

    /// The shutdown signal: unreferenced and the linger window has elapsed.
    /// With `linger_ms == 0` this is true the moment the last ref drops.
    pub fn should_exit(&self, now: u64) -> bool {
        self.linger_deadline.is_some_and(|d| now >= d)
    }

    /// The next wall-clock moment the daemon loop must wake for (the linger
    /// expiry), for folding into its poll deadline. `None` while referenced.
    pub fn next_deadline(&self) -> Option<u64> {
        self.linger_deadline
    }
}

// ---------------------------------------------------------------------------
// The mux IPC protocol (M1 Task 3, design doc "IPC"): zmx-style framing —
// 1-byte tag + u32 LE payload length, the session/ipc.rs style — but in the
// mux socket's OWN tag space. Bounds-checked decodes throughout (RFC 0008
// security rules); same-uid IPC under the hardened mux/ dir.

/// The compile-time protocol/version stamp the RFC 0011 §6 endpoint rule
/// keys on: `"mux1/"` (the mux IPC protocol generation) + the §2 channel
/// envelope version this build speaks ([`channel::VER_1`], pinned by test).
/// A client seeing a different stamp in the `MuxHelloAck` MUST start a fresh
/// socket-name variant and let this endpoint drain — never negotiate down.
pub const MUX_PROTO_STAMP: &str = "mux1/1";

/// Upper bound on one mux IPC frame's payload. Legitimate payloads are a
/// stamp + a few scalars (well under 1 KiB); the bound stops a hostile or
/// confused peer from driving unbounded buffering via a huge length header.
/// M1's control-only protocol fit in 4 KiB; M2's `SessionFrame`/`SessionMsg`
/// carry whole encoded `ServerFrame`/`ClientMessage` bodies (a Full keyframe
/// of a large terminal runs to hundreds of KiB). Same-uid IPC under a
/// hardened dir, so the bound is a sanity cap against a corrupt length
/// prefix, not a security budget.
const MUX_MAX_FRAME_LEN: usize = 4 * 1024 * 1024;

/// Tags in the mux socket's own space (deliberately NOT `session/ipc::Tag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MuxTag {
    /// Client → mux: `MuxHello` (version stamp + pid). First verb on a conn.
    Hello = 0,
    /// Mux → client: `MuxHelloAck` (stamp, connection state, destination key).
    HelloAck = 1,
    /// Client → mux: register this invocation as a live local session for the
    /// destination (the FDR 0014 M1 policy input). At most one ref per IPC
    /// connection; the ref drops automatically when the connection closes, so
    /// a crashed client can never pin serviceability (no pid probing).
    SessionRef = 2,
    /// Client → mux: request the one-line debug summary (FDR 0007 surface).
    Status = 3,
    /// Mux → client: the summary line (UTF-8).
    StatusReply = 4,
    /// Mux → client: the `SessionRef` was registered. The confirmation the
    /// client BLOCKS on before surrendering its per-connection forwarding —
    /// a ref the daemon never acked is an error, not a silent success
    /// (apply_mux_gate's fallback keys on exactly this).
    RefAck = 5,
    /// Client → mux (M2): open a session channel to the RFC 0001 target
    /// (UTF-8, whole payload). Implies this conn's session ref.
    SessionOpen = 6,
    /// Mux → client (M2): [`MuxSessionOpenAck`] — the granted wire-channel
    /// ordinal, or a refusal reason (on which the client falls back to a
    /// per-invocation connection).
    SessionOpenAck = 7,
    /// Client → mux (M2): one encoded `ClientMessage`, opaque to the mux —
    /// relayed verbatim onto this conn's wire session channel.
    SessionMsg = 8,
    /// Mux → client (M2): [`MuxSessionFrame`] — the connection's live
    /// `srtt_ms` (u32 LE; the prediction engine's trigger, which the
    /// foreground process no longer owns a UDP socket to measure) followed
    /// by one encoded `ServerFrame`, opaque (geometry, scrollback caps, and
    /// codec selection pass through untouched).
    SessionFrame = 9,
    /// Either direction (M2): [`MuxSessionClose`] — a local detach, or the
    /// wire channel's terminal surfacing (remote daemon exit; the payload
    /// carries the exit-status path's bytes). Dropping the IPC conn implies
    /// close.
    SessionClose = 10,
}

impl MuxTag {
    fn from_u8(b: u8) -> Option<MuxTag> {
        Some(match b {
            0 => MuxTag::Hello,
            1 => MuxTag::HelloAck,
            2 => MuxTag::SessionRef,
            3 => MuxTag::Status,
            4 => MuxTag::StatusReply,
            5 => MuxTag::RefAck,
            6 => MuxTag::SessionOpen,
            7 => MuxTag::SessionOpenAck,
            8 => MuxTag::SessionMsg,
            9 => MuxTag::SessionFrame,
            10 => MuxTag::SessionClose,
            _ => return None,
        })
    }
}

pub fn encode_mux_frame(tag: MuxTag, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(tag as u8);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxFrame {
    pub tag: MuxTag,
    pub payload: Vec<u8>,
}

/// Reassembles mux frames from a (typically non-blocking) stream socket —
/// the `session/ipc::FrameBuffer` shape over the mux tag space, with the
/// tighter [`MUX_MAX_FRAME_LEN`] bound. Unknown tags are skipped (forward
/// compatibility); an oversize length errors so the conn is dropped.
#[derive(Default)]
pub struct MuxFrameBuffer {
    buf: Vec<u8>,
    head: usize,
}

impl MuxFrameBuffer {
    pub fn feed(&mut self, data: &[u8]) {
        if self.head > 0 {
            self.buf.drain(..self.head);
            self.head = 0;
        }
        self.buf.extend_from_slice(data);
    }

    pub fn next(&mut self) -> Result<Option<MuxFrame>> {
        loop {
            let avail = &self.buf[self.head..];
            if avail.len() < 5 {
                return Ok(None);
            }
            let len = u32::from_le_bytes([avail[1], avail[2], avail[3], avail[4]]) as usize;
            if len > MUX_MAX_FRAME_LEN {
                return Err(util::Error::Msg(format!(
                    "mux frame length {len} exceeds maximum {MUX_MAX_FRAME_LEN}"
                )));
            }
            if avail.len() < 5 + len {
                return Ok(None);
            }
            let tag_byte = avail[0];
            let payload = avail[5..5 + len].to_vec();
            self.head += 5 + len;
            if let Some(tag) = MuxTag::from_u8(tag_byte) {
                return Ok(Some(MuxFrame { tag, payload }));
            }
        }
    }
}

/// `MuxTag::Hello` payload: `pid: u32 LE` + the sender's protocol stamp
/// (UTF-8, to end of payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxHello {
    pub pid: u32,
    pub stamp: String,
}

impl MuxHello {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.stamp.len());
        out.extend_from_slice(&self.pid.to_le_bytes());
        out.extend_from_slice(self.stamp.as_bytes());
        out
    }

    pub fn decode(payload: &[u8]) -> Option<MuxHello> {
        if payload.len() < 4 {
            return None;
        }
        Some(MuxHello {
            pid: u32::from_le_bytes(payload[..4].try_into().ok()?),
            stamp: String::from_utf8_lossy(&payload[4..]).into_owned(),
        })
    }
}

/// The connection state a `MuxHelloAck` reports (design doc "IPC").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MuxConnState {
    /// The ssh bootstrap / UDP association is still coming up.
    Bootstrapping = 0,
    /// The enveloped agent-only connection is live.
    Connected = 1,
    /// Superseded (stamp mismatch) or winding down: serving no new refs.
    Draining = 2,
}

impl MuxConnState {
    fn from_u8(b: u8) -> Option<MuxConnState> {
        Some(match b {
            0 => MuxConnState::Bootstrapping,
            1 => MuxConnState::Connected,
            2 => MuxConnState::Draining,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            MuxConnState::Bootstrapping => "bootstrapping",
            MuxConnState::Connected => "connected",
            MuxConnState::Draining => "draining",
        }
    }
}

/// `MuxTag::HelloAck` payload: `state: u8`, `stamp_len: u16 LE`, the stamp,
/// `key_len: u16 LE`, the destination key (UTF-8), then the daemon's
/// resolved local agent-source path (OS bytes, to end of payload). The
/// source lets a joining invocation SEE which agent the endpoint actually
/// forwards — the daemon inherited its spawner's resolution, and a later
/// invocation resolving differently would otherwise diverge silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxHelloAck {
    pub state: MuxConnState,
    pub stamp: String,
    pub key: String,
    pub source: PathBuf,
}

impl MuxHelloAck {
    pub fn encode(&self) -> Vec<u8> {
        use std::os::unix::ffi::OsStrExt;
        let stamp = self.stamp.as_bytes();
        let key = self.key.as_bytes();
        let source = self.source.as_os_str().as_bytes();
        let mut out = Vec::with_capacity(5 + stamp.len() + key.len() + source.len());
        out.push(self.state as u8);
        out.extend_from_slice(&(stamp.len() as u16).to_le_bytes());
        out.extend_from_slice(stamp);
        out.extend_from_slice(&(key.len() as u16).to_le_bytes());
        out.extend_from_slice(key);
        out.extend_from_slice(source);
        out
    }

    pub fn decode(payload: &[u8]) -> Option<MuxHelloAck> {
        use std::os::unix::ffi::OsStrExt;
        if payload.len() < 3 {
            return None;
        }
        let state = MuxConnState::from_u8(payload[0])?;
        let stamp_len = u16::from_le_bytes([payload[1], payload[2]]) as usize;
        let rest = &payload[3..];
        if stamp_len.saturating_add(2) > rest.len() {
            return None;
        }
        let stamp = &rest[..stamp_len];
        let rest = &rest[stamp_len..];
        let key_len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
        let rest = &rest[2..];
        if key_len > rest.len() {
            return None;
        }
        Some(MuxHelloAck {
            state,
            stamp: String::from_utf8_lossy(stamp).into_owned(),
            key: String::from_utf8_lossy(&rest[..key_len]).into_owned(),
            source: PathBuf::from(std::ffi::OsStr::from_bytes(&rest[key_len..])),
        })
    }
}

/// `MuxTag::SessionOpenAck` payload (M2): `u8` granted flag, then either the
/// `u64 LE` wire-channel ordinal or a UTF-8 refusal reason (to end of
/// payload). A refusal is the client's cue to fall back to a per-invocation
/// connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MuxSessionOpenAck {
    Granted { ordinal: u64 },
    Refused { reason: String },
}

impl MuxSessionOpenAck {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            MuxSessionOpenAck::Granted { ordinal } => {
                let mut out = Vec::with_capacity(9);
                out.push(1);
                out.extend_from_slice(&ordinal.to_le_bytes());
                out
            }
            MuxSessionOpenAck::Refused { reason } => {
                let mut out = Vec::with_capacity(1 + reason.len());
                out.push(0);
                out.extend_from_slice(reason.as_bytes());
                out
            }
        }
    }

    pub fn decode(payload: &[u8]) -> Option<MuxSessionOpenAck> {
        match payload.first()? {
            1 => {
                if payload.len() < 9 {
                    return None;
                }
                Some(MuxSessionOpenAck::Granted {
                    ordinal: u64::from_le_bytes(payload[1..9].try_into().ok()?),
                })
            }
            0 => Some(MuxSessionOpenAck::Refused {
                reason: String::from_utf8_lossy(&payload[1..]).into_owned(),
            }),
            _ => None,
        }
    }
}

/// `MuxTag::SessionFrame` payload (M2): `u32 LE srtt_ms` + the encoded
/// `ServerFrame` bytes, opaque to the mux. The srtt rides every frame so the
/// foreground client's prediction engine keeps its SRTT trigger without
/// owning the UDP socket.
pub fn encode_session_frame(srtt_ms: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&srtt_ms.to_le_bytes());
    out.extend_from_slice(body);
    out
}

pub fn decode_session_frame(payload: &[u8]) -> Option<(u32, &[u8])> {
    if payload.len() < 4 {
        return None;
    }
    Some((
        u32::from_le_bytes(payload[..4].try_into().ok()?),
        &payload[4..],
    ))
}

/// `MuxTag::SessionClose` payload (M2): `u8` origin (0 = local detach,
/// 1 = the wire channel's remote terminal) + the exit-status path's bytes
/// (opaque, possibly empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxSessionClose {
    pub remote: bool,
    pub payload: Vec<u8>,
}

impl MuxSessionClose {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + self.payload.len());
        out.push(self.remote as u8);
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(payload: &[u8]) -> Option<MuxSessionClose> {
        match payload.first()? {
            0 | 1 => Some(MuxSessionClose {
                remote: payload[0] == 1,
                payload: payload[1..].to_vec(),
            }),
            _ => None,
        }
    }
}

/// M2 wire micro-envelope: the first byte of every instruction on a
/// session-kind channel with ordinal >= 2 (ordinal 1 — `SESSION_CHANNEL` —
/// stays bare for the M1 heartbeat and the pre-M2 single-session wire).
/// RFC 0011 §3.3 requires the OPEN-bearing instruction to carry the
/// channel's binding parameters and a receiver to distinguish stragglers
/// from opens; datagrams reorder, so the marker cannot be positional.
pub(crate) const SESSION_WIRE_DATA: u8 = 0;
pub(crate) const SESSION_WIRE_OPEN: u8 = 1;
pub(crate) const SESSION_WIRE_CLOSE: u8 = 2;

/// How long an unconfirmed session OPEN retransmits (one send per RTO)
/// before the endpoint gives up and surfaces a remote close to the client
/// (which then falls back per-invocation). Generous: a cold remote daemon
/// spawn sits behind connect-or-create.
const SESSION_OPEN_RETRANSMITS_MAX: u32 = 60;

/// Best-effort close retransmits (the agent kind's TERM_RETRANSMITS shape);
/// the remote also reaps on connection silence, so this only shortens the
/// window.
const SESSION_CLOSE_RETRANSMITS: u32 = 4;

/// The local bound on session channels across one endpoint's IPC conns —
/// mirrors the remote peer's table bound so a refusal happens on the unix
/// hop, not a wire round-trip later.
const MAX_LOCAL_SESSION_CHANNELS: usize = 16;

/// One IPC conn's live session channel (M2): the wire channel it owns, the
/// open state (RFC 0011 §3.3 open-until-confirmed), and messages queued
/// while unconfirmed (they must not race the OPEN onto the wire — a data
/// instruction arriving on an unseen identifier is a straggler, not an
/// open).
struct IpcSession {
    chan: channel::ChannelId,
    target: Vec<u8>,
    confirmed: bool,
    queued: Vec<Vec<u8>>,
    open_sends: u32,
    last_open_send: Option<u64>,
}

/// A close owed to the wire after its IPC conn is gone (or detached):
/// retransmitted [`SESSION_CLOSE_RETRANSMITS`] times on the RTO cadence.
struct PendingClose {
    chan: channel::ChannelId,
    payload: Vec<u8>,
    sends: u32,
    last_send: Option<u64>,
}

/// The M2 verbs `process_ipc_conn` parses but cannot apply itself (they
/// need the wire connection, the allocator, and the routing table, all
/// owned by `mux_loop`).
enum MuxSessionVerb {
    Open(Vec<u8>),
    Msg(Vec<u8>),
    Close(Vec<u8>),
}

/// One accepted IPC connection: framing reassembly plus the per-conn
/// facts the daemon tracks — hello completed, whether this conn holds
/// the (at most one) session ref that auto-drops on close, a stable id
/// (routing survives Vec index shifts), and the M2 session channel.
struct IpcConn {
    stream: UnixStream,
    read_buf: MuxFrameBuffer,
    hello_ok: bool,
    holds_ref: bool,
    conn_id: u64,
    session: Option<IpcSession>,
    /// The client pid its `Hello` reported — attribution for the posh#161
    /// ref-lifecycle log lines (which invocation pinned the daemon alive).
    peer_pid: Option<u32>,
}

impl IpcConn {
    fn with_id(stream: UnixStream, conn_id: u64) -> IpcConn {
        IpcConn {
            stream,
            read_buf: MuxFrameBuffer::default(),
            hello_ok: false,
            holds_ref: false,
            conn_id,
            session: None,
            peer_pid: None,
        }
    }
}

/// The per-iteration facts `MuxStatus` reports alongside the live
/// [`MuxState`]: destination key, connection state, peer address,
/// last-heard age, forwarded-channel count — plus the resolved local
/// agent-source path the `MuxHelloAck` reports.
struct MuxStatusCtx<'a> {
    key: &'a str,
    conn_state: MuxConnState,
    peer: Option<std::net::SocketAddr>,
    heard_age_ms: u64,
    channels: usize,
    agent_source: &'a Path,
    /// The §9.2 congestion summary (`AgentChannelMux::congestion_summary`):
    /// live cwnd bytes, cumulative MD cuts, deepest backoff streak.
    congestion: (usize, u64, u32),
    /// RFC 0013 §3: the remote endpoint's identity, once its heartbeat
    /// answer landed — `mux ls`'s "what build is the far end" column.
    remote_ident: Option<&'a caps::ServerIdent>,
}

/// The `MuxStatus` one-liner (FDR 0007 dump surface): peer addr, last-heard
/// age, channel count, refs, linger state, §9.2 congestion summary.
fn status_line(ctx: &MuxStatusCtx, state: &MuxState) -> String {
    format!(
        "mux {key}: state={cs} peer={peer} remote={remote} heard={heard}ms channels={ch} refs={refs} linger={linger} cwnd={cwnd} cuts={cuts} streak_hwm={hwm}",
        key = ctx.key,
        cs = ctx.conn_state.label(),
        peer = ctx
            .peer
            .map_or_else(|| "none".to_string(), |a| a.to_string()),
        remote = ctx.remote_ident.map_or_else(
            || "unknown".to_string(),
            |id| format!("{} ({})", id.version, id.git_sha)
        ),
        heard = ctx.heard_age_ms,
        ch = ctx.channels,
        refs = state.refs(),
        linger = if state.lingering() { "armed" } else { "off" },
        cwnd = ctx.congestion.0,
        cuts = ctx.congestion.1,
        hwm = ctx.congestion.2,
    )
}

/// Writes one mux frame to a conn, reporting whether the conn is still good.
/// IPC replies are tiny (a stamp + scalars), so a short/failed write within
/// the retry budget just condemns the conn — no partial-write bookkeeping.
fn send_mux_frame(conn: &mut IpcConn, tag: MuxTag, payload: &[u8]) -> bool {
    let wire = encode_mux_frame(tag, payload);
    matches!(
        util::write_all_retry(conn.stream.as_raw_fd(), &wire, 100),
        Ok(n) if n == wire.len()
    )
}

/// Drains and processes one IPC connection: reads until `WouldBlock`, then
/// dispatches every complete frame. Returns whether the connection stays
/// open; `false` covers EOF, I/O errors, malformed frames, protocol-order
/// violations (a verb before `Hello`), and the §6 stamp mismatch — which is
/// first ANSWERED with our own stamp (so the client can tell and start a
/// fresh `<key>.<ver>` endpoint) and then rejected. The caller drops a dead
/// conn via [`drop_ipc_conn`], which releases its auto-unref ref.
fn process_ipc_conn(
    conn: &mut IpcConn,
    state: &mut MuxState,
    ctx: &MuxStatusCtx,
    verbs: &mut Vec<MuxSessionVerb>,
) -> bool {
    let fd = conn.stream.as_raw_fd();
    let mut open = true;
    loop {
        let mut tmp = [0u8; 1024];
        match util::read_fd(fd, &mut tmp) {
            Ok(0) => {
                open = false;
                break;
            }
            Ok(n) => conn.read_buf.feed(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                open = false;
                break;
            }
        }
    }
    loop {
        let frame = match conn.read_buf.next() {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(_) => return false, // oversize/corrupt framing: drop the peer
        };
        match frame.tag {
            MuxTag::Hello => {
                let Some(hello) = MuxHello::decode(&frame.payload) else {
                    return false;
                };
                let ack = MuxHelloAck {
                    state: ctx.conn_state,
                    stamp: MUX_PROTO_STAMP.to_string(),
                    key: ctx.key.to_string(),
                    source: ctx.agent_source.to_path_buf(),
                };
                if !send_mux_frame(conn, MuxTag::HelloAck, &ack.encode()) {
                    return false;
                }
                if hello.stamp != MUX_PROTO_STAMP {
                    // RFC 0011 §6: never negotiate down. The ack above told
                    // the client OUR stamp; it starts a fresh endpoint and
                    // this conn is rejected.
                    util::log_write(
                        "warn",
                        &format!(
                            "rejecting mux hello with stamp {:?} (ours: {MUX_PROTO_STAMP}) pid={}",
                            hello.stamp, hello.pid
                        ),
                    );
                    return false;
                }
                conn.hello_ok = true;
                conn.peer_pid = Some(hello.pid);
            }
            MuxTag::SessionRef => {
                if !conn.hello_ok {
                    return false; // protocol order: Hello first
                }
                // Each accepted IPC conn carries at most one ref; a duplicate
                // is idempotent so a confused client cannot inflate the count.
                if !conn.holds_ref {
                    conn.holds_ref = true;
                    state.add_ref();
                    log_ref_change("+ipc-ref", conn.peer_pid, state);
                }
                // Confirm registration — the client blocks on this before
                // dropping its own forwarding. A duplicate is re-acked so a
                // waiting client never hangs on an idempotent ref.
                if !send_mux_frame(conn, MuxTag::RefAck, b"") {
                    return false;
                }
            }
            MuxTag::Status => {
                if !conn.hello_ok {
                    return false;
                }
                let line = status_line(ctx, state);
                if !send_mux_frame(conn, MuxTag::StatusReply, line.as_bytes()) {
                    return false;
                }
            }
            MuxTag::SessionOpen => {
                if !conn.hello_ok {
                    return false; // protocol order: Hello first
                }
                verbs.push(MuxSessionVerb::Open(frame.payload));
            }
            MuxTag::SessionMsg => {
                if !conn.hello_ok {
                    return false;
                }
                verbs.push(MuxSessionVerb::Msg(frame.payload));
            }
            MuxTag::SessionClose => {
                if !conn.hello_ok {
                    return false;
                }
                let payload = MuxSessionClose::decode(&frame.payload)
                    .map(|c| c.payload)
                    .unwrap_or_default();
                verbs.push(MuxSessionVerb::Close(payload));
            }
            // Mux → client verbs arriving FROM a peer: ignore, keep the conn.
            MuxTag::HelloAck | MuxTag::StatusReply | MuxTag::RefAck
            | MuxTag::SessionOpenAck | MuxTag::SessionFrame => {}
        }
    }
    open
}

/// posh#161 observability: one log line per session-ref transition, so the
/// daemon log answers "which invocations pinned this daemon alive" and "when
/// did agent service actually stop" (refs=0 arms the linger with service off).
fn log_ref_change(action: &str, pid: Option<u32>, state: &MuxState) {
    util::log_write(
        "info",
        &format!(
            "refs {action} (pid={}): refs={} linger={}",
            pid.map_or_else(|| "?".to_string(), |p| p.to_string()),
            state.refs(),
            if state.lingering() { "armed" } else { "off" }
        ),
    );
}

/// Releases a departing conn's session ref (the auto-unref half of
/// `MuxSessionRef`): the caller invokes this exactly once per dropped conn.
fn drop_ipc_conn(conn: &IpcConn, state: &mut MuxState, now: u64) {
    if conn.holds_ref {
        state.unref(now);
        log_ref_change("-conn-drop", conn.peer_pid, state);
    }
}

// ---------------------------------------------------------------------------
// The daemon (M1 Task 3, design doc "Lifecycle"): a double-forked,
// process-grouped grandchild per destination key — the session-daemon
// pattern — owning the ssh bootstrap for `posh-server agent` and the client
// half of the agent channels.

/// What ensuring the endpoint for a key produced, for the spawner (Task 4).
#[derive(Debug, PartialEq, Eq)]
pub enum MuxSpawn {
    /// This process bound the socket and forked the daemon; connect now.
    Spawned,
    /// A live daemon already owns the socket (we lost the bind race, or one
    /// was simply already up): connect to it instead.
    AlreadyRunning,
}

enum MuxBind {
    Bound(UnixListener),
    ExistingDaemon,
}

/// The bind seam, unit-testable with two binds in one process: bind wins;
/// on `AddrInUse`, a successful probe connect means a live winner exists
/// (losing race ⇒ defer to it), a dead socket (crash leftover) is unlinked
/// and rebound — the session-daemon stale-socket pattern (github #15: only a
/// genuinely dead socket is reclaimed; a live-but-slow daemon is not).
fn bind_or_probe(path: &Path) -> Result<MuxBind> {
    match UnixListener::bind(path) {
        Ok(l) => return Ok(MuxBind::Bound(l)),
        // "a node is already at this path — probe it." Linux reports EADDRINUSE
        // (AddrInUse); macOS reports EEXIST (AlreadyExists) when a concurrent
        // bind to the same path just won the race — the cold-start loser. Both
        // must route into the live-daemon-defer / dead-socket-reclaim logic
        // below, not the hard-error arm.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AlreadyExists
            ) => {}
        Err(e) => return Err(util::Error::Msg(format!("bind {}: {e}", path.display()))),
    }
    if socket_becomes_live(path) {
        return Ok(MuxBind::ExistingDaemon);
    }
    util::log_write(
        "warn",
        &format!("stale mux socket found, cleaning up {}", path.display()),
    );
    let _ = std::fs::remove_file(path);
    UnixListener::bind(path)
        .map(MuxBind::Bound)
        .map_err(|e| util::Error::Msg(format!("bind {}: {e}", path.display())))
}

/// Bounded liveness probe for a contended socket path. Returns `true` as soon
/// as a connect succeeds (a live winner — reclaim would clobber it), and
/// `false` only after the socket stays unconnectable across every attempt (a
/// genuine crash leftover — safe to reclaim). The retry closes the concurrent
/// cold-start window where a winner has bound but the loser probed a beat too
/// early. See [`PROBE_ATTEMPTS`].
fn socket_becomes_live(path: &Path) -> bool {
    for i in 0..PROBE_ATTEMPTS {
        if !crate::session::socket_is_dead(path) {
            return true;
        }
        if i + 1 < PROBE_ATTEMPTS {
            std::thread::sleep(PROBE_RETRY_DELAY);
        }
    }
    false
}

/// Bound on the mux daemon's bootstrap ssh TCP connect. The daemon is
/// SHARED per destination, so a hung ssh here would gate every invocation
/// behind it for the hang's duration (pre-M1 a hang cost only its own
/// invocation); on the resulting bootstrap error the daemon exits and
/// unlinks its socket, so the next invocation's bind reclaims the key.
const MUX_BOOTSTRAP_CONNECT_TIMEOUT_SECS: u32 = 10;

/// The mux daemon's bootstrap [`SshOptions`](crate::remote::sshwrap::SshOptions):
/// no `-A` (the agent verb IS forwarding), channels on (RFC 0011 §5 agent
/// channels exist only enveloped), and the bounded ssh ConnectTimeout —
/// factored from [`run_daemon`] so the call shape is pinned by test.
fn mux_ssh_options(
    family: Family,
    port_range: Option<String>,
) -> crate::remote::sshwrap::SshOptions {
    crate::remote::sshwrap::SshOptions {
        family,
        port_range,
        agent_source: None, // the agent verb IS forwarding; no -A
        real_ssh_agent_forward: None,
        channels: true, // RFC 0011 §6: selected on the bootstrap invocation
        connect_timeout_secs: Some(MUX_BOOTSTRAP_CONNECT_TIMEOUT_SECS),
    }
}

/// Ensure the mux daemon for `key` toward ssh destination `dest`
/// (`[user@]host`, what [`sshwrap::run`](crate::remote::sshwrap::run) takes).
/// Binds `mux/<key>.sock` (losing a race defers to the live winner; a stale
/// socket is reclaimed), double-forks like a session daemon, and in the
/// grandchild drives the ssh bootstrap for `posh-server agent --client-id
/// <id>` (channels implied — RFC 0011 §5 agent channels exist only enveloped)
/// before entering [`mux_loop`]. `agent_source` is the FDR 0004-resolved
/// local agent socket the spawning invocation carries (design doc
/// "Security": the endpoint inherits the spawner's resolved source).
///
/// Returns in the SPAWNER only; the daemon grandchild exits the process.
pub fn run_daemon(
    key: &str,
    dest: &str,
    family: Family,
    port_range: Option<String>,
    agent_source: PathBuf,
) -> Result<MuxSpawn> {
    let sock = mux_socket_path(key)?;
    let listener = match bind_or_probe(&sock)? {
        MuxBind::ExistingDaemon => return Ok(MuxSpawn::AlreadyRunning),
        MuxBind::Bound(l) => l,
    };
    if util::double_fork()? {
        drop(listener);
        // Give the grandchild a beat to exist before the spawner connects
        // (the socket is already bound, so a fast connect just queues).
        std::thread::sleep(std::time::Duration::from_millis(10));
        return Ok(MuxSpawn::Spawned);
    }

    // The daemon grandchild. Mirror daemon_main: detach stdio, log to a
    // per-key file beside the socket, record panics, name terminating
    // signals.
    util::redirect_stdio_devnull();
    let _ = util::log_init(&sock.with_extension("log"));
    std::panic::set_hook(Box::new(|info| {
        util::log_write("error", &format!("mux daemon panic: {info}"));
    }));
    util::install_daemon_signal_handlers();
    // SIGUSR2 appends a one-line status to the daemon's log (FDR 0007's dump
    // surface, mux shape). Also load-bearing as a handler: without it the
    // default disposition would TERMINATE the daemon — and with it every
    // session's agent forwarding to this destination (posh#161 blast radius).
    util::install_sigusr2_handler();

    let result = (|| -> Result<()> {
        let opts = mux_ssh_options(family, port_range);
        let tail = vec![
            "agent".to_string(),
            "--client-id".to_string(),
            client_id(),
        ];
        let (host, port, key_b64) = crate::remote::sshwrap::bootstrap(dest, &tail, &opts)?;
        let addr = crate::remote::client::resolve(&host, port, family)?;
        let udp_key = crate::remote::crypto::Key::from_base64(key_b64.trim())?;
        let conn = Connection::client(addr, &udp_key)?;
        util::log_write(
            "info",
            &format!("mux daemon started key={key} dest={dest} peer={addr}"),
        );
        mux_loop(listener, conn, &agent_source, linger_ms_from_env(), key);
        Ok(())
    })();
    let _ = std::fs::remove_file(&sock);
    match result {
        Ok(()) => {
            util::log_write("info", &format!("mux daemon exiting key={key}"));
            std::process::exit(0);
        }
        Err(e) => {
            util::log_write("error", &format!("mux daemon failed key={key}: {e}"));
            std::process::exit(1);
        }
    }
}

/// The `ClientMessage` heartbeat body: the connection keepalive is the SAME
/// mechanism the roaming client uses — a session-channel instruction at
/// least every [`HEARTBEAT_INTERVAL`] (`drive_client`'s `session_due` arm) —
/// not a new wire message. The agent-only remote discards session-kind
/// instructions but counts them as authentic peer activity (they cleared the
/// AEAD seal), which is exactly what keeps its `last_heard`/election marker
/// fresh. Zero rows/cols and an empty input stream: there is no session.
/// While no remote ident is held, the caps carry the RFC 0013 §3 identity
/// request (the remote answers with one Empty frame; the request stops the
/// moment the answer lands, so a steady-state heartbeat is caps-empty).
fn heartbeat_message(request_ident: bool) -> Vec<u8> {
    sync::ClientMessage {
        flags: 0,
        caps: if request_ident {
            vec![caps::Cap {
                id: caps::CAP_SERVER_IDENT,
                payload: vec![],
            }]
        } else {
            Vec::new()
        },
        acked_frame: 0,
        rows: 0,
        cols: 0,
        input_base: 0,
        input: Vec::new(),
    }
    .encode()
}

/// The daemon's event loop — `drive_client` minus everything session, plus
/// the IPC listener: polls the IPC socket + per-conn fds, the enveloped UDP
/// connection, and the local-agent proxy's channel fds. Server-initiated
/// agent OPENs dial the local `agent_source` while a session ref is held;
/// with `refs == 0` every OPEN is answered FAIL and open channels are closed
/// on the unref-to-zero edge (the FDR 0014 M1 policy, client-enforced).
/// Outbound traffic drains through `iteration_sends(None, ..)` with RTO
/// pacing; a heartbeat session instruction rides at least every
/// [`HEARTBEAT_INTERVAL`]. Exits on the linger expiry or a terminating
/// signal; the remote side's Drop follows from the ensuing silence (its
/// peer timeout). Factored from [`run_daemon`] so tests drive it in-process
/// over loopback UDP against a real Task 2 `agent_only_loop` peer.
fn mux_loop(
    listener: UnixListener,
    mut conn: Connection,
    agent_source: &Path,
    linger_ms: u64,
    key: &str,
) {
    let _ = listener.set_nonblocking(true);
    let mut fragmenter = sync::Fragmenter::new();
    let mut assembly = sync::FragmentAssembly::new();
    let mut agent_mux = AgentChannelMux::new_client();
    let mut proxy = AgentClient::new(agent_source.to_path_buf());
    let mut state = MuxState::new(linger_ms, now_ms());
    let mut conns: Vec<IpcConn> = Vec::new();
    // M2 session channels: the wire allocator (ordinal 1 = SESSION_CHANNEL,
    // reserved for the bare heartbeat stream — session channels start at 2),
    // the ChannelId → conn_id routing table (conn ids are stable across Vec
    // index shifts), and closes owed to the wire after their conn is gone.
    let mut alloc = channel::ChannelAllocator::new(channel::Role::Client);
    let reserved = alloc.next(channel::KIND_SESSION);
    debug_assert_eq!(reserved, channel::SESSION_CHANNEL);
    let mut routes: std::collections::HashMap<channel::ChannelId, u64> =
        std::collections::HashMap::new();
    let mut pending_closes: Vec<PendingClose> = Vec::new();
    let mut next_conn_id: u64 = 1;
    let mut verbs: Vec<MuxSessionVerb> = Vec::new();
    // `None` = never sent, so the FIRST heartbeat goes on the first
    // iteration — the remote learns its peer address within ms of the
    // connection coming up. (A `0` sentinel would NOT work: `now_ms()` is
    // monotonic from a recent base, not epoch ms, so early in the daemon
    // process `now - 0` sits below HEARTBEAT_INTERVAL and the first
    // heartbeat would wait until the clock crossed 3 s — the same trap
    // relay.rs's `wait_for_handshake` documents.)
    let mut last_send: Option<u64> = None;
    let mut last_heard: u64 = now_ms();
    // posh#161 observability: wire recv errors, edge-logged once per episode.
    // On a connected UDP socket an exited remote endpoint surfaces here as
    // ECONNREFUSED (ICMP port unreachable) — the one client-side signal that
    // distinguishes "remote died" from a merely idle connection (an idle M1
    // remote legitimately sends nothing, so heard-age alone proves nothing).
    let mut recv_err_logged = false;
    // RFC 0013 §3: the remote endpoint's identity — requested on every
    // heartbeat until held, then reported on the status line (`mux ls`'s
    // remote= column, the "what build is the far end" answer).
    let mut remote_ident: Option<caps::ServerIdent> = None;

    loop {
        if util::take_flag(&util::SIGTERM_RECEIVED) {
            let signo = util::LAST_SIGNAL.load(std::sync::atomic::Ordering::Acquire);
            util::log_write(
                "info",
                &format!("{} received, mux daemon winding down", util::signal_name(signo)),
            );
            break;
        }
        let now = now_ms();

        // Wake for the next heartbeat, the linger expiry, the agent mux's
        // fresh sends / RTO retransmissions (RFC 0011 §5), and the M2
        // session-channel open/close retransmit cadence.
        let mut deadline = last_send.map_or(now, |t| t.saturating_add(HEARTBEAT_INTERVAL));
        if let Some(d) = state.next_deadline() {
            deadline = deadline.min(d);
        }
        if let Some(d) = agent_mux.next_deadline(conn.rto()) {
            deadline = deadline.min(d.max(now));
        }
        for c in &conns {
            if let Some(s) = &c.session {
                if !s.confirmed {
                    let due = s.last_open_send.map_or(now, |t| t + conn.rto());
                    deadline = deadline.min(due.max(now));
                }
            }
        }
        for p in &pending_closes {
            let due = p.last_send.map_or(now, |t| t + conn.rto());
            deadline = deadline.min(due.max(now));
        }
        let timeout = deadline.saturating_sub(now).min(1000) as i32;

        let mut fds = vec![
            util::pollfd(listener.as_raw_fd(), libc::POLLIN),
            util::pollfd(conn.raw_fd(), libc::POLLIN),
        ];
        // IPC conns occupy 2..2+n_ipc; accepts below push AFTER them, so this
        // iteration's fd<->conn index mapping stays stable (daemon.rs pattern).
        let n_ipc = conns.len();
        for c in &conns {
            fds.push(util::pollfd(c.stream.as_raw_fd(), libc::POLLIN));
        }
        let agent_base = fds.len();
        fds.extend_from_slice(&proxy.pollfds());

        match util::poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                util::log_write("error", &format!("mux poll failed: {e}"));
                break;
            }
        }
        let now = now_ms();

        // New IPC connections.
        if fds[0].revents & libc::POLLIN != 0 {
            while let Ok((stream, _)) = listener.accept() {
                let _ = stream.set_nonblocking(true);
                conns.push(IpcConn::with_id(stream, next_conn_id));
                next_conn_id += 1;
            }
        }

        // Enveloped datagrams: agent-kind instructions dispatch through the
        // channel mux; session-kind carries nothing here (the daemon IS the
        // client — the remote sends no session frames) but any admitted
        // instruction is authentic peer activity.
        if fds[1].revents & libc::POLLIN != 0 {
            loop {
                match conn.recv() {
                    Ok(Some(payload)) => {
                        let Ok(frag) = sync::Fragment::from_bytes(&payload) else {
                            continue;
                        };
                        let Some(assembled) = assembly.add(frag) else {
                            continue;
                        };
                        let Some((chan, message)) =
                            channel::open_any_instruction(true, &assembled)
                        else {
                            util::log_write(
                                "warn",
                                "mux: discarded instruction: bad envelope or foreign channel",
                            );
                            continue;
                        };
                        last_heard = now_ms();
                        if recv_err_logged {
                            recv_err_logged = false;
                            util::log_write("info", "mux wire recv recovered");
                        }
                        if chan.kind() != channel::KIND_AGENT {
                            // RFC 0013 §3: the remote answers our heartbeat
                            // ident request with an Empty frame on the
                            // reserved channel (which otherwise carries
                            // nothing daemon-ward).
                            if chan == channel::SESSION_CHANNEL && remote_ident.is_none() {
                                if let Ok(f) = sync::ServerFrame::decode(message) {
                                    if let Some(cap) =
                                        caps::find(&f.caps, caps::CAP_SERVER_IDENT)
                                    {
                                        if let Ok(id) = caps::decode_server_ident(&cap.payload)
                                        {
                                            util::log_write(
                                                "info",
                                                &format!(
                                                    "remote endpoint: posh {} ({})",
                                                    id.version, id.git_sha
                                                ),
                                            );
                                            remote_ident = Some(id);
                                        }
                                    }
                                }
                            }
                            // M2 session channels (ordinal >= 2, the wire
                            // micro-envelope); ordinal 1 stays the bare
                            // heartbeat stream and is ignored as before.
                            if chan.kind() == channel::KIND_SESSION
                                && chan != channel::SESSION_CHANNEL
                            {
                                let srtt = conn.srtt() as u32;
                                let Some(owner) = routes.get(&chan).copied() else {
                                    continue; // straggler on a closed channel
                                };
                                let Some(ci) =
                                    conns.iter_mut().find(|c| c.conn_id == owner)
                                else {
                                    continue;
                                };
                                let Some(sess) = ci.session.as_mut() else {
                                    continue;
                                };
                                // Any inbound on the identifier proves our
                                // OPEN arrived (§3.3): flush what queued.
                                if !sess.confirmed {
                                    sess.confirmed = true;
                                    let chan = sess.chan;
                                    for m in std::mem::take(&mut sess.queued) {
                                        send_session_wire(
                                            &mut conn,
                                            &mut fragmenter,
                                            chan,
                                            SESSION_WIRE_DATA,
                                            &m,
                                        );
                                    }
                                }
                                match message.first() {
                                    Some(&SESSION_WIRE_DATA) => {
                                        let framed =
                                            encode_session_frame(srtt, &message[1..]);
                                        let _ = send_mux_frame(
                                            ci,
                                            MuxTag::SessionFrame,
                                            &framed,
                                        );
                                    }
                                    Some(&SESSION_WIRE_CLOSE) => {
                                        let close = MuxSessionClose {
                                            remote: true,
                                            payload: message[1..].to_vec(),
                                        };
                                        let _ = send_mux_frame(
                                            ci,
                                            MuxTag::SessionClose,
                                            &close.encode(),
                                        );
                                        ci.session = None;
                                        routes.remove(&chan);
                                        if ci.holds_ref {
                                            ci.holds_ref = false;
                                            let was = state.serviceable();
                                            state.unref(now_ms());
                                            log_ref_change(
                                                "-wire-close",
                                                ci.peer_pid,
                                                &state,
                                            );
                                            if was && !state.serviceable() {
                                                agent_mux
                                                    .queue_records(&proxy.close_all());
                                            }
                                        }
                                    }
                                    // Unknown micro-kind: forward compat.
                                    _ => {}
                                }
                            }
                            continue;
                        }
                        let recs = agent_mux.on_instruction(chan, message);
                        if state.serviceable() {
                            let replies = proxy.apply_records(&recs);
                            agent_mux.queue_records(&replies);
                        } else {
                            // FDR 0014 M1 policy: refs == 0 ⇒ agent service
                            // off — answer every OPEN with FAIL and hand
                            // nothing to the local agent.
                            let fails: Vec<AgentRecord> = recs
                                .iter()
                                .filter(|r| r.kind == RecordKind::Open)
                                .map(|r| AgentRecord {
                                    channel: r.channel,
                                    kind: RecordKind::Fail,
                                    payload: Vec::new(),
                                })
                                .collect();
                            if !fails.is_empty() {
                                agent_mux.queue_records(&fails);
                            }
                        }
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        if !recv_err_logged {
                            recv_err_logged = true;
                            util::log_write(
                                "warn",
                                &format!("mux wire recv error: {e} (remote endpoint gone?)"),
                            );
                        }
                        break;
                    }
                }
            }
        }

        // Local-agent reply bytes onto the per-channel outboxes (one
        // signalled fd drives the whole sweep, as in the sibling loops).
        if (agent_base..fds.len())
            .any(|i| fds[i].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
        {
            agent_mux.queue_records(&proxy.read_channels());
        }

        // IPC traffic on the polled prefix; walk backwards so removal is
        // safe. A departing conn auto-unrefs; the unref-to-zero edge closes
        // every open agent channel (the M1 exposure bound — linger keeps the
        // CONNECTION, never agent service).
        let ctx = MuxStatusCtx {
            key,
            conn_state: MuxConnState::Connected,
            peer: conn.remote(),
            heard_age_ms: now.saturating_sub(last_heard),
            channels: proxy.live_channel_count(),
            agent_source,
            congestion: agent_mux.congestion_summary(),
            remote_ident: remote_ident.as_ref(),
        };
        // The FDR 0007 on-demand dump, mux-daemon shape: the same line
        // `posh mux ls` reports, appended to the daemon's own log. NOTE:
        // heard= grows without bound on a healthy IDLE M1 connection (the
        // remote sends nothing unprompted); the recv-error lines above are
        // the remote-death signal, not this age.
        if util::take_flag(&util::SIGUSR2_RECEIVED) {
            util::log_write("status", &status_line(&ctx, &state));
        }
        let mut i = conns.len().min(n_ipc);
        while i > 0 {
            i -= 1;
            if fds[2 + i].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
                continue;
            }
            verbs.clear();
            if !process_ipc_conn(&mut conns[i], &mut state, &ctx, &mut verbs) {
                let dead = conns.remove(i);
                // A departing conn's open channel owes the wire a close.
                if let Some(s) = dead.session.as_ref() {
                    routes.remove(&s.chan);
                    pending_closes.push(PendingClose {
                        chan: s.chan,
                        payload: Vec::new(),
                        sends: 0,
                        last_send: None,
                    });
                }
                let was_serviceable = state.serviceable();
                drop_ipc_conn(&dead, &mut state, now);
                if was_serviceable && !state.serviceable() {
                    agent_mux.queue_records(&proxy.close_all());
                }
                continue;
            }
            // Apply the M2 session verbs this conn produced.
            for verb in verbs.drain(..) {
                match verb {
                    MuxSessionVerb::Open(target) => {
                        if let Some(s) = &conns[i].session {
                            // Idempotent re-open: re-ack the same grant so a
                            // retrying client never hangs.
                            let ack = MuxSessionOpenAck::Granted {
                                ordinal: s.chan.ordinal(),
                            };
                            let _ = send_mux_frame(
                                &mut conns[i],
                                MuxTag::SessionOpenAck,
                                &ack.encode(),
                            );
                            continue;
                        }
                        let live = conns.iter().filter(|c| c.session.is_some()).count();
                        if live >= MAX_LOCAL_SESSION_CHANNELS {
                            let ack = MuxSessionOpenAck::Refused {
                                reason: "session channel table full".into(),
                            };
                            let _ = send_mux_frame(
                                &mut conns[i],
                                MuxTag::SessionOpenAck,
                                &ack.encode(),
                            );
                            continue;
                        }
                        let chan = alloc.next(channel::KIND_SESSION);
                        routes.insert(chan, conns[i].conn_id);
                        if !conns[i].holds_ref {
                            conns[i].holds_ref = true;
                            state.add_ref();
                            log_ref_change("+session-open", conns[i].peer_pid, &state);
                        }
                        conns[i].session = Some(IpcSession {
                            chan,
                            target,
                            confirmed: false,
                            queued: Vec::new(),
                            open_sends: 0,
                            last_open_send: None,
                        });
                        let ack = MuxSessionOpenAck::Granted { ordinal: chan.ordinal() };
                        let _ = send_mux_frame(
                            &mut conns[i],
                            MuxTag::SessionOpenAck,
                            &ack.encode(),
                        );
                        // The wire OPEN goes out in this iteration's session
                        // send pass below.
                    }
                    MuxSessionVerb::Msg(bytes) => {
                        if let Some(s) = conns[i].session.as_mut() {
                            if s.confirmed {
                                send_session_wire(
                                    &mut conn,
                                    &mut fragmenter,
                                    s.chan,
                                    SESSION_WIRE_DATA,
                                    &bytes,
                                );
                            } else {
                                s.queued.push(bytes);
                            }
                        }
                    }
                    MuxSessionVerb::Close(payload) => {
                        if let Some(s) = conns[i].session.take() {
                            routes.remove(&s.chan);
                            pending_closes.push(PendingClose {
                                chan: s.chan,
                                payload,
                                sends: 0,
                                last_send: None,
                            });
                            if conns[i].holds_ref {
                                conns[i].holds_ref = false;
                                let was = state.serviceable();
                                state.unref(now);
                                log_ref_change("-ipc-close", conns[i].peer_pid, &state);
                                if was && !state.serviceable() {
                                    agent_mux.queue_records(&proxy.close_all());
                                }
                            }
                        }
                    }
                }
            }
        }

        // Linger expiry — checked AFTER IPC processing so a ref queued on a
        // just-accepted conn lands before the exit decision (the zero-linger
        // spawner race).
        if state.should_exit(now) {
            util::log_write("info", "mux daemon: linger expired");
            break;
        }

        // Sends, §4.1 order: session-channel maintenance first (open
        // retransmits until confirmed, pending closes on the RTO cadence),
        // then the heartbeat, then agent instructions.
        let now = now_ms();
        let mut expired: Vec<u64> = Vec::new();
        for c in &mut conns {
            if let Some(s) = c.session.as_mut() {
                if !s.confirmed {
                    if s.open_sends >= SESSION_OPEN_RETRANSMITS_MAX {
                        // Exhausted: the remote never answered (dead peer,
                        // or a pre-M2 remote that ignores session
                        // channels). Surface the failure instead of
                        // retransmitting-then-hanging forever.
                        expired.push(c.conn_id);
                        continue;
                    }
                    if s.last_open_send
                        .is_none_or(|t| now.saturating_sub(t) >= conn.rto())
                    {
                        s.open_sends += 1;
                        s.last_open_send = Some(now);
                        let (chan, target) = (s.chan, s.target.clone());
                        send_session_wire(
                            &mut conn,
                            &mut fragmenter,
                            chan,
                            SESSION_WIRE_OPEN,
                            &target,
                        );
                    }
                }
            }
        }
        // Exhausted opens: tell the client (SessionClose ⇒ its fallback
        // cue), free the channel slot and ref, and owe the wire a CLOSE in
        // case the remote half-opened. Exactly the wire-CLOSE teardown,
        // driven by timeout instead of an answer.
        for id in expired {
            let Some(c) = conns.iter_mut().find(|c| c.conn_id == id) else {
                continue;
            };
            let Some(s) = c.session.take() else { continue };
            routes.remove(&s.chan);
            pending_closes.push(PendingClose {
                chan: s.chan,
                payload: Vec::new(),
                sends: 0,
                last_send: None,
            });
            let close = MuxSessionClose {
                remote: true,
                payload: b"session open timed out (no answer from the remote peer)".to_vec(),
            };
            let _ = send_mux_frame(c, MuxTag::SessionClose, &close.encode());
            util::log_write(
                "warn",
                "session open timed out (no answer from the remote peer)",
            );
            if c.holds_ref {
                c.holds_ref = false;
                let was = state.serviceable();
                state.unref(now);
                log_ref_change("-open-timeout", c.peer_pid, &state);
                if was && !state.serviceable() {
                    agent_mux.queue_records(&proxy.close_all());
                }
            }
        }
        pending_closes.retain_mut(|p| {
            if p.last_send
                .is_none_or(|t| now.saturating_sub(t) >= conn.rto())
            {
                p.sends += 1;
                p.last_send = Some(now);
                send_session_wire(
                    &mut conn,
                    &mut fragmenter,
                    p.chan,
                    SESSION_WIRE_CLOSE,
                    &p.payload,
                );
            }
            p.sends < SESSION_CLOSE_RETRANSMITS
        });
        let session_due = last_send.is_none_or(|t| now.saturating_sub(t) >= HEARTBEAT_INTERVAL);
        let session = session_due.then(|| heartbeat_message(remote_ident.is_none()));
        if session.is_some() {
            last_send = Some(now);
        }
        for (chan, payload) in
            crate::remote::agent::iteration_sends(session, Some(&mut agent_mux), now, conn.rto())
        {
            let wire = channel::seal_on(true, chan, &payload);
            for frag in fragmenter.make_fragments(&wire, sync::FRAGMENT_CONTENTS_MAX) {
                let _ = conn.send(&frag.to_bytes());
            }
        }
    }
}

/// Seals one M2 session-channel instruction — the 1-byte wire micro-envelope
/// kind + body — and sends its fragments. The single seam every session
/// send goes through, on BOTH ends of the connection (the local daemon here,
/// the remote channel-table peer in `server.rs`).
pub(crate) fn send_session_wire(
    conn: &mut Connection,
    fragmenter: &mut sync::Fragmenter,
    chan: channel::ChannelId,
    kind: u8,
    body: &[u8],
) {
    let mut payload = Vec::with_capacity(1 + body.len());
    payload.push(kind);
    payload.extend_from_slice(body);
    crate::remote::server::send_on_channel(conn, fragmenter, chan, &payload, true);
}

// ---------------------------------------------------------------------------
// The client half (M1 Task 4, design doc "Lifecycle" spawn): ensure the
// endpoint for a destination — connect-or-spawn, the §6 hello handshake with
// the variant-socket retry, then hold the invocation's session ref.

/// How long the client waits for the daemon's `MuxHelloAck`. A freshly
/// spawned daemon answers only after its ssh bootstrap completes, so this
/// bounds the stall a cold spawn can impose on an invocation before the
/// per-connection fallback kicks in; a warm endpoint answers at once.
const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Bounded connect-retry budget after a spawn. The socket is bound before
/// [`run_daemon`] forks (and a lost race means the winner's bind already
/// happened or is imminent), so the endpoint is normally connectable at
/// once — the retries cover the narrow window where a racing winner is a
/// beat away from `bind`, or has just reclaimed a stale socket.
const CONNECT_ATTEMPTS: u32 = 20;
const CONNECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

/// [`bind_or_probe`]'s bounded liveness retry for a CONTENDED socket: a
/// cold-start loser that finds a node already at the path retries the connect
/// probe before concluding the socket is dead. A live winner (bound, so
/// connectable via the listen backlog even before it reaches `accept`) answers
/// within a beat; a genuine crash leftover never does. This closes the
/// concurrent cold-start window where the loser would otherwise clobber the
/// winner's socket and spawn a duplicate daemon. The budget (~50ms worst case)
/// stays well under the loser's own post-spawn `CONNECT_ATTEMPTS` backoff.
const PROBE_ATTEMPTS: u32 = 10;
const PROBE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

/// The client's live claim on a mux endpoint: holds the IPC connection
/// carrying this invocation's `MuxSessionRef` open for the invocation's
/// lifetime. Drop closes the socket, which IS the unref (daemon-side
/// auto-unref on disconnect) — there is no explicit release verb, so a
/// crashed client can never pin the refcount.
pub struct MuxHandle {
    /// The IPC connection. Idle after the handshake on the agent-only (M1)
    /// path — closing it is the unref — and the session transport on the
    /// M2 path after [`open_session`](Self::open_session).
    conn: UnixStream,
    /// Frame reassembly for the post-handshake protocol (M2 session
    /// traffic); carries any bytes read past the acks.
    buf: MuxFrameBuffer,
    state: MuxConnState,
    key: String,
    /// `Some(daemon's source)` when the endpoint forwards a DIFFERENT local
    /// agent than this invocation resolved — the daemon keeps its own (it
    /// inherited its spawner's resolution; restarting it is the only way to
    /// change), and the caller warns instead of silently diverging.
    source_mismatch: Option<PathBuf>,
}

impl MuxHandle {
    /// The endpoint's connection state as reported by its `MuxHelloAck`.
    pub fn state(&self) -> MuxConnState {
        self.state
    }

    /// The destination key the endpoint reported (the variant key when the
    /// §6 mismatch path was taken).
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The daemon's agent-source path when it differs from the source this
    /// invocation resolved; `None` when they agree. The endpoint keeps
    /// forwarding ITS source either way — the caller's job is to warn.
    pub fn source_mismatch(&self) -> Option<&Path> {
        self.source_mismatch.as_deref()
    }

    /// M2: opens this invocation's session channel to `target` over the
    /// handle's IPC connection, consuming the handle into the
    /// [`MuxSessionTransport`] the client loop drives. A refusal or timeout
    /// is an `Err` — the caller's cue to fall back to a per-invocation
    /// connection (the handle is gone either way; the fallback path re-runs
    /// `apply_mux_gate` semantics on its own connection).
    pub fn open_session(mut self, target: &str) -> Result<MuxSessionTransport> {
        use std::io::Write;
        self.conn
            .write_all(&encode_mux_frame(MuxTag::SessionOpen, target.as_bytes()))?;
        let frame = await_frame(
            &mut self.conn,
            &mut self.buf,
            HELLO_TIMEOUT,
            MuxTag::SessionOpenAck,
        )?;
        match MuxSessionOpenAck::decode(&frame.payload) {
            Some(MuxSessionOpenAck::Granted { .. }) => {
                self.conn.set_nonblocking(true)?;
                Ok(MuxSessionTransport {
                    handle: self,
                    srtt_ms: 0,
                    bytes_tx: 0,
                    bytes_rx: 0,
                    established: false,
                })
            }
            Some(MuxSessionOpenAck::Refused { reason }) => {
                Err(util::Error::Msg(format!("mux session refused: {reason}")))
            }
            None => Err(util::Error::from("mux session ack malformed")),
        }
    }
}

/// What one [`MuxSessionTransport::next_event`] poll surfaced.
#[derive(Debug, PartialEq, Eq)]
pub enum MuxSessionEvent {
    /// One whole encoded `ServerFrame` (the srtt hint already consumed).
    Frame(Vec<u8>),
    /// The channel closed (remote daemon exit / endpoint teardown); the
    /// payload is the exit-status path's bytes, possibly empty.
    Closed(Vec<u8>),
}

/// The M2 client-side transport: whole assembled messages over the mux IPC
/// socket, in place of a per-invocation UDP connection. Pacing numbers come
/// from the srtt hint each `SessionFrame` carries (the foreground process no
/// longer owns a socket to measure), shaped by the same clamps as
/// `datagram.rs` so the prediction engine and send cadence behave
/// identically.
pub struct MuxSessionTransport {
    handle: MuxHandle,
    srtt_ms: u32,
    bytes_tx: u64,
    bytes_rx: u64,
    /// At least one frame arrived: the channel reached the remote daemon.
    /// A close BEFORE this is a failed establishment (remote refusal, dead
    /// peer, open timeout) — the client's per-invocation fallback cue, as
    /// opposed to a genuine session ending.
    established: bool,
}

impl MuxSessionTransport {
    pub fn raw_fd(&self) -> std::os::unix::io::RawFd {
        self.handle.conn.as_raw_fd()
    }

    /// Sends one encoded `ClientMessage` as a `SessionMsg` frame.
    pub fn send_msg(&mut self, message: &[u8]) {
        use std::io::Write;
        let frame = encode_mux_frame(MuxTag::SessionMsg, message);
        self.bytes_tx += frame.len() as u64;
        let _ = self.handle.conn.write_all(&frame);
    }

    /// Drains the (non-blocking) socket and returns the next event, `None`
    /// when the socket runs dry. A dead daemon surfaces as `Closed`.
    pub fn next_event(&mut self) -> Option<MuxSessionEvent> {
        use std::io::Read;
        loop {
            match self.handle.buf.next() {
                Ok(Some(frame)) => match frame.tag {
                    MuxTag::SessionFrame => {
                        if let Some((srtt, body)) = decode_session_frame(&frame.payload) {
                            self.srtt_ms = srtt;
                            self.established = true;
                            return Some(MuxSessionEvent::Frame(body.to_vec()));
                        }
                    }
                    MuxTag::SessionClose => {
                        let payload = MuxSessionClose::decode(&frame.payload)
                            .map(|c| c.payload)
                            .unwrap_or_default();
                        return Some(MuxSessionEvent::Closed(payload));
                    }
                    _ => {}
                },
                Ok(None) => {}
                Err(_) => return Some(MuxSessionEvent::Closed(Vec::new())),
            }
            let mut tmp = [0u8; 4096];
            match self.handle.conn.read(&mut tmp) {
                Ok(0) => return Some(MuxSessionEvent::Closed(Vec::new())),
                Ok(n) => {
                    self.bytes_rx += n as u64;
                    self.handle.buf.feed(&tmp[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Some(MuxSessionEvent::Closed(Vec::new())),
            }
        }
    }

    /// Whether any frame ever arrived — a close before this is a failed
    /// establishment, not a session ending.
    pub fn established(&self) -> bool {
        self.established
    }

    /// The connection's smoothed RTT as last hinted by a frame (ms).
    pub fn srtt(&self) -> f64 {
        self.srtt_ms as f64
    }

    /// The RTO analog, `datagram.rs`'s clamp shape over the hinted srtt.
    pub fn rto(&self) -> u64 {
        ((self.srtt_ms as u64).saturating_mul(2)).clamp(50, 1000)
    }

    /// mosh's send interval over the hinted srtt (`datagram.rs` clamps).
    pub fn send_interval(&self) -> u64 {
        ((self.srtt_ms as f64 / 2.0).ceil() as u64).clamp(20, 250)
    }

    pub fn bytes_tx(&self) -> u64 {
        self.bytes_tx
    }

    pub fn bytes_rx(&self) -> u64 {
        self.bytes_rx
    }
}

/// `[user@]host` split at the LAST `@` (ssh's rule — the same byte
/// [`bootstrap`](crate::remote::sshwrap::bootstrap)'s fallback host uses).
/// An empty user (`@host`) counts as absent.
fn split_dest(dest: &str) -> (Option<&str>, &str) {
    match dest.rsplit_once('@') {
        Some(("", host)) => (None, host),
        Some((user, host)) => (Some(user), host),
        None => (None, dest),
    }
}

/// The §6 stamp-mismatch socket variant: `<key>.<ver>`, with the stamp
/// rendered slug-safe so the variant stays a single path component. A client
/// finding an endpoint on a different stamp starts a fresh daemon here and
/// leaves the old one to drain — never negotiates down.
fn variant_key(key: &str) -> String {
    format!("{key}.{}", sanitize_id(MUX_PROTO_STAMP))
}

/// Ensure a live, stamp-matching mux endpoint for ssh destination `dest`
/// (`[user@]host`) and claim a session ref on it: computes the destination
/// key, connects to `mux/<key>.sock` (spawning [`run_daemon`] when absent or
/// stale; a lost spawn race defers to the winner), performs the hello
/// handshake — a stamp mismatch retries once on the `<key>.<ver>` variant
/// socket — and sends `MuxSessionRef`. The returned [`MuxHandle`] must be
/// held for the invocation's lifetime; dropping it is the unref.
///
/// `agent_source` is the invocation's FDR 0004-resolved local agent socket,
/// inherited by a daemon this call spawns (design doc "Security").
pub fn ensure_mux(
    dest: &str,
    family: Family,
    port_range: Option<&str>,
    agent_source: &Path,
) -> Result<MuxHandle> {
    let (user, host) = split_dest(dest);
    let key = dest_key(user, host, family, port_range);
    let dir = mux_dir()?;
    let mut spawn = |k: &str| {
        run_daemon(
            k,
            dest,
            family,
            port_range.map(str::to_string),
            agent_source.to_path_buf(),
        )
    };
    let handle = ensure_mux_conn(&dir, &key, &mut spawn, HELLO_TIMEOUT, agent_source)?;
    if let Some(theirs) = handle.source_mismatch() {
        eprintln!(
            "posh: mux endpoint {} forwards {}; restart it to change (this \
             invocation resolved {})",
            handle.key(),
            theirs.display(),
            agent_source.display()
        );
    }
    Ok(handle)
}

/// The seam behind [`ensure_mux`]: explicit mux dir, spawn action, and the
/// invocation's resolved local agent source (compared against the ack's for
/// [`MuxHandle::source_mismatch`]), so the whole
/// connect/spawn/hello/variant/ref ladder is tested in-process against a
/// [`mux_loop`] thread instead of a forked daemon.
fn ensure_mux_conn(
    dir: &Path,
    key: &str,
    spawn: &mut dyn FnMut(&str) -> Result<MuxSpawn>,
    hello_timeout: std::time::Duration,
    local_source: &Path,
) -> Result<MuxHandle> {
    let (stream, buf, ack) = connect_and_hello(dir, key, spawn, hello_timeout)?;
    if ack.stamp == MUX_PROTO_STAMP {
        return claim_ref(stream, buf, ack, hello_timeout, local_source);
    }
    // RFC 0011 §6: never negotiate down. The endpoint told us ITS stamp; we
    // start a fresh daemon on the variant socket and let the old one drain.
    let variant = variant_key(key);
    util::log_write(
        "warn",
        &format!(
            "mux endpoint {key} speaks {:?} (ours: {MUX_PROTO_STAMP}); starting variant {variant}",
            ack.stamp
        ),
    );
    let (stream, buf, ack) = connect_and_hello(dir, &variant, spawn, hello_timeout)?;
    if ack.stamp != MUX_PROTO_STAMP {
        return Err(util::Error::Msg(format!(
            "mux variant endpoint {variant} still speaks {:?} (ours: {MUX_PROTO_STAMP})",
            ack.stamp
        )));
    }
    claim_ref(stream, buf, ack, hello_timeout, local_source)
}

/// One connect-or-spawn + hello round for a single socket name. A failed
/// connect (absent, refused, stale) triggers exactly one spawn — whose
/// `AlreadyRunning`/`Spawned` outcomes both mean "a live endpoint is
/// imminent" — followed by the bounded retry-connect.
fn connect_and_hello(
    dir: &Path,
    key: &str,
    spawn: &mut dyn FnMut(&str) -> Result<MuxSpawn>,
    hello_timeout: std::time::Duration,
) -> Result<(UnixStream, MuxFrameBuffer, MuxHelloAck)> {
    let path = mux_socket_path_in(dir, key);
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(_) => {
            spawn(key)?;
            retry_connect(&path)?
        }
    };
    let (buf, ack) = hello_handshake(&mut stream, hello_timeout)?;
    Ok((stream, buf, ack))
}

/// The bounded post-spawn backoff: [`CONNECT_ATTEMPTS`] tries,
/// [`CONNECT_RETRY_DELAY`] apart.
fn retry_connect(path: &Path) -> Result<UnixStream> {
    let mut last = None;
    for _ in 0..CONNECT_ATTEMPTS {
        match UnixStream::connect(path) {
            Ok(s) => return Ok(s),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(CONNECT_RETRY_DELAY);
            }
        }
    }
    Err(util::Error::Msg(format!(
        "mux endpoint never became connectable at {}: {}",
        path.display(),
        last.expect("CONNECT_ATTEMPTS > 0")
    )))
}

/// Reads frames (blocking, deadline via the socket read timeout) until one
/// with tag `want` arrives; other frames are skipped, mirroring the daemon's
/// forward-compatibility posture. The shared wait behind the hello ack and
/// the ref ack: any close, error, or timeout is an `Err`, never a silent
/// success. `buf` carries partial bytes between waits on the same stream.
fn await_frame(
    stream: &mut UnixStream,
    buf: &mut MuxFrameBuffer,
    timeout: std::time::Duration,
    want: MuxTag,
) -> Result<MuxFrame> {
    use std::io::Read;
    stream.set_read_timeout(Some(timeout))?;
    loop {
        while let Some(frame) = buf.next()? {
            if frame.tag == want {
                return Ok(frame);
            }
        }
        let mut tmp = [0u8; 256];
        match stream.read(&mut tmp) {
            Ok(0) => {
                return Err(util::Error::Msg(format!(
                    "mux endpoint closed while awaiting {want:?}"
                )))
            }
            Ok(n) => buf.feed(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(util::Error::Msg(format!("mux {want:?} read: {e}"))),
        }
    }
}

/// Sends `MuxHello` and awaits the `MuxHelloAck`. Returns the frame buffer
/// alongside the ack so a follow-up wait on the same stream (the ref ack)
/// never loses bytes already read past the hello.
fn hello_handshake(
    stream: &mut UnixStream,
    timeout: std::time::Duration,
) -> Result<(MuxFrameBuffer, MuxHelloAck)> {
    use std::io::Write;
    let hello = MuxHello {
        pid: std::process::id(),
        stamp: MUX_PROTO_STAMP.to_string(),
    };
    stream.write_all(&encode_mux_frame(MuxTag::Hello, &hello.encode()))?;
    let mut buf = MuxFrameBuffer::default();
    let frame = await_frame(stream, &mut buf, timeout, MuxTag::HelloAck)?;
    let ack = MuxHelloAck::decode(&frame.payload)
        .ok_or_else(|| util::Error::from("malformed mux hello ack"))?;
    Ok((buf, ack))
}

/// The `posh mux ls` probe bound: a live daemon answers hello + Status in
/// milliseconds over the unix socket; anything slower is effectively dead.
const LS_STATUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// What [`mux_ls`] returns when no endpoint sockets exist — exported so the
/// unified `posh list` view (#158) can suppress its mux section without
/// string-matching a copy of this text.
pub const MUX_LS_EMPTY: &str = "no mux endpoints\n";

/// The #156 soak instrument: one status line per endpoint socket under the
/// mux dir. See [`mux_ls_in`].
pub fn mux_ls() -> Result<String> {
    mux_ls_in(&mux_dir()?)
}

/// Enumerates `<dir>/*.sock` (sorted; non-`.sock` entries — pidfiles, logs
/// — are skipped) and probes each: a live daemon answers hello + `Status`
/// on the OBSERVER path (no ref taken, the linger clock untouched) and its
/// own one-liner is printed verbatim; a §6 stamp mismatch is labeled an
/// old-generation daemon rather than probed further; a socket nothing
/// answers on is flagged stale (a daemon that died without unlinking).
fn mux_ls_in(dir: &Path) -> Result<String> {
    let mut keys: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| util::Error::Msg(format!("mux dir {}: {e}", dir.display())))?
        .filter_map(|ent| {
            let name = ent.ok()?.file_name().into_string().ok()?;
            name.strip_suffix(".sock").map(str::to_string)
        })
        .collect();
    keys.sort();
    if keys.is_empty() {
        return Ok(MUX_LS_EMPTY.to_string());
    }
    let mut out = String::new();
    for key in keys {
        match probe_endpoint(&mux_socket_path_in(dir, &key)) {
            Ok(line) => out.push_str(&line),
            Err(e) => out.push_str(&format!("mux {key}: stale ({e})")),
        }
        out.push('\n');
    }
    Ok(out)
}

/// Hello + `Status` against one endpoint socket, bounded by
/// [`LS_STATUS_TIMEOUT`]; returns the daemon's own status one-liner.
fn probe_endpoint(path: &Path) -> Result<String> {
    use std::io::Write;
    let mut s = UnixStream::connect(path)?;
    let (mut buf, ack) = hello_handshake(&mut s, LS_STATUS_TIMEOUT)?;
    if ack.stamp != MUX_PROTO_STAMP {
        return Ok(format!(
            "mux {}: old-generation daemon (stamp {}, ours {})",
            ack.key, ack.stamp, MUX_PROTO_STAMP
        ));
    }
    s.write_all(&encode_mux_frame(MuxTag::Status, b""))?;
    let frame = await_frame(&mut s, &mut buf, LS_STATUS_TIMEOUT, MuxTag::StatusReply)?;
    Ok(String::from_utf8_lossy(&frame.payload).into_owned())
}

/// The invocation-seam gate (M1 Task 4.3): decides who owns agent
/// forwarding for a remote invocation. Off, or with forwarding already
/// resolved off, it is a pass-through — the construction sites see exactly
/// what FDR 0004 resolution produced and no endpoint is touched. On, with a
/// resolved source, `ensure` runs BEFORE the session bootstrap: success
/// moves ownership to the endpoint (session `agent_source` becomes `None`,
/// so `remote_command` carries no `-A` and no per-session `srv-<pid>`
/// endpoint exists) and the returned handle must be held for the
/// invocation's lifetime; ANY failure warns once and falls back to
/// per-connection forwarding exactly as today — never strand the user
/// agentless.
pub fn apply_mux_gate(
    selected: bool,
    agent_source: Option<PathBuf>,
    ensure: impl FnOnce(&Path) -> Result<MuxHandle>,
) -> (Option<PathBuf>, Option<MuxHandle>) {
    if !selected {
        return (agent_source, None);
    }
    let Some(source) = agent_source else {
        // Forwarding resolved off: nothing for the endpoint to own — the
        // mux exists to carry agent forwarding, so no spawn at all.
        return (None, None);
    };
    match ensure(&source) {
        Ok(handle) => {
            util::log_write(
                "info",
                &format!(
                    "mux endpoint {} {}; session forwarding off",
                    handle.key(),
                    handle.state().label()
                ),
            );
            (None, Some(handle))
        }
        Err(e) => {
            eprintln!(
                "posh: mux endpoint unavailable ({e}); falling back to per-connection agent forwarding"
            );
            (Some(source), None)
        }
    }
}

/// Sends the invocation's `MuxSessionRef` and BLOCKS (bounded, the hello
/// machinery) for the daemon's `RefAck` before wrapping the connection: only
/// a confirmed ref lets the caller surrender per-connection forwarding — a
/// daemon that dies, hangs, or closes pre-ack is an `Err`, which
/// [`apply_mux_gate`]'s fallback turns back into per-connection forwarding.
/// From the ack on, the open socket IS the ref.
fn claim_ref(
    mut stream: UnixStream,
    mut buf: MuxFrameBuffer,
    ack: MuxHelloAck,
    timeout: std::time::Duration,
    local_source: &Path,
) -> Result<MuxHandle> {
    use std::io::Write;
    stream.write_all(&encode_mux_frame(MuxTag::SessionRef, b""))?;
    await_frame(&mut stream, &mut buf, timeout, MuxTag::RefAck)?;
    let source_mismatch = (ack.source != local_source).then_some(ack.source);
    Ok(MuxHandle {
        conn: stream,
        buf,
        state: ack.state,
        key: ack.key,
        source_mismatch,
    })
}

/// The local hostname via gethostname(2); `"unknown"` when the call fails or
/// reports an empty name, so [`client_id`] never yields an empty id.
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into a valid,
    // exclusively held buffer; no pointers escape the call.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc != 0 {
        return "unknown".to_string();
    }
    // POSIX leaves NUL-termination unspecified on truncation; force one so
    // the scan below is bounded either way.
    buf[buf.len() - 1] = 0;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
    let name = String::from_utf8_lossy(&buf[..end]);
    if name.is_empty() {
        "unknown".to_string()
    } else {
        name.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- MuxState: the refcount/linger machine (M1 Task 3.1), virtual time ---

    #[test]
    fn refs_gate_serviceability_exactly() {
        let mut st = MuxState::new(60_000, 0);
        assert!(!st.serviceable(), "no refs at spawn: agent service off");
        st.add_ref();
        assert!(st.serviceable(), "first ref turns agent service on");
        st.add_ref();
        assert!(st.serviceable());
        st.unref(10);
        assert!(st.serviceable(), "one of two refs dropped: still serviceable");
        st.unref(20);
        assert!(!st.serviceable(), "last ref dropped: service off at once");
        st.add_ref();
        assert!(st.serviceable(), "re-ref re-enables");
    }

    #[test]
    fn unref_to_zero_starts_linger_and_expiry_signals_shutdown() {
        let mut st = MuxState::new(60_000, 0);
        st.add_ref();
        assert!(!st.should_exit(u64::MAX), "a held ref never expires");
        assert_eq!(st.next_deadline(), None, "no linger clock while referenced");
        st.unref(1_000);
        assert_eq!(st.next_deadline(), Some(61_000));
        assert!(!st.should_exit(60_999), "inside the linger window");
        assert!(st.should_exit(61_000), "linger expiry is the shutdown signal");
    }

    #[test]
    fn re_ref_during_linger_cancels_it() {
        let mut st = MuxState::new(60_000, 0);
        st.add_ref();
        st.unref(1_000);
        assert!(st.lingering());
        st.add_ref();
        assert!(!st.lingering(), "a fresh ref cancels the linger clock");
        assert!(!st.should_exit(u64::MAX));
        // The next unref restarts the window from ITS moment, not the old one.
        st.unref(500_000);
        assert!(!st.should_exit(559_999));
        assert!(st.should_exit(560_000));
    }

    #[test]
    fn zero_linger_exits_immediately_on_unref_but_not_before_spawn_grace() {
        let mut st = MuxState::new(0, 7);
        // POSH_MUX_PERSIST=0 must not exit the daemon before its spawner's
        // first ref can land: construction arms the spawn grace, and the
        // zero linger governs only the post-unref window.
        assert!(!st.should_exit(7), "the spawn grace outlives a zero linger");
        assert!(!st.should_exit(7 + SPAWN_GRACE_MS - 1));
        assert!(st.should_exit(7 + SPAWN_GRACE_MS), "an orphan still exits");
        st.add_ref();
        assert!(!st.should_exit(u64::MAX));
        st.unref(42);
        assert!(st.should_exit(42), "POSH_MUX_PERSIST=0: no linger after unref");
    }

    #[test]
    fn spawn_starts_an_orphan_linger_clock() {
        // A daemon whose spawner dies before ever connecting must not idle
        // forever: the linger clock is armed from construction, and the first
        // ref (the spawner's, in the normal path) cancels it.
        let st = MuxState::new(60_000, 100);
        assert!(!st.should_exit(60_099));
        assert!(st.should_exit(60_100));
    }

    #[test]
    fn ref_unref_cycles_track_refs() {
        let mut st = MuxState::new(60_000, 0);
        for round in 0..3u64 {
            st.add_ref();
            st.add_ref();
            st.unref(round);
            st.unref(round);
            assert!(!st.serviceable());
            assert!(st.lingering(), "round {round}: linger armed after each cycle");
        }
        // Unref below zero saturates rather than wrapping.
        st.unref(99);
        assert_eq!(st.refs(), 0);
    }

    // --- The mux IPC protocol (M1 Task 3.2): codecs + conn lifecycle ---

    #[test]
    fn mux_stamp_pins_rfc0011_envelope_ver() {
        // The compile-time stamp is "mux1/" + the RFC 0011 §2 envelope version
        // this build speaks; bumping VER_1 without bumping the stamp (or vice
        // versa) must fail here, since §6 keys endpoint compatibility on it.
        assert_eq!(
            MUX_PROTO_STAMP,
            format!("mux1/{}", crate::remote::channel::VER_1)
        );
    }

    #[test]
    fn mux_hello_roundtrips_and_rejects_truncation() {
        let hello = MuxHello {
            pid: 4242,
            stamp: MUX_PROTO_STAMP.to_string(),
        };
        assert_eq!(MuxHello::decode(&hello.encode()), Some(hello));
        // An empty stamp survives (it just mismatches later).
        let bare = MuxHello {
            pid: 1,
            stamp: String::new(),
        };
        assert_eq!(MuxHello::decode(&bare.encode()), Some(bare));
        assert_eq!(MuxHello::decode(b""), None);
        assert_eq!(MuxHello::decode(&[0u8; 3]), None);
    }

    #[test]
    fn mux_hello_ack_roundtrips_all_states_and_rejects_truncation() {
        for state in [
            MuxConnState::Bootstrapping,
            MuxConnState::Connected,
            MuxConnState::Draining,
        ] {
            let ack = MuxHelloAck {
                state,
                stamp: MUX_PROTO_STAMP.to_string(),
                key: "example.com-4".to_string(),
                source: PathBuf::from("/run/user/1000/agent.sock"),
            };
            assert_eq!(MuxHelloAck::decode(&ack.encode()), Some(ack));
        }
        // An empty source path survives the roundtrip (source is the
        // trailing field, so empty is representable).
        let bare = MuxHelloAck {
            state: MuxConnState::Connected,
            stamp: "s".into(),
            key: "k".into(),
            source: PathBuf::new(),
        };
        assert_eq!(MuxHelloAck::decode(&bare.encode()), Some(bare));
        assert_eq!(MuxHelloAck::decode(b""), None);
        assert_eq!(MuxHelloAck::decode(&[1u8, 9, 0]), None, "stamp_len past end");
        // key_len reaching past the payload end is rejected too.
        let mut truncated = MuxHelloAck {
            state: MuxConnState::Connected,
            stamp: "s".into(),
            key: "key".into(),
            source: PathBuf::new(),
        }
        .encode();
        truncated.truncate(truncated.len() - 2); // cut into the key
        assert_eq!(MuxHelloAck::decode(&truncated), None, "key_len past end");
        // Unknown state byte is rejected, not guessed.
        let mut wire = MuxHelloAck {
            state: MuxConnState::Connected,
            stamp: "s".into(),
            key: "k".into(),
            source: PathBuf::from("/a"),
        }
        .encode();
        wire[0] = 9;
        assert_eq!(MuxHelloAck::decode(&wire), None);
    }

    #[test]
    fn session_open_ack_roundtrips_ok_and_failure() {
        let ok = MuxSessionOpenAck::Granted { ordinal: 0x1122_3344_5566 };
        assert_eq!(MuxSessionOpenAck::decode(&ok.encode()), Some(ok.clone()));
        let no = MuxSessionOpenAck::Refused { reason: "table full".into() };
        assert_eq!(MuxSessionOpenAck::decode(&no.encode()), Some(no));
        // Truncated grant and unknown flag byte both reject.
        assert_eq!(MuxSessionOpenAck::decode(&[1, 0, 0]), None);
        assert_eq!(MuxSessionOpenAck::decode(&[9]), None);
        assert_eq!(MuxSessionOpenAck::decode(&[]), None);
    }

    #[test]
    fn session_frame_prefixes_srtt_and_keeps_body_opaque() {
        let body = b"\x00opaque server frame bytes\xff";
        let wire = encode_session_frame(137, body);
        let (srtt, out) = decode_session_frame(&wire).unwrap();
        assert_eq!(srtt, 137);
        assert_eq!(out, body);
        // A frame near the M1 4 KiB bound (raised for M2) survives framing.
        let big = vec![0xEEu8; 64 * 1024];
        let framed = encode_mux_frame(MuxTag::SessionFrame, &encode_session_frame(9, &big));
        let mut buf = MuxFrameBuffer::default();
        buf.feed(&framed);
        let f = buf.next().unwrap().unwrap();
        assert_eq!(f.tag, MuxTag::SessionFrame);
        assert_eq!(decode_session_frame(&f.payload).unwrap().1.len(), big.len());
        // Truncation rejects.
        assert_eq!(decode_session_frame(&[1, 2]), None);
    }

    #[test]
    fn session_close_roundtrips_both_origins() {
        let local = MuxSessionClose { remote: false, payload: Vec::new() };
        assert_eq!(MuxSessionClose::decode(&local.encode()), Some(local));
        let remote = MuxSessionClose { remote: true, payload: b"exit 3".to_vec() };
        assert_eq!(MuxSessionClose::decode(&remote.encode()), Some(remote));
        assert_eq!(MuxSessionClose::decode(&[]), None);
        assert_eq!(MuxSessionClose::decode(&[7]), None, "unknown origin rejects");
    }

    #[test]
    fn session_tags_roundtrip_through_the_frame_buffer() {
        // SessionOpen's payload is the bare UTF-8 target; SessionMsg is
        // opaque bytes — both ride the zmx framing unchanged, and an
        // UNKNOWN tag between them is skipped (forward compatibility).
        let mut buf = MuxFrameBuffer::default();
        buf.feed(&encode_mux_frame(MuxTag::SessionOpen, b"box:dev"));
        buf.feed(&[42, 3, 0, 0, 0, 1, 2, 3]); // unknown tag 42, skipped
        buf.feed(&encode_mux_frame(MuxTag::SessionMsg, b"\x01client message"));
        let open = buf.next().unwrap().unwrap();
        assert_eq!(open.tag, MuxTag::SessionOpen);
        assert_eq!(open.payload, b"box:dev");
        let msg = buf.next().unwrap().unwrap();
        assert_eq!(msg.tag, MuxTag::SessionMsg);
        assert_eq!(msg.payload, b"\x01client message");
        assert!(buf.next().unwrap().is_none());
    }

    #[test]
    fn mux_frame_buffer_reassembles_split_frames_and_skips_unknown_tags() {
        let mut wire = encode_mux_frame(MuxTag::Hello, b"abc");
        wire.extend_from_slice(&[0xee, 2, 0, 0, 0, b'x', b'y']); // unknown tag
        wire.extend_from_slice(&encode_mux_frame(MuxTag::SessionRef, b""));
        let mut buf = MuxFrameBuffer::default();
        // Byte-at-a-time delivery (ADR-0003): frames appear only when complete.
        for (i, b) in wire.iter().enumerate() {
            buf.feed(&[*b]);
            if i + 1 < encode_mux_frame(MuxTag::Hello, b"abc").len() {
                assert_eq!(buf.next().unwrap(), None);
            }
        }
        let first = buf.next().unwrap().unwrap();
        assert_eq!(first.tag, MuxTag::Hello);
        assert_eq!(first.payload, b"abc");
        let second = buf.next().unwrap().unwrap();
        assert_eq!(second.tag, MuxTag::SessionRef, "unknown tag skipped");
        assert!(second.payload.is_empty());
        assert_eq!(buf.next().unwrap(), None);
    }

    #[test]
    fn mux_frame_rejects_oversize_length() {
        let mut wire = vec![MuxTag::Hello as u8];
        wire.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut buf = MuxFrameBuffer::default();
        buf.feed(&wire);
        assert!(buf.next().is_err(), "a hostile length must not drive buffering");
    }

    /// A connected daemon-side conn + the client end of the socketpair.
    fn ipc_pair() -> (IpcConn, std::os::unix::net::UnixStream) {
        let (daemon_side, client_side) = std::os::unix::net::UnixStream::pair().unwrap();
        daemon_side.set_nonblocking(true).unwrap();
        (IpcConn::with_id(daemon_side, 0), client_side)
    }

    /// The agent-source path every `test_ctx` daemon reports in its ack.
    const TEST_CTX_SOURCE: &str = "/run/user/1000/test-agent.sock";

    fn test_ctx(key: &str) -> MuxStatusCtx<'_> {
        MuxStatusCtx {
            key,
            conn_state: MuxConnState::Connected,
            peer: None,
            heard_age_ms: 12,
            channels: 0,
            agent_source: Path::new(TEST_CTX_SOURCE),
            congestion: (262_144, 0, 0),
            remote_ident: None,
        }
    }

    /// Reads one mux frame off a blocking client-side stream.
    fn read_client_frame(stream: &mut std::os::unix::net::UnixStream) -> MuxFrame {
        use std::io::Read;
        let mut buf = MuxFrameBuffer::default();
        loop {
            if let Some(f) = buf.next().unwrap() {
                return f;
            }
            let mut tmp = [0u8; 256];
            let n = stream.read(&mut tmp).unwrap();
            assert!(n > 0, "conn closed before a frame arrived");
            buf.feed(&tmp[..n]);
        }
    }

    #[test]
    fn ipc_hello_ref_status_drop_lifecycle_over_socketpair() {
        use std::io::Write;
        let (mut conn, mut client) = ipc_pair();
        let mut state = MuxState::new(60_000, 0);
        let ctx = test_ctx("example.com-4");

        // Hello -> HelloAck carrying our stamp, connection state, and key.
        let hello = MuxHello {
            pid: 7,
            stamp: MUX_PROTO_STAMP.to_string(),
        };
        client
            .write_all(&encode_mux_frame(MuxTag::Hello, &hello.encode()))
            .unwrap();
        assert!(process_ipc_conn(&mut conn, &mut state, &ctx, &mut Vec::new()));
        let ack_frame = read_client_frame(&mut client);
        assert_eq!(ack_frame.tag, MuxTag::HelloAck);
        let ack = MuxHelloAck::decode(&ack_frame.payload).unwrap();
        assert_eq!(ack.stamp, MUX_PROTO_STAMP);
        assert_eq!(ack.state, MuxConnState::Connected);
        assert_eq!(ack.key, "example.com-4");
        assert_eq!(
            ack.source,
            Path::new(TEST_CTX_SOURCE),
            "the ack reports the daemon's resolved agent source"
        );
        assert!(!state.serviceable(), "hello alone holds no ref");

        // SessionRef -> serviceable, confirmed by a RefAck. One exchange per
        // step (read_client_frame buffers per call, so coalesced replies
        // would be lost across calls).
        client
            .write_all(&encode_mux_frame(MuxTag::SessionRef, b""))
            .unwrap();
        assert!(process_ipc_conn(&mut conn, &mut state, &ctx, &mut Vec::new()));
        assert_eq!(state.refs(), 1);
        assert!(state.serviceable());
        assert_eq!(read_client_frame(&mut client).tag, MuxTag::RefAck);
        // A duplicate on the same conn stays one ref — and is RE-acked, so a
        // waiting client can never hang on an idempotent ref.
        client
            .write_all(&encode_mux_frame(MuxTag::SessionRef, b""))
            .unwrap();
        assert!(process_ipc_conn(&mut conn, &mut state, &ctx, &mut Vec::new()));
        assert_eq!(state.refs(), 1, "one accepted conn = at most one ref");
        assert_eq!(
            read_client_frame(&mut client).tag,
            MuxTag::RefAck,
            "a duplicate SessionRef is re-acked"
        );

        // Status -> a one-line summary with the live counters.
        client
            .write_all(&encode_mux_frame(MuxTag::Status, b""))
            .unwrap();
        assert!(process_ipc_conn(&mut conn, &mut state, &ctx, &mut Vec::new()));
        let status_frame = read_client_frame(&mut client);
        assert_eq!(status_frame.tag, MuxTag::StatusReply);
        let line = String::from_utf8(status_frame.payload).unwrap();
        assert!(!line.contains('\n'), "one line: {line:?}");
        for needle in [
            "example.com-4",
            "refs=1",
            "channels=0",
            "heard=12ms",
            "cwnd=262144",
            "cuts=0",
            "streak_hwm=0",
        ] {
            assert!(line.contains(needle), "{needle:?} missing from {line:?}");
        }

        // Dropping the IPC connection auto-unrefs (crashed client safety).
        drop(client);
        assert!(!process_ipc_conn(&mut conn, &mut state, &ctx, &mut Vec::new()), "EOF ends the conn");
        drop_ipc_conn(&conn, &mut state, 5_000);
        assert_eq!(state.refs(), 0);
        assert!(!state.serviceable());
        assert!(state.lingering(), "auto-unref-to-zero arms the linger clock");
    }

    #[test]
    fn ipc_mismatched_hello_stamp_is_answered_then_rejected() {
        use std::io::Write;
        let (mut conn, mut client) = ipc_pair();
        let mut state = MuxState::new(60_000, 0);
        state.add_ref(); // an unrelated live ref must survive the rejection
        let ctx = test_ctx("k");
        let hello = MuxHello {
            pid: 9,
            stamp: "mux0/9".to_string(),
        };
        client
            .write_all(&encode_mux_frame(MuxTag::Hello, &hello.encode()))
            .unwrap();
        // §6: never negotiate down — answer with OUR stamp (so the client can
        // tell and start a fresh endpoint), then reject the connection.
        assert!(!process_ipc_conn(&mut conn, &mut state, &ctx, &mut Vec::new()));
        let ack = MuxHelloAck::decode(&read_client_frame(&mut client).payload).unwrap();
        assert_eq!(ack.stamp, MUX_PROTO_STAMP);
        assert!(!conn.holds_ref);
        drop_ipc_conn(&conn, &mut state, 0);
        assert_eq!(state.refs(), 1, "rejection must not touch other refs");
    }

    #[test]
    fn ipc_session_ref_before_hello_is_a_protocol_error() {
        use std::io::Write;
        let (mut conn, mut client) = ipc_pair();
        let mut state = MuxState::new(60_000, 0);
        let ctx = test_ctx("k");
        client
            .write_all(&encode_mux_frame(MuxTag::SessionRef, b""))
            .unwrap();
        assert!(!process_ipc_conn(&mut conn, &mut state, &ctx, &mut Vec::new()));
        assert_eq!(state.refs(), 0);
    }

    // --- The daemon (M1 Task 3.3): bind seam + the loop over loopback ---

    #[test]
    fn losing_bind_race_connects_to_the_live_winner() {
        let base = temp_base();
        let path = base.join("race.sock");
        let winner = match bind_or_probe(&path).unwrap() {
            MuxBind::Bound(l) => l,
            MuxBind::ExistingDaemon => panic!("first bind must win"),
        };
        // A second binder while the winner LIVES: detect it and defer — the
        // spawner then talks to the winner instead of spawning a duplicate.
        match bind_or_probe(&path).unwrap() {
            MuxBind::ExistingDaemon => {}
            MuxBind::Bound(_) => panic!("must not steal a live daemon's socket"),
        }
        drop(winner);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn stale_mux_socket_is_unlinked_and_rebound() {
        let base = temp_base();
        let path = base.join("stale.sock");
        // A crashed daemon leaves the socket file behind with nothing
        // accepting: connect fails, so the next spawner reclaims it.
        match bind_or_probe(&path).unwrap() {
            MuxBind::Bound(l) => drop(l), // dead, file still present
            MuxBind::ExistingDaemon => panic!("first bind must win"),
        }
        assert!(std::fs::symlink_metadata(&path).is_ok(), "stale file remains");
        match bind_or_probe(&path).unwrap() {
            MuxBind::Bound(_) => {}
            MuxBind::ExistingDaemon => panic!("a dead socket must be reclaimed"),
        }
        std::fs::remove_dir_all(&base).ok();
    }

    /// End-to-end daemon-loop lifecycle against a synthetic remote — the
    /// Task 2 [`agent_only_loop`](crate::remote::server) on loopback UDP is
    /// the ideal peer. Drives: hello/ack over the real IPC socket, the
    /// FDR 0014 M1 serviceability gate (an agent consumer is FAILed while
    /// refs == 0, serviced after `MuxSessionRef`), `MuxStatus` through the
    /// loop, and auto-unref-on-close arming the linger that exits the loop.
    #[test]
    fn mux_loop_gates_agent_service_on_refs_and_exits_after_linger() {
        use std::io::{Read, Write};
        use std::os::unix::net::{UnixListener, UnixStream};

        const REQUEST: &[u8] = b"AGENT-REQUEST-PING";
        const REPLY: &[u8] = b"AGENT-REPLY-PONG";

        let local_base = temp_base();
        let remote_base = temp_base();
        let agent_sock = local_base.join("fake-agent.sock");

        // Fake local ssh-agent: exactly one connection is ever expected to
        // reach it — the serviceable phase-2 round-trip. The FAILed phase-1
        // attempt must never dial it.
        let agent_listener = UnixListener::bind(&agent_sock).unwrap();
        let agent_thread = std::thread::spawn(move || {
            if let Ok((mut s, _)) = agent_listener.accept() {
                let mut buf = vec![0u8; REQUEST.len()];
                if s.read_exact(&mut buf).is_ok() {
                    assert_eq!(buf, REQUEST);
                    let _ = s.write_all(REPLY);
                }
            }
        });

        // The synthetic remote: a real agent-only server (Task 2) with a
        // short peer timeout so it exits once the daemon goes silent.
        let key = crate::remote::crypto::Key::random();
        let (server_conn, port) =
            Connection::server((63400, 63449), &key, Family::Inet).unwrap();
        let endpoint =
            crate::remote::agent::AgentEndpoint::new_mux(&remote_base, "muxdaemon").unwrap();
        let well_known = endpoint.sock_path().to_path_buf();
        let remote = std::thread::spawn(move || {
            crate::remote::server::agent_only_loop(server_conn, endpoint, 4_000)
        });

        // The daemon loop under test, on its own thread: real IPC listener,
        // real loopback connection, a 3 s linger — the same clock also arms
        // at construction (the orphan guard), so it must outlast the
        // pre-ref phase-1 exchange below.
        let sock_path = local_base.join("dest.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let conn = Connection::client(addr, &key).unwrap();
        let daemon = {
            let agent_sock = agent_sock.clone();
            std::thread::spawn(move || mux_loop(listener, conn, &agent_sock, 3_000, "test-dest"))
        };

        // IPC hello.
        let mut ipc = UnixStream::connect(&sock_path).unwrap();
        ipc.set_read_timeout(Some(std::time::Duration::from_secs(8)))
            .unwrap();
        let hello = MuxHello {
            pid: std::process::id(),
            stamp: MUX_PROTO_STAMP.to_string(),
        };
        ipc.write_all(&encode_mux_frame(MuxTag::Hello, &hello.encode()))
            .unwrap();
        let ack = MuxHelloAck::decode(&read_client_frame(&mut ipc).payload).unwrap();
        assert_eq!(ack.state, MuxConnState::Connected);
        assert_eq!(ack.stamp, MUX_PROTO_STAMP);
        assert_eq!(ack.key, "test-dest");
        assert_eq!(ack.source, agent_sock, "the loop reports its agent source");

        // Phase 1 — refs == 0 (the FDR 0014 M1 policy): a consumer on the
        // remote's agent/sock is answered with FAIL, never the local agent.
        let mut refused = UnixStream::connect(&well_known).unwrap();
        refused
            .set_read_timeout(Some(std::time::Duration::from_secs(8)))
            .unwrap();
        // The FAIL can close the socket before this write lands (EPIPE) —
        // that is refusal evidence too, so the write is not asserted.
        let _ = refused.write_all(REQUEST);
        let mut got = Vec::new();
        let _ = refused.read_to_end(&mut got); // failure answer + close
        assert!(
            !got.windows(REPLY.len()).any(|w| w == REPLY),
            "an unreferenced mux must never service an agent request: {got:?}"
        );

        // Ref; the daemon confirms registration with a RefAck.
        ipc.write_all(&encode_mux_frame(MuxTag::SessionRef, b""))
            .unwrap();
        assert_eq!(read_client_frame(&mut ipc).tag, MuxTag::RefAck);
        let deadline = util::now_ms() + 8_000;
        loop {
            ipc.write_all(&encode_mux_frame(MuxTag::Status, b"")).unwrap();
            let frame = read_client_frame(&mut ipc);
            assert_eq!(frame.tag, MuxTag::StatusReply);
            let line = String::from_utf8(frame.payload).unwrap();
            if line.contains("refs=1") {
                break;
            }
            assert!(util::now_ms() < deadline, "ref never landed: {line:?}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Phase 2 — serviceable: the same consumer path round-trips through
        // the daemon's client half to the fake local agent and back.
        let mut served = UnixStream::connect(&well_known).unwrap();
        served
            .set_read_timeout(Some(std::time::Duration::from_secs(8)))
            .unwrap();
        served.write_all(REQUEST).unwrap();
        let mut reply = vec![0u8; REPLY.len()];
        served
            .read_exact(&mut reply)
            .expect("a referenced mux services agent channels");
        assert_eq!(reply, REPLY);

        // Drop the IPC conn: auto-unref -> linger (3 s) -> loop exit.
        drop(ipc);
        daemon
            .join()
            .expect("the daemon loop exits after the linger window");
        // Silence from the departed daemon ends the remote too (Drop
        // semantics on the remote side follow from connection timeout).
        remote.join().expect("the agent-only remote exits on peer silence");
        let _ = agent_thread.join();
        std::fs::remove_dir_all(&local_base).ok();
        std::fs::remove_dir_all(&remote_base).ok();
    }

    #[test]
    fn linger_env_parses_seconds_with_default() {
        // POSH_MUX_PERSIST is seconds (the POSH_SERVER_*_TMOUT / ssh
        // ControlPersist convention); internal time is ms. Tested via the pure
        // predicate, never the process environment.
        assert_eq!(parse_linger_ms(None), DEFAULT_LINGER_MS);
        assert_eq!(parse_linger_ms(Some("")), DEFAULT_LINGER_MS);
        assert_eq!(parse_linger_ms(Some("junk")), DEFAULT_LINGER_MS);
        assert_eq!(parse_linger_ms(Some("-3")), DEFAULT_LINGER_MS);
        assert_eq!(parse_linger_ms(Some("0")), 0);
        assert_eq!(parse_linger_ms(Some("5")), 5_000);
        assert_eq!(parse_linger_ms(Some(" 120 ")), 120_000);
    }

    /// A private 0700 base dir with a SHORT path (the `agent.rs` `temp_base`
    /// pattern): the scratch `$TMPDIR` is too deep for sun_path, so anchor at
    /// `/tmp` like the production `/tmp/posh-<uid>` fallback.
    fn temp_base() -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = PathBuf::from(format!("/tmp/posh-mux-{}-{}", std::process::id(), n));
        std::fs::remove_dir_all(&base).ok();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)
            .unwrap();
        base
    }

    #[test]
    fn dest_key_is_stable_and_case_folds_host() {
        assert_eq!(
            dest_key(None, "Example.COM", Family::Auto, None),
            dest_key(None, "example.com", Family::Auto, None),
        );
        // Same inputs twice ⇒ byte-identical key.
        assert_eq!(
            dest_key(Some("me"), "example.com", Family::Inet, Some("60000:61000")),
            dest_key(Some("me"), "example.com", Family::Inet, Some("60000:61000")),
        );
    }

    #[test]
    fn dest_key_distinguishes_user_from_default() {
        assert_ne!(
            dest_key(Some("root"), "example.com", Family::Auto, None),
            dest_key(None, "example.com", Family::Auto, None),
        );
    }

    #[test]
    fn dest_key_distinguishes_family() {
        let auto = dest_key(None, "example.com", Family::Auto, None);
        let v4 = dest_key(None, "example.com", Family::Inet, None);
        let v6 = dest_key(None, "example.com", Family::Inet6, None);
        assert_ne!(auto, v4);
        assert_ne!(auto, v6);
        assert_ne!(v4, v6);
    }

    #[test]
    fn dest_key_distinguishes_port_ranges() {
        let none = dest_key(None, "example.com", Family::Auto, None);
        let a = dest_key(None, "example.com", Family::Auto, Some("60000:61000"));
        let b = dest_key(None, "example.com", Family::Auto, Some("62000:62099"));
        assert_ne!(none, a);
        assert_ne!(a, b);
    }

    #[test]
    fn hostile_host_yields_safe_slug_without_traversal() {
        let key = dest_key(Some("ev il"), "../..//etc passwd\x01ü", Family::Auto, None);
        assert!(
            key.bytes().all(
                |b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-'
            ),
            "unsafe byte survived: {key:?}"
        );
        assert!(!key.contains('/'), "path separator survived: {key:?}");
        // The key is a single path component: joining it never escapes mux/.
        let joined = Path::new("/base/mux").join(format!("{key}.sock"));
        assert!(joined.starts_with("/base/mux"));
    }

    #[test]
    fn sanitize_id_keeps_safe_bytes_and_maps_the_rest() {
        assert_eq!(sanitize_id("host-1.example_X"), "host-1.example_X");
        assert_eq!(sanitize_id("a b/c:d@e"), "a-b-c-d-e");
        // Multi-byte UTF-8: every byte outside the safe set maps to '-'.
        assert_eq!(sanitize_id("ü"), "--");
        assert_eq!(sanitize_id(""), "");
    }

    #[test]
    fn mux_ls_reports_live_stale_and_empty() {
        let dir = temp_base();
        assert_eq!(mux_ls_in(&dir).unwrap(), "no mux endpoints\n");
        // One live in-process daemon, one dead socket (bound then dropped —
        // connect refused), one non-.sock sibling that must be skipped.
        let (daemon, _server) = start_inprocess_daemon(&dir, "lslive", 2_500, (63660, 63669));
        drop(UnixListener::bind(mux_socket_path_in(&dir, "lsdead")).unwrap());
        std::fs::write(dir.join("lslive.pid"), b"1").unwrap();
        let out = mux_ls_in(&dir).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "one line per socket, sorted: {out}");
        assert!(lines[0].starts_with("mux lsdead: stale ("), "{out}");
        assert!(lines[1].starts_with("mux lslive: state="), "{out}");
        assert!(
            lines[1].contains("refs=0") && lines[1].contains("cwnd="),
            "the live line is the daemon's own status one-liner: {out}"
        );
        daemon.join().unwrap(); // observer probes never reset the linger
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mux_dir_at_creates_private_hardened_dir() {
        use std::os::unix::fs::MetadataExt;
        let base = temp_base();
        let dir = mux_dir_at(&base).unwrap();
        assert_eq!(dir, base.join("mux"));
        let mode = std::fs::metadata(&dir).unwrap().mode();
        assert_eq!(mode & 0o777, 0o700, "mux/ must be private, got {mode:o}");
        // Idempotent: a second call on the existing dir succeeds.
        assert_eq!(mux_dir_at(&base).unwrap(), dir);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn mux_dir_at_rejects_symlinked_mux_dir() {
        // A pre-planted symlink at <base>/mux must be refused by the shared
        // #7 hardening rather than followed — same contract as agent/.
        let base = temp_base();
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, base.join("mux")).unwrap();
        assert!(mux_dir_at(&base).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn mux_socket_path_lands_under_mux_dir_with_sock_suffix() {
        let base = temp_base();
        let dir = mux_dir_at(&base).unwrap();
        // The pub fn resolves the base from env; the join it performs is the
        // pure seam pinned here.
        let path = mux_socket_path_in(&dir, "example.com-4");
        assert_eq!(path, base.join("mux").join("example.com-4.sock"));
        std::fs::remove_dir_all(&base).ok();
    }

    // --- The client half (M1 Task 4): ensure_mux over the in-process Task 3
    // daemon loop — no forking; the spawn seam stands in for run_daemon. ---

    /// Binds `<dir>/<key>.sock` and runs the REAL [`mux_loop`] on a thread
    /// over a loopback UDP pair whose server half is merely held (the
    /// daemon's heartbeats go unanswered, which the loop tolerates) — the
    /// in-process stand-in for [`run_daemon`]'s grandchild.
    fn start_inprocess_daemon(
        dir: &Path,
        key: &str,
        linger_ms: u64,
        ports: (u16, u16),
    ) -> (std::thread::JoinHandle<()>, Connection) {
        let ukey = crate::remote::crypto::Key::random();
        let (server_conn, port) = Connection::server(ports, &ukey, Family::Inet).unwrap();
        let listener = UnixListener::bind(mux_socket_path_in(dir, key)).unwrap();
        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let conn = Connection::client(addr, &ukey).unwrap();
        let agent = dir.join("no-agent.sock");
        let key = key.to_string();
        let handle = std::thread::spawn(move || mux_loop(listener, conn, &agent, linger_ms, &key));
        (handle, server_conn)
    }

    /// A hello-completed observer conn for `MuxStatus` queries (holds no ref).
    fn ipc_observer(path: &Path) -> std::os::unix::net::UnixStream {
        use std::io::Write;
        let mut s = std::os::unix::net::UnixStream::connect(path).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        let hello = MuxHello {
            pid: 1,
            stamp: MUX_PROTO_STAMP.to_string(),
        };
        s.write_all(&encode_mux_frame(MuxTag::Hello, &hello.encode())).unwrap();
        assert_eq!(read_client_frame(&mut s).tag, MuxTag::HelloAck);
        s
    }

    fn ipc_status(s: &mut std::os::unix::net::UnixStream) -> String {
        use std::io::Write;
        s.write_all(&encode_mux_frame(MuxTag::Status, b"")).unwrap();
        let f = read_client_frame(s);
        assert_eq!(f.tag, MuxTag::StatusReply);
        String::from_utf8(f.payload).unwrap()
    }

    /// Polls `MuxStatus` until the summary contains `needle` (bounded).
    fn wait_status_contains(s: &mut std::os::unix::net::UnixStream, needle: &str) {
        let deadline = util::now_ms() + 8_000;
        loop {
            let line = ipc_status(s);
            if line.contains(needle) {
                return;
            }
            assert!(util::now_ms() < deadline, "status never showed {needle:?}: {line:?}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    // --- M2 session-channel routing (Task 2): the daemon side driven over
    // the in-process harness; the test plays the remote peer on the wire. ---

    /// Receives assembled instructions on the test's server connection until
    /// `pred` accepts one (bounded). Heartbeats ride bare `SESSION_CHANNEL`
    /// and are skipped by predicates keyed on ordinals >= 2.
    fn recv_wire_until(
        server: &mut Connection,
        assembly: &mut sync::FragmentAssembly,
        mut pred: impl FnMut(channel::ChannelId, &[u8]) -> bool,
    ) -> (channel::ChannelId, Vec<u8>) {
        let deadline = util::now_ms() + 8_000;
        loop {
            assert!(util::now_ms() < deadline, "wire instruction never arrived");
            match server.recv() {
                Ok(Some(payload)) => {
                    let Ok(frag) = sync::Fragment::from_bytes(&payload) else { continue };
                    let Some(assembled) = assembly.add(frag) else { continue };
                    let Some((chan, message)) = channel::open_any_instruction(true, &assembled)
                    else {
                        continue;
                    };
                    if pred(chan, message) {
                        return (chan, message.to_vec());
                    }
                }
                Ok(None) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("server recv: {e}"),
            }
        }
    }

    /// Sends one micro-enveloped session instruction from the test's remote
    /// peer toward the mux daemon.
    fn send_wire(
        server: &mut Connection,
        fragmenter: &mut sync::Fragmenter,
        chan: channel::ChannelId,
        kind: u8,
        body: &[u8],
    ) {
        let mut payload = vec![kind];
        payload.extend_from_slice(body);
        let wire = channel::seal_on(true, chan, &payload);
        for frag in fragmenter.make_fragments(&wire, sync::FRAGMENT_CONTENTS_MAX) {
            let _ = server.send(&frag.to_bytes());
        }
    }

    /// Hello + SessionOpen over the IPC socket; returns the stream and the
    /// granted wire ordinal.
    fn ipc_open_session(path: &Path, target: &[u8]) -> (std::os::unix::net::UnixStream, u64) {
        use std::io::Write;
        let mut s = ipc_observer(path);
        s.write_all(&encode_mux_frame(MuxTag::SessionOpen, target)).unwrap();
        loop {
            let f = read_client_frame(&mut s);
            if f.tag == MuxTag::SessionOpenAck {
                match MuxSessionOpenAck::decode(&f.payload).unwrap() {
                    MuxSessionOpenAck::Granted { ordinal } => return (s, ordinal),
                    MuxSessionOpenAck::Refused { reason } => panic!("refused: {reason}"),
                }
            }
        }
    }

    #[test]
    fn daemon_requests_ident_and_reports_the_remote_build() {
        use crate::remote::caps;
        let dir = temp_base();
        let (daemon, mut server) = start_inprocess_daemon(&dir, "ident", 3_000, (63970, 63979));
        // While no remote ident is held, the heartbeat carries the RFC 0013
        // §3 request on the reserved session channel.
        let mut assembly = sync::FragmentAssembly::new();
        let (_chan, _msg) = recv_wire_until(&mut server, &mut assembly, |c, m| {
            c == channel::SESSION_CHANNEL
                && sync::ClientMessage::decode(m)
                    .is_ok_and(|cm| caps::find(&cm.caps, caps::CAP_SERVER_IDENT).is_some())
        });
        // Answer with one Empty frame carrying a distinctive ident.
        let ident = caps::encode_server_ident(&caps::ServerIdent {
            version: "9.9.9".into(),
            git_sha: "cafef00".into(),
            pid: 77,
            start_unix_ms: 1,
        });
        let frame = sync::ServerFrame {
            flags: 0,
            caps: vec![ident],
            frame_num: 0,
            input_ack: 0,
            echo_ack: 0,
            body: sync::FrameBody::Empty,
        };
        let encoded = frame.encode();
        let wire = channel::seal_on(true, channel::SESSION_CHANNEL, &encoded);
        let mut fragmenter = sync::Fragmenter::new();
        for frag in fragmenter.make_fragments(&wire, sync::FRAGMENT_CONTENTS_MAX) {
            let _ = server.send(&frag.to_bytes());
        }
        // The status line reports the remote build (mux ls's new column).
        let mut obs = ipc_observer(&mux_socket_path_in(&dir, "ident"));
        wait_status_contains(&mut obs, "remote=9.9.9 (cafef00)");
        drop(obs);
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_open_allocates_channel_acks_and_refs() {
        let dir = temp_base();
        let (daemon, mut server) = start_inprocess_daemon(&dir, "m2open", 1_000, (63700, 63709));
        let (ipc, ordinal) = ipc_open_session(&mux_socket_path_in(&dir, "m2open"), b"box:dev");
        assert!(ordinal >= 2, "ordinal 1 is the reserved heartbeat stream, got {ordinal}");
        let mut obs = ipc_observer(&mux_socket_path_in(&dir, "m2open"));
        wait_status_contains(&mut obs, "refs=1 ");
        // The wire OPEN carries the target (RFC 0011 §3.3), retransmitted
        // until confirmed — the remote peer sees it.
        let mut assembly = sync::FragmentAssembly::new();
        let (chan, msg) = recv_wire_until(&mut server, &mut assembly, |c, m| {
            c.kind() == channel::KIND_SESSION
                && c.ordinal() == ordinal
                && m.first() == Some(&SESSION_WIRE_OPEN)
        });
        assert_eq!(&msg[1..], b"box:dev");
        assert_eq!(chan.ordinal(), ordinal);
        drop(ipc);
        wait_status_contains(&mut obs, "refs=0 ");
        drop(obs);
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_msg_routes_to_the_wire_and_frames_route_back() {
        use std::io::Write;
        let dir = temp_base();
        let (daemon, mut server) = start_inprocess_daemon(&dir, "m2msg", 1_000, (63710, 63719));
        let (mut ipc, ordinal) = ipc_open_session(&mux_socket_path_in(&dir, "m2msg"), b"box:a");
        let mut assembly = sync::FragmentAssembly::new();
        let mut fragmenter = sync::Fragmenter::new();
        // A message sent BEFORE confirmation queues — the wire must show the
        // OPEN first, and the data only after the remote's first reply.
        ipc.write_all(&encode_mux_frame(MuxTag::SessionMsg, b"early input")).unwrap();
        let (chan, _) = recv_wire_until(&mut server, &mut assembly, |c, m| {
            c.ordinal() == ordinal && m.first() == Some(&SESSION_WIRE_OPEN)
        });
        // Confirm by answering with a frame; the mux must (a) deliver it to
        // the IPC conn with the srtt prefix and (b) flush the queued input.
        send_wire(&mut server, &mut fragmenter, chan, SESSION_WIRE_DATA, b"first frame");
        loop {
            let f = read_client_frame(&mut ipc);
            if f.tag == MuxTag::SessionFrame {
                let (_srtt, body) = decode_session_frame(&f.payload).unwrap();
                assert_eq!(body, b"first frame");
                break;
            }
        }
        let (_, early) = recv_wire_until(&mut server, &mut assembly, |c, m| {
            c.ordinal() == ordinal && m.first() == Some(&SESSION_WIRE_DATA)
        });
        assert_eq!(&early[1..], b"early input", "queued input flushes on confirmation");
        // Post-confirmation messages relay immediately.
        ipc.write_all(&encode_mux_frame(MuxTag::SessionMsg, b"live input")).unwrap();
        let (_, live) = recv_wire_until(&mut server, &mut assembly, |c, m| {
            c.ordinal() == ordinal
                && m.first() == Some(&SESSION_WIRE_DATA)
                && &m[1..] == b"live input"
        });
        assert_eq!(&live[1..], b"live input");
        drop(ipc);
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A wire session frame LARGER than one fragment (a real TUI redraw)
    /// must reassemble in the mux daemon and reach the IPC conn intact —
    /// isolates the wire→local-daemon→IPC leg of the "large frames vanish"
    /// wedge (vim/sc-list screens are multi-fragment; echoes are not).
    #[test]
    fn session_frames_larger_than_one_fragment_route_to_ipc() {
        use std::io::Write;
        let dir = temp_base();
        let (daemon, mut server) = start_inprocess_daemon(&dir, "m2big", 1_000, (63820, 63829));
        let (mut ipc, ordinal) = ipc_open_session(&mux_socket_path_in(&dir, "m2big"), b"box:big");
        let mut assembly = sync::FragmentAssembly::new();
        let mut fragmenter = sync::Fragmenter::new();
        ipc.write_all(&encode_mux_frame(MuxTag::SessionMsg, b"poke")).unwrap();
        let (chan, _) = recv_wire_until(&mut server, &mut assembly, |c, m| {
            c.ordinal() == ordinal && m.first() == Some(&SESSION_WIRE_OPEN)
        });
        let big = sync::multi_fragment_payload();
        send_wire(&mut server, &mut fragmenter, chan, SESSION_WIRE_DATA, &big);
        loop {
            let f = read_client_frame(&mut ipc);
            if f.tag == MuxTag::SessionFrame {
                let (_srtt, body) = decode_session_frame(&f.payload).unwrap();
                assert_eq!(body, big, "the multi-fragment frame must arrive intact");
                break;
            }
        }
        drop(ipc);
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn two_ipc_conns_get_disjoint_channels_and_isolated_frames() {
        let dir = temp_base();
        let (daemon, mut server) = start_inprocess_daemon(&dir, "m2iso", 1_000, (63720, 63729));
        let path = mux_socket_path_in(&dir, "m2iso");
        let (mut ipc_a, ord_a) = ipc_open_session(&path, b"box:a");
        let (mut ipc_b, ord_b) = ipc_open_session(&path, b"box:b");
        assert_ne!(ord_a, ord_b, "channels are disjoint");
        let mut assembly = sync::FragmentAssembly::new();
        let mut fragmenter = sync::Fragmenter::new();
        let (chan_a, _) = recv_wire_until(&mut server, &mut assembly, |c, m| {
            c.ordinal() == ord_a && m.first() == Some(&SESSION_WIRE_OPEN)
        });
        let (chan_b, _) = recv_wire_until(&mut server, &mut assembly, |c, m| {
            c.ordinal() == ord_b && m.first() == Some(&SESSION_WIRE_OPEN)
        });
        // One frame to each channel: each IPC conn sees exactly its own.
        send_wire(&mut server, &mut fragmenter, chan_a, SESSION_WIRE_DATA, b"for a");
        send_wire(&mut server, &mut fragmenter, chan_b, SESSION_WIRE_DATA, b"for b");
        let fa = loop {
            let f = read_client_frame(&mut ipc_a);
            if f.tag == MuxTag::SessionFrame {
                break decode_session_frame(&f.payload).unwrap().1.to_vec();
            }
        };
        let fb = loop {
            let f = read_client_frame(&mut ipc_b);
            if f.tag == MuxTag::SessionFrame {
                break decode_session_frame(&f.payload).unwrap().1.to_vec();
            }
        };
        assert_eq!(fa, b"for a");
        assert_eq!(fb, b"for b");
        drop(ipc_a);
        drop(ipc_b);
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ipc_conn_drop_closes_its_wire_channel_and_unrefs() {
        let dir = temp_base();
        let (daemon, mut server) = start_inprocess_daemon(&dir, "m2drop", 1_000, (63730, 63739));
        let path = mux_socket_path_in(&dir, "m2drop");
        let (ipc, ordinal) = ipc_open_session(&path, b"box:gone");
        let mut obs = ipc_observer(&path);
        wait_status_contains(&mut obs, "refs=1 ");
        let mut assembly = sync::FragmentAssembly::new();
        recv_wire_until(&mut server, &mut assembly, |c, m| {
            c.ordinal() == ordinal && m.first() == Some(&SESSION_WIRE_OPEN)
        });
        // Dropping the IPC conn owes the wire a CLOSE and releases the ref.
        drop(ipc);
        recv_wire_until(&mut server, &mut assembly, |c, m| {
            c.ordinal() == ordinal && m.first() == Some(&SESSION_WIRE_CLOSE)
        });
        wait_status_contains(&mut obs, "refs=0 ");
        drop(obs);
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_retransmits_target_until_acked() {
        let dir = temp_base();
        let (daemon, mut server) = start_inprocess_daemon(&dir, "m2retx", 2_000, (63740, 63749));
        let (ipc, ordinal) = ipc_open_session(&mux_socket_path_in(&dir, "m2retx"), b"box:slow");
        let mut assembly = sync::FragmentAssembly::new();
        let mut fragmenter = sync::Fragmenter::new();
        // Two OPENs without an answer proves the RTO retransmit; both carry
        // the target (subsequent instructions never repeat it — §3.3 — so
        // both being OPEN-marked shows these are retransmissions).
        for _ in 0..2 {
            let (_, m) = recv_wire_until(&mut server, &mut assembly, |c, m| {
                c.ordinal() == ordinal && m.first() == Some(&SESSION_WIRE_OPEN)
            });
            assert_eq!(&m[1..], b"box:slow");
        }
        // Answer: the retransmits stop (bounded look for silence).
        let chan = channel::ChannelId::new(false, channel::KIND_SESSION, ordinal);
        send_wire(&mut server, &mut fragmenter, chan, SESSION_WIRE_DATA, b"ok");
        // Drain until quiet: after the confirmation reaches the daemon, no
        // further OPEN should arrive for a full second.
        let quiet_from = util::now_ms() + 1_500;
        let mut last_open = 0u64;
        while util::now_ms() < quiet_from + 1_000 {
            match server.recv() {
                Ok(Some(payload)) => {
                    if let Ok(frag) = sync::Fragment::from_bytes(&payload) {
                        if let Some(assembled) = assembly.add(frag) {
                            if let Some((c, m)) = channel::open_any_instruction(true, &assembled) {
                                if c.ordinal() == ordinal
                                    && m.first() == Some(&SESSION_WIRE_OPEN)
                                {
                                    last_open = util::now_ms();
                                }
                            }
                        }
                    }
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        assert!(
            last_open < quiet_from,
            "OPEN retransmits must stop after confirmation (last at {last_open}, quiet from {quiet_from})"
        );
        drop(ipc);
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mux_sessions_gate_is_opt_in() {
        use crate::remote::sshwrap::env_value_on;
        // The M2 rollout gate deliberately takes the POSH_CHANNELS opt-IN
        // truthy shape, NOT the promoted default-on off-switch (no env
        // mutation: the predicate is pinned directly).
        for v in ["1", "true", "on", "yes"] {
            assert!(env_value_on(v), "{v:?} must select mux sessions");
        }
        for v in ["", "0", "false", "off", "no", "2"] {
            assert!(!env_value_on(v), "{v:?} must leave the opt-in gate off");
        }
    }

    #[test]
    fn open_session_grants_and_transports_frames() {
        let dir = temp_base();
        let (daemon, mut server) = start_inprocess_daemon(&dir, "m2t", 1_000, (63630, 63639));
        let mut spawn = |_: &str| -> Result<MuxSpawn> { panic!("daemon is live") };
        let handle = ensure_mux_conn(
            &dir,
            "m2t",
            &mut spawn,
            std::time::Duration::from_secs(8),
            &dir.join("no-agent.sock"),
        )
        .unwrap();
        let mut t = handle.open_session("box:t").unwrap();
        // The wire OPEN reaches the remote; a frame comes back through the
        // transport with its body intact.
        let mut assembly = sync::FragmentAssembly::new();
        let mut fragmenter = sync::Fragmenter::new();
        let (chan, m) = recv_wire_until(&mut server, &mut assembly, |c, m| {
            c.kind() == channel::KIND_SESSION
                && c != channel::SESSION_CHANNEL
                && m.first() == Some(&SESSION_WIRE_OPEN)
        });
        assert_eq!(&m[1..], b"box:t");
        send_wire(&mut server, &mut fragmenter, chan, SESSION_WIRE_DATA, b"frame-bytes");
        let deadline = util::now_ms() + 8_000;
        let ev = loop {
            assert!(util::now_ms() < deadline, "transport never surfaced the frame");
            if let Some(ev) = t.next_event() {
                break ev;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(ev, MuxSessionEvent::Frame(b"frame-bytes".to_vec()));
        // A client message relays out as channel DATA.
        t.send_msg(b"client-message");
        let (_, m) = recv_wire_until(&mut server, &mut assembly, |c, m| {
            c == chan && m.first() == Some(&SESSION_WIRE_DATA) && &m[1..] == b"client-message"
        });
        assert_eq!(&m[1..], b"client-message");
        // Dropping the transport (the IPC conn) owes the wire a CLOSE.
        drop(t);
        recv_wire_until(&mut server, &mut assembly, |c, m| {
            c == chan && m.first() == Some(&SESSION_WIRE_CLOSE)
        });
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_session_refusal_is_an_error() {
        let dir = temp_base();
        let (daemon, _server) = start_inprocess_daemon(&dir, "m2full", 1_000, (63640, 63649));
        let path = mux_socket_path_in(&dir, "m2full");
        // Fill the local table (MAX_LOCAL_SESSION_CHANNELS) with raw opens…
        let mut held = Vec::new();
        for i in 0..MAX_LOCAL_SESSION_CHANNELS {
            let (s, _ord) = ipc_open_session(&path, format!("box:{i}").as_bytes());
            held.push(s);
        }
        // …then the 17th, through the real client half, must Err — the
        // fallback cue.
        let mut spawn = |_: &str| -> Result<MuxSpawn> { panic!("daemon is live") };
        let handle = ensure_mux_conn(
            &dir,
            "m2full",
            &mut spawn,
            std::time::Duration::from_secs(8),
            &dir.join("no-agent.sock"),
        )
        .unwrap();
        let err = match handle.open_session("box:overflow") {
            Ok(_) => panic!("a full table must refuse"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("refused"), "got: {err}");
        drop(held);
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The M2 end-to-end, all four components REAL and in-process: two
    /// `MuxSessionTransport`s → the mux daemon (`mux_loop`) → one loopback
    /// AEAD-UDP connection → the channel-table peer (`mux_peer_loop`) →
    /// per-channel fake daemons whose reply frames carry disjoint
    /// frame-number ranges, so delivery AND isolation are asserted through
    /// the whole chain. An agent round-trip rides the SAME connection
    /// (agent bulk + sessions coexisting), and killing one transport leaves
    /// the other fully live — the design doc's promotion-criteria shape,
    /// minus only the real-binary/real-ssh-agent staging that promotion
    /// itself will run.
    #[test]
    fn m2_two_transports_share_one_connection_and_survive_peer_loss() {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd as _;

        const REQUEST: &[u8] = b"AGENT-REQUEST-PING";
        const REPLY: &[u8] = b"AGENT-REPLY-PONG";

        let local_base = temp_base();
        let remote_base = temp_base();

        // Fake local ssh-agent behind the mux daemon's proxy.
        let agent_sock = local_base.join("fake-agent.sock");
        let agent_listener = UnixListener::bind(&agent_sock).unwrap();
        let agent_thread = std::thread::spawn(move || {
            if let Ok((mut s, _)) = agent_listener.accept() {
                let mut buf = vec![0u8; REQUEST.len()];
                if s.read_exact(&mut buf).is_ok() {
                    assert_eq!(buf, REQUEST);
                    let _ = s.write_all(REPLY);
                }
            }
        });

        // The remote peer: real mux_peer_loop; each opened target gets a
        // fake daemon thread that answers every Tag::Input with a frame in
        // its own frame-number range (a=100+, b=200+).
        let ukey = crate::remote::crypto::Key::random();
        let (server_conn, port) = Connection::server((63650, 63659), &ukey, Family::Inet).unwrap();
        let endpoint =
            crate::remote::agent::AgentEndpoint::new_mux(&remote_base, "m2e2e").unwrap();
        let well_known = endpoint.sock_path().to_path_buf();
        let peer = std::thread::spawn(move || {
            let mut connector = |target: &str| {
                let (peer_side, daemon_side) = UnixStream::pair().unwrap();
                peer_side.set_nonblocking(true).unwrap();
                let base = if target.ends_with("/a") { 100u64 } else { 200u64 };
                std::thread::spawn(move || {
                    let mut buf = crate::session::ipc::FrameBuffer::new();
                    let mut inputs = 0u64;
                    daemon_side
                        .set_read_timeout(Some(std::time::Duration::from_millis(50)))
                        .ok();
                    let deadline = util::now_ms() + 15_000;
                    while util::now_ms() < deadline {
                        let _ = buf.read_from(daemon_side.as_raw_fd());
                        let mut wrote = false;
                        while let Ok(Some(rec)) = buf.next() {
                            match rec.tag {
                                crate::session::ipc::Tag::Input => {
                                    inputs += 1;
                                    let frame = sync::ServerFrame {
                                        flags: 0,
                                        caps: crate::remote::caps::own_table(&[]),
                                        frame_num: base + inputs,
                                        input_ack: 0,
                                        echo_ack: 0,
                                        body: sync::FrameBody::Empty,
                                    };
                                    let mut out = Vec::new();
                                    crate::session::ipc::append_frame(
                                        &mut out,
                                        crate::session::ipc::Tag::Frame,
                                        &frame.encode(),
                                    );
                                    if (&daemon_side).write_all(&out).is_err() {
                                        return;
                                    }
                                    wrote = true;
                                }
                                crate::session::ipc::Tag::Detach => return,
                                _ => {}
                            }
                        }
                        if !wrote {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                    }
                });
                Ok(peer_side)
            };
            crate::remote::server::mux_peer_loop(server_conn, endpoint, 6_000, &mut connector);
        });

        // The local mux daemon: real mux_loop over the loopback connection.
        let dir = local_base.clone();
        let listener = UnixListener::bind(mux_socket_path_in(&dir, "m2e2e")).unwrap();
        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let daemon_conn = Connection::client(addr, &ukey).unwrap();
        let agent_path = agent_sock.clone();
        let daemon = std::thread::spawn(move || {
            mux_loop(listener, daemon_conn, &agent_path, 1_000, "m2e2e")
        });

        // Two invocations through the real client half.
        let mut spawn = |_: &str| -> Result<MuxSpawn> { panic!("daemon is live") };
        let timeout = std::time::Duration::from_secs(10);
        let h_a = ensure_mux_conn(&dir, "m2e2e", &mut spawn, timeout, &agent_sock).unwrap();
        let mut t_a = h_a.open_session("default/a").unwrap();
        let h_b = ensure_mux_conn(&dir, "m2e2e", &mut spawn, timeout, &agent_sock).unwrap();
        let mut t_b = h_b.open_session("default/b").unwrap();

        let msg = |input: &[u8], acked: u64| {
            sync::ClientMessage {
                flags: 0,
                caps: crate::remote::caps::own_table(&[]),
                acked_frame: acked,
                rows: 24,
                cols: 80,
                input_base: 0,
                input: input.to_vec(),
            }
            .encode()
        };
        // Skips confirmation/ack empties (frame_num 0) and held-frame
        // retransmissions below `min` — cumulative acks make re-sends of an
        // already-seen number legal, never a test signal.
        let next_frame = |t: &mut MuxSessionTransport, min: u64| -> sync::ServerFrame {
            let deadline = util::now_ms() + 10_000;
            loop {
                assert!(util::now_ms() < deadline, "frame never arrived");
                match t.next_event() {
                    Some(MuxSessionEvent::Frame(b)) => {
                        let f = sync::ServerFrame::decode(&b).unwrap();
                        if f.frame_num >= min {
                            return f;
                        }
                    }
                    Some(MuxSessionEvent::Closed(p)) => {
                        panic!("channel closed unexpectedly: {p:?}")
                    }
                    None => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            }
        };

        t_a.send_msg(&msg(b"ping-a", 0));
        t_b.send_msg(&msg(b"ping-b", 0));
        let f_a = next_frame(&mut t_a, 1);
        let f_b = next_frame(&mut t_b, 1);
        assert_eq!(f_a.frame_num, 101, "A's frame from A's daemon, nothing else");
        assert_eq!(f_b.frame_num, 201, "B's frame from B's daemon, nothing else");

        // Agent bulk on the SAME connection: consumer → remote endpoint →
        // agent channel → mux daemon's proxy → fake local agent → back.
        let mut served = UnixStream::connect(&well_known).unwrap();
        served
            .set_read_timeout(Some(std::time::Duration::from_secs(8)))
            .unwrap();
        served.write_all(REQUEST).unwrap();
        let mut reply = vec![0u8; REPLY.len()];
        served
            .read_exact(&mut reply)
            .expect("agent service rides the shared connection");
        assert_eq!(reply, REPLY);

        // Kill A: B is untouched — another round-trip proves it. The message
        // acks 201 so the peer releases its held frame instead of
        // retransmitting it over the fresh one.
        drop(t_a);
        t_b.send_msg(&msg(b"ping-b-again", 201));
        let f_b2 = next_frame(&mut t_b, 202);
        assert_eq!(f_b2.frame_num, 202, "B keeps its daemon after A's death");

        drop(t_b);
        drop(served);
        agent_thread.join().unwrap();
        daemon.join().unwrap();
        peer.join().unwrap();
        std::fs::remove_dir_all(&local_base).ok();
        std::fs::remove_dir_all(&remote_base).ok();
    }

    #[test]
    fn split_dest_separates_the_optional_user() {
        assert_eq!(split_dest("example.com"), (None, "example.com"));
        assert_eq!(split_dest("me@example.com"), (Some("me"), "example.com"));
        // The LAST @ splits, ssh-style (matches bootstrap's fallback host).
        assert_eq!(split_dest("u@v@host"), (Some("u@v"), "host"));
        assert_eq!(split_dest("@host"), (None, "host"));
    }

    #[test]
    fn variant_key_appends_the_sanitized_stamp() {
        // The §6 mismatch path lands on `<key>.<ver>.sock`; the stamp's `/`
        // renders slug-safe so the variant stays a single path component.
        assert_eq!(variant_key("example.com-4"), "example.com-4.mux1-1");
    }

    #[test]
    fn ensure_mux_spawns_hellos_refs_and_drop_auto_unrefs() {
        let dir = temp_base();
        let spawned = std::cell::Cell::new(0u32);
        let mut daemon = None;
        let mut server_hold = None;
        let mut spawn = |k: &str| {
            spawned.set(spawned.get() + 1);
            let (h, sc) = start_inprocess_daemon(&dir, k, 2_000, (63450, 63459));
            daemon = Some(h);
            server_hold = Some(sc);
            Ok(MuxSpawn::Spawned)
        };
        let timeout = std::time::Duration::from_secs(8);
        let local_source = dir.join("no-agent.sock");

        // Absent socket: exactly one spawn, then connect + hello + ref.
        let handle = ensure_mux_conn(&dir, "dest", &mut spawn, timeout, &local_source).unwrap();
        assert_eq!(handle.key(), "dest");
        assert_eq!(handle.state(), MuxConnState::Connected);
        assert_eq!(
            handle.source_mismatch(),
            None,
            "a matching agent source is not a mismatch"
        );
        assert_eq!(spawned.get(), 1, "absent socket: exactly one spawn");

        let mut obs = ipc_observer(&mux_socket_path_in(&dir, "dest"));
        wait_status_contains(&mut obs, "refs=1 ");

        // A second invocation reuses the live daemon: no new spawn, 2nd ref.
        let handle2 = ensure_mux_conn(&dir, "dest", &mut spawn, timeout, &local_source).unwrap();
        assert_eq!(spawned.get(), 1, "a live daemon is reused, never respawned");
        wait_status_contains(&mut obs, "refs=2 ");

        // Drop = auto-unref, one handle at a time, observable via MuxStatus.
        drop(handle2);
        wait_status_contains(&mut obs, "refs=1 ");
        drop(handle);
        wait_status_contains(&mut obs, "refs=0 ");

        drop(obs);
        daemon.take().unwrap().join().unwrap(); // the 2 s linger expiry exits
        drop(server_hold);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_mux_retries_connect_after_losing_the_spawn_race() {
        let dir = temp_base();
        // The spawn seam reports AlreadyRunning (another spawner's bind won)
        // while the winner is still a beat away from `bind`: the client must
        // keep retrying within its bounded budget instead of failing on the
        // first refused connect.
        let mut pending = None;
        let mut spawn = |k: &str| {
            let dir = dir.clone();
            let k = k.to_string();
            pending = Some(std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(120));
                start_inprocess_daemon(&dir, &k, 1_000, (63460, 63469))
            }));
            Ok(MuxSpawn::AlreadyRunning)
        };
        let handle = ensure_mux_conn(
            &dir,
            "raced",
            &mut spawn,
            std::time::Duration::from_secs(8),
            &dir.join("no-agent.sock"),
        )
        .unwrap();
        assert_eq!(handle.key(), "raced");
        assert_eq!(handle.state(), MuxConnState::Connected);
        drop(handle);
        let (daemon, _server) = pending.take().unwrap().join().unwrap();
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn spawned_daemon_that_dies_before_hello_falls_back_and_unlinks() {
        // The default-on interop shape (promotion, FDR 0014): against a
        // remote whose posh-server lacks the `agent` verb, run_daemon binds
        // the socket, its grandchild's ssh bootstrap fails, and the error
        // path unlinks the socket and exits without ever answering a hello.
        // The spawner must surface an ensure error (=> apply_mux_gate falls
        // back to per-connection forwarding) and leave NO socket behind, so
        // the next invocation re-attempts a spawn instead of hitting a
        // stale endpoint.
        let dir = temp_base();
        let path = mux_socket_path_in(&dir, "oldremote");
        let mut spawns = 0;
        let mut dying = None;
        let mut spawn = |_: &str| {
            spawns += 1;
            // Bind-then-die, mirroring run_daemon's bootstrap-failure exit:
            // accept the spawner's connect, close it unanswered, unlink.
            let listener = UnixListener::bind(&path).unwrap();
            let p = path.clone();
            dying = Some(std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                drop(stream); // EOF before any HelloAck
                std::fs::remove_file(&p).ok();
            }));
            Ok(MuxSpawn::Spawned)
        };
        let err = match ensure_mux_conn(
            &dir,
            "oldremote",
            &mut spawn,
            std::time::Duration::from_secs(8),
            &dir.join("no-agent.sock"),
        ) {
            Ok(_) => panic!("a daemon that died before hello must be an error"),
            Err(e) => e,
        };
        // EOF ("closed while awaiting HelloAck") or ECONNRESET ("HelloAck
        // read: ...") depending on whether the hello bytes were still
        // unread at close — either way an explicit error, never a silent
        // success.
        assert!(
            err.to_string().contains("HelloAck"),
            "the dead endpoint is an explicit hello-phase error, got: {err}"
        );
        dying.take().unwrap().join().unwrap();
        assert!(!path.exists(), "the failed daemon's socket must be unlinked");

        // Recovery: with the socket gone, the next ensure re-spawns — a
        // healthy daemon this time — and succeeds.
        assert_eq!(spawns, 1, "the failed attempt spawned exactly once");
        let mut hold = None;
        let mut respawn = |k: &str| {
            hold = Some(start_inprocess_daemon(&dir, k, 1_000, (63480, 63489)));
            Ok(MuxSpawn::Spawned)
        };
        let handle = ensure_mux_conn(
            &dir,
            "oldremote",
            &mut respawn,
            std::time::Duration::from_secs(8),
            &dir.join("no-agent.sock"),
        )
        .unwrap();
        assert_eq!(handle.state(), MuxConnState::Connected);
        drop(handle);
        let (daemon, _server) = hold.take().unwrap();
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stamp_mismatch_starts_the_variant_endpoint() {
        use std::io::Write;
        let dir = temp_base();
        // A live OLD-generation daemon owns the base-key socket: it answers
        // hello with a foreign stamp and closes, exactly as process_ipc_conn
        // does on mismatch (answered, then rejected).
        let old = UnixListener::bind(mux_socket_path_in(&dir, "dest")).unwrap();
        let old_thread = std::thread::spawn(move || {
            let (mut s, _) = old.accept().unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
            let frame = read_client_frame(&mut s);
            assert_eq!(frame.tag, MuxTag::Hello);
            let ack = MuxHelloAck {
                state: MuxConnState::Draining,
                stamp: "mux0/9".to_string(),
                key: "dest".to_string(),
                source: PathBuf::from("/old/agent.sock"),
            };
            s.write_all(&encode_mux_frame(MuxTag::HelloAck, &ack.encode())).unwrap();
        });
        let mut spawned_keys = Vec::new();
        let mut hold = None;
        let mut spawn = |k: &str| {
            spawned_keys.push(k.to_string());
            hold = Some(start_inprocess_daemon(&dir, k, 1_000, (63470, 63479)));
            Ok(MuxSpawn::Spawned)
        };
        let handle = ensure_mux_conn(
            &dir,
            "dest",
            &mut spawn,
            std::time::Duration::from_secs(8),
            &dir.join("no-agent.sock"),
        )
        .unwrap();
        // §6: never negotiate down — the fresh endpoint lives on the variant
        // socket; the old one is left to drain.
        assert_eq!(spawned_keys, vec![variant_key("dest")], "only the variant spawns");
        assert_eq!(handle.key(), variant_key("dest"));
        assert_eq!(handle.state(), MuxConnState::Connected);
        let mut obs = ipc_observer(&mux_socket_path_in(&dir, &variant_key("dest")));
        wait_status_contains(&mut obs, "refs=1 ");
        old_thread.join().unwrap();
        drop(handle);
        drop(obs);
        let (daemon, _server) = hold.take().unwrap();
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn differing_agent_source_is_noted_on_the_handle() {
        // Finding-3 seam: the daemon reports ITS resolved agent source in
        // the hello ack; an invocation that resolved a DIFFERENT source gets
        // a handle noting the daemon's path (the caller warns and proceeds —
        // keep = the daemon's, restart to change), while a matching source
        // notes nothing.
        let dir = temp_base();
        let daemon_source = dir.join("no-agent.sock"); // start_inprocess_daemon's
        let mut hold = None;
        let mut spawn = |k: &str| {
            hold = Some(start_inprocess_daemon(&dir, k, 1_000, (63430, 63439)));
            Ok(MuxSpawn::Spawned)
        };
        let timeout = std::time::Duration::from_secs(8);
        let mismatched = ensure_mux_conn(
            &dir,
            "src",
            &mut spawn,
            timeout,
            &dir.join("other-agent.sock"),
        )
        .unwrap();
        assert_eq!(
            mismatched.source_mismatch(),
            Some(daemon_source.as_path()),
            "the handle notes the DAEMON's source on a mismatch"
        );
        // The same endpoint, hello'd with the daemon's own source: no note.
        let mut spawn2 = |_: &str| -> Result<MuxSpawn> { panic!("daemon is live") };
        let matching = ensure_mux_conn(&dir, "src", &mut spawn2, timeout, &daemon_source).unwrap();
        assert_eq!(matching.source_mismatch(), None);
        drop(matching);
        drop(mismatched);
        let (daemon, _server) = hold.take().unwrap();
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zero_linger_daemon_accepts_the_first_ref_within_spawn_grace() {
        // POSH_MUX_PERSIST=0: the daemon must not exit before the spawner's
        // first ref can land — construction arms the spawn grace, not the
        // (zero) linger — and the ref is CONFIRMED (RefAck), not
        // fire-and-forgotten. The full connect+hello+ref ladder runs against
        // the REAL mux_loop; the unref then exits with no linger at all.
        let dir = temp_base();
        let mut hold = None;
        let mut spawn = |k: &str| {
            hold = Some(start_inprocess_daemon(&dir, k, 0, (63490, 63499)));
            Ok(MuxSpawn::Spawned)
        };
        let handle = ensure_mux_conn(
            &dir,
            "zero",
            &mut spawn,
            std::time::Duration::from_secs(8),
            &dir.join("no-agent.sock"),
        )
        .expect("a zero-linger daemon must accept its first ref");
        assert_eq!(handle.key(), "zero");
        assert_eq!(handle.state(), MuxConnState::Connected);
        // Dropping the only ref exits immediately: linger 0 governs the
        // post-unref window (the spawn grace does not extend it).
        drop(handle);
        let (daemon, _server) = hold.take().unwrap();
        daemon.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fake endpoint that completes the hello and then closes WITHOUT
    /// acking the session ref — the daemon-died-pre-ack seam.
    fn fake_endpoint_closing_before_ref_ack(dir: &Path, key: &str) -> std::thread::JoinHandle<()> {
        use std::io::Write;
        let listener = UnixListener::bind(mux_socket_path_in(dir, key)).unwrap();
        let key = key.to_string();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
            let frame = read_client_frame(&mut s);
            assert_eq!(frame.tag, MuxTag::Hello);
            let ack = MuxHelloAck {
                state: MuxConnState::Connected,
                stamp: MUX_PROTO_STAMP.to_string(),
                key,
                source: PathBuf::from("/run/user/1000/agent.sock"),
            };
            s.write_all(&encode_mux_frame(MuxTag::HelloAck, &ack.encode())).unwrap();
            // Consume the SessionRef so the client's write SUCCEEDS (the
            // fire-and-forget trap), then exit without acking: the conn
            // closes with the ref unconfirmed.
            let frame = read_client_frame(&mut s);
            assert_eq!(frame.tag, MuxTag::SessionRef);
        })
    }

    #[test]
    fn unacked_session_ref_is_an_error_not_a_silent_success() {
        // The load-bearing finding-1 fix: claim_ref waits for the daemon's
        // RefAck; a daemon that dies between HelloAck and registering the
        // ref must yield Err — the signal apply_mux_gate's fallback keys on
        // — never a handle whose forwarding silently went nowhere.
        let dir = temp_base();
        let fake = fake_endpoint_closing_before_ref_ack(&dir, "dying");
        let mut spawn =
            |_: &str| -> Result<MuxSpawn> { panic!("socket exists; no spawn expected") };
        let got = ensure_mux_conn(
            &dir,
            "dying",
            &mut spawn,
            std::time::Duration::from_secs(2),
            Path::new("/run/user/1000/agent.sock"),
        );
        assert!(
            got.is_err(),
            "an unconfirmed session ref must be an Err, not a silent handle"
        );
        fake.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mux_gate_keeps_the_source_when_the_ref_is_never_acked() {
        // End-to-end through the real seam: the ensure failure from an
        // unacked ref reaches apply_mux_gate, whose fallback keeps the
        // per-connection agent source — forwarding is never silently lost.
        let dir = temp_base();
        let fake = fake_endpoint_closing_before_ref_ack(&dir, "gated");
        let source = PathBuf::from("/run/user/1000/agent.sock");
        let (agent_source, handle) = apply_mux_gate(true, Some(source.clone()), |s| {
            let mut spawn =
                |_: &str| -> Result<MuxSpawn> { panic!("socket exists; no spawn expected") };
            ensure_mux_conn(&dir, "gated", &mut spawn, std::time::Duration::from_secs(2), s)
        });
        assert_eq!(
            agent_source,
            Some(source),
            "the fallback must keep the per-connection source on an unacked ref"
        );
        assert!(handle.is_none());
        fake.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_mux_conn_concurrent_cold_start_races_to_one_daemon() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::{Arc, Barrier, Mutex};

        // The M1 E2E's two invocations are sequential; this pins the
        // genuinely CONCURRENT cold start — two invocations race on one
        // absent socket through the real spawn path (bind_or_probe deciding
        // the race exactly as run_daemon does, the winner running the real
        // mux_loop in-process): exactly one daemon is born, BOTH racers come
        // back with confirmed handles, and the endpoint holds both refs.
        let dir = Arc::new(temp_base());
        let spawned = Arc::new(AtomicU32::new(0));
        let daemon_hold = Arc::new(Mutex::new(Vec::new()));
        let barrier = Arc::new(Barrier::new(2));
        let mut racers = Vec::new();
        for _ in 0..2 {
            let dir = Arc::clone(&dir);
            let spawned = Arc::clone(&spawned);
            let daemon_hold = Arc::clone(&daemon_hold);
            let barrier = Arc::clone(&barrier);
            racers.push(std::thread::spawn(move || {
                let mut spawn = |k: &str| {
                    // run_daemon's bind seam: the bind winner BECOMES the
                    // daemon; a lost race defers to the live winner.
                    match bind_or_probe(&mux_socket_path_in(&dir, k))? {
                        MuxBind::ExistingDaemon => Ok(MuxSpawn::AlreadyRunning),
                        MuxBind::Bound(listener) => {
                            spawned.fetch_add(1, Ordering::SeqCst);
                            let ukey = crate::remote::crypto::Key::random();
                            let (server_conn, port) =
                                Connection::server((63440, 63449), &ukey, Family::Inet).unwrap();
                            let addr = format!("127.0.0.1:{port}").parse().unwrap();
                            let conn = Connection::client(addr, &ukey).unwrap();
                            let agent = dir.join("no-agent.sock");
                            let k = k.to_string();
                            let daemon = std::thread::spawn(move || {
                                mux_loop(listener, conn, &agent, 1_000, &k)
                            });
                            daemon_hold.lock().unwrap().push((daemon, server_conn));
                            Ok(MuxSpawn::Spawned)
                        }
                    }
                };
                barrier.wait();
                ensure_mux_conn(
                    &dir,
                    "cold",
                    &mut spawn,
                    std::time::Duration::from_secs(8),
                    &dir.join("no-agent.sock"),
                )
            }));
        }
        let handles: Vec<MuxHandle> = racers
            .into_iter()
            .map(|t| t.join().unwrap().expect("both racers get confirmed handles"))
            .collect();
        assert_eq!(spawned.load(Ordering::SeqCst), 1, "exactly one daemon is born");
        let mut obs = ipc_observer(&mux_socket_path_in(&dir, "cold"));
        wait_status_contains(&mut obs, "refs=2 ");
        drop(handles);
        drop(obs);
        let (daemon, _server) = {
            let mut holds = daemon_hold.lock().unwrap();
            let pair = holds.pop().unwrap();
            assert!(holds.is_empty(), "one daemon means one hold");
            pair
        };
        daemon.join().unwrap();
        std::fs::remove_dir_all(&*dir).ok();
    }

    // --- The invocation-seam gate (M1 Task 4.3): forwarding ownership. ---

    /// A MuxHandle over a dangling socketpair half, for gate tests that only
    /// care about ownership plumbing, not a live daemon.
    fn fake_handle() -> MuxHandle {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        MuxHandle {
            conn: a,
            buf: MuxFrameBuffer::default(),
            state: MuxConnState::Connected,
            key: "k".to_string(),
            source_mismatch: None,
        }
    }

    /// SshOptions around a gate outcome, for pinning the bootstrap bytes.
    fn opts_with(agent_source: Option<PathBuf>) -> crate::remote::sshwrap::SshOptions {
        crate::remote::sshwrap::SshOptions {
            family: Family::Auto,
            port_range: None,
            agent_source,
            real_ssh_agent_forward: None,
            channels: false,
            connect_timeout_secs: None,
        }
    }

    #[test]
    fn mux_bootstrap_ssh_argv_is_bounded_by_connect_timeout() {
        // The daemon's ssh bootstrap call shape: a hung destination must not
        // wedge the SHARED endpoint indefinitely, so the mux options carry
        // the bounded ConnectTimeout — pinned at the argv seam.
        let opts = mux_ssh_options(Family::Auto, None);
        assert_eq!(
            crate::remote::sshwrap::ssh_args(&opts),
            vec!["-o", "ConnectTimeout=10"]
        );
        // Family still rides ahead of the timeout, as in the session path.
        let opts = mux_ssh_options(Family::Inet, Some("60000:61000".to_string()));
        assert_eq!(
            crate::remote::sshwrap::ssh_args(&opts),
            vec!["-4", "-o", "ConnectTimeout=10"]
        );
        assert!(opts.channels, "agent channels exist only enveloped");
        assert_eq!(opts.agent_source, None, "the agent verb IS forwarding; no -A");
    }

    #[test]
    fn mux_gate_off_keeps_the_bootstrap_byte_identical() {
        // The opt-out contract (POSH_MUX=0, the post-promotion rollback
        // switch): the construction sites see exactly what
        // resolve_agent_source produced, no ensure call, no handle — and the
        // bootstrap wire string is byte-identical to the pre-M1 legacy
        // (`-A` rides when forwarding resolved on).
        let source = PathBuf::from("/run/user/1000/agent.sock");
        let (agent_source, handle) = apply_mux_gate(false, Some(source.clone()), |_| {
            panic!("mux off must never ensure an endpoint")
        });
        assert!(handle.is_none());
        assert_eq!(agent_source, Some(source));
        let cmd =
            crate::remote::sshwrap::remote_command(&opts_with(agent_source), &[], &[]);
        assert_eq!(cmd, "posh-server new -A");
        // And forwarding-off stays byte-identical too.
        let (agent_source, handle) =
            apply_mux_gate(false, None, |_| panic!("mux off must never ensure"));
        assert!(handle.is_none());
        assert_eq!(agent_source, None);
        let cmd =
            crate::remote::sshwrap::remote_command(&opts_with(agent_source), &[], &[]);
        assert_eq!(cmd, "posh-server new");
    }

    #[test]
    fn mux_gate_on_moves_forwarding_ownership_to_the_endpoint() {
        // mux on + forwarding on: the endpoint owns forwarding — the session
        // bootstrap runs with agent_source None (no `-A`, no per-session
        // srv endpoint) and the handle is held for the invocation.
        let source = PathBuf::from("/run/user/1000/agent.sock");
        let seen = std::cell::RefCell::new(None);
        let (agent_source, handle) = apply_mux_gate(true, Some(source.clone()), |s| {
            *seen.borrow_mut() = Some(s.to_path_buf());
            Ok(fake_handle())
        });
        assert_eq!(
            seen.borrow().as_deref(),
            Some(source.as_path()),
            "the endpoint inherits the invocation's resolved agent source"
        );
        assert_eq!(agent_source, None, "session bootstrap forwards nothing");
        assert!(handle.is_some(), "the ref is held for the invocation");
        let cmd =
            crate::remote::sshwrap::remote_command(&opts_with(agent_source), &[], &[]);
        assert_eq!(cmd, "posh-server new", "no -A on the session bootstrap");
    }

    #[test]
    fn mux_gate_with_forwarding_off_skips_the_endpoint_entirely() {
        // mux on but forwarding resolved off: nothing for the endpoint to
        // own — no spawn, no handle, bootstrap unchanged.
        let (agent_source, handle) = apply_mux_gate(true, None, |_| {
            panic!("no forwarding ⇒ no mux spawn at all")
        });
        assert_eq!(agent_source, None);
        assert!(handle.is_none());
    }

    #[test]
    fn mux_gate_failure_falls_back_to_per_connection_forwarding() {
        // Failure posture: any ensure_mux failure warns and proceeds with
        // per-connection forwarding exactly as today — never strand the
        // user agentless.
        let source = PathBuf::from("/run/user/1000/agent.sock");
        let (agent_source, handle) = apply_mux_gate(true, Some(source.clone()), |_| {
            Err(util::Error::from("endpoint exploded"))
        });
        assert_eq!(agent_source, Some(source), "fallback keeps the session's -A");
        assert!(handle.is_none());
        let cmd =
            crate::remote::sshwrap::remote_command(&opts_with(agent_source), &[], &[]);
        assert_eq!(cmd, "posh-server new -A");
    }

    #[test]
    fn posh_mux_is_default_on_with_the_shared_off_switch() {
        // Promotion (FDR 0014 stable bar): the gate is DEFAULT ON — unset,
        // empty, and truthy spellings all select the mux; only the explicit
        // off set disables it (no env mutation: the parser is pinned
        // directly). Same off-switch convention as POSH_SESSION_FRAMES.
        assert!(parse_mux_gate(None), "unset must select the mux");
        for v in ["", "1", "true", "TRUE", "on", "On", "yes", "YES"] {
            assert!(parse_mux_gate(Some(v)), "{v:?} must select the mux");
        }
        for v in ["0", "false", "off", "no", "FALSE", " off "] {
            assert!(!parse_mux_gate(Some(v)), "{v:?} must switch the mux off");
        }
        // Convention switch pinned deliberately: pre-promotion these were
        // outside env_value_on's truthy set and therefore OFF; under the
        // default-on off-switch shape, unrecognized values are ON.
        for v in ["2", "enable"] {
            assert!(parse_mux_gate(Some(v)), "{v:?} is not the off switch");
        }
    }

    #[test]
    fn hostname_is_nonempty_and_client_id_shape_is_safe() {
        let h = hostname();
        assert!(!h.is_empty());
        // client_id() is hostname (or override) through sanitize_id; the
        // sanitized form of whatever it returns must be itself.
        let id = client_id();
        assert!(!id.is_empty());
        assert_eq!(sanitize_id(&id), id, "client_id must already be sanitized");
    }

    // --- The M1 E2E (Task 5, docs/plans/2026-07-28-mux-endpoint-m1-impl.md):
    // the FDR 0014 promotion-criteria run for posh#136. ---
    //
    // The pre-M1 shape this answers (reproduced conceptually, not re-run):
    // each forwarded invocation stood up its own connection with a per-pid
    // `srv-<pid>.sock` endpoint, and the remote `agent/sock` was a symlink
    // ELECTED among them — so a client going idle or dying while its endpoint
    // owned the link forced a handoff, whose unusable window measured 9.9 s
    // before the posh#152 interim and zero-at-the-edge after (see
    // `remote::agent::tests::handoff_repoints_to_the_active_sibling_on_the_inactivity_edge`
    // — still an election among per-connection processes, explicitly not the
    // FDR 0014 bar).
    //
    // Under POSH_MUX (M1) both invocations share ONE mux endpoint whose
    // agent-only connection is the destination's sole agent-capable
    // connection, so from a single client host `agent/sock` has exactly one
    // owner BY CONSTRUCTION: a client departing is a local refcount decrement
    // the remote never even observes. The FDR 0014 bar this proves: with two
    // forwarded invocations SHARING the endpoint — sequential ensure calls,
    // the common shape of a second invocation joining a live endpoint; the
    // cold-start CONCURRENT race is pinned separately by
    // `ensure_mux_conn_concurrent_cold_start_races_to_one_daemon` — a real
    // `ssh-add -l` through the remote `agent/sock` succeeds; after one
    // client is killed, the OTHER's forwarding keeps working with ZERO
    // handoff window — every probe from the instant of departure succeeds
    // (no SSH_AGENT_FAILURE, which would surface as a failed `ssh-add -l`),
    // and the symlink target never changes.
    //
    // Real components, per the suite's layering (server.rs agent E2Es): a
    // real `ssh-agent` holding a real key behind the daemon's proxy; the REAL
    // `posh server agent --client-id ...` BINARY as the remote — spawned over
    // loopback via its own `POSH CONNECT` handshake, the suite's stand-in for
    // the ssh bootstrap (whose command bytes sshwrap's tests pin) — and the
    // REAL `mux_loop` + `ensure_mux_conn` client half as the two invocations
    // (the spawn seam stands in for run_daemon's fork, exactly as the Task 4
    // tests drive it). #[ignore]: needs the posh binary + ssh tooling, absent
    // from the hermetic sandbox; run with `just debug-agent-e2e`.
    #[test]
    #[ignore = "mux M1 E2E; needs the posh binary + ssh tooling; run with --ignored"]
    fn agent_forward_mux_m1_two_sequential_invocations_one_owner_zero_handoff_window() {
        use std::io::BufRead;
        use std::process::{Command, Stdio};

        // The posh binary cargo builds alongside this test.
        let posh_bin = {
            let exe = std::env::current_exe().expect("current_exe");
            exe.parent()
                .and_then(|p| p.parent())
                .expect("target/<profile> dir")
                .join("posh")
        };
        assert!(
            posh_bin.exists(),
            "posh binary not found at {posh_bin:?} (cargo test should have built it)"
        );

        let local_base = temp_base(); // mux socket + the real agent's socket
        let remote_base = temp_base(); // the remote server's POSH_DIR

        // (1) A real ssh-agent holding an ephemeral key; the fingerprint
        // `ssh-add -l` must report through the forwarded path.
        let key_path = local_base.join("id_ed25519");
        assert!(
            Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-N", "", "-C", "posh-mux-m1-e2e", "-q", "-f"])
                .arg(&key_path)
                .status()
                .expect("run ssh-keygen")
                .success(),
            "ssh-keygen failed"
        );
        let fp_out = Command::new("ssh-keygen")
            .arg("-lf")
            .arg(key_path.with_extension("pub"))
            .output()
            .expect("run ssh-keygen -lf");
        let fp_text = String::from_utf8_lossy(&fp_out.stdout);
        let fingerprint = fp_text
            .split_whitespace()
            .find(|t| t.starts_with("SHA256:"))
            .expect("a SHA256 fingerprint")
            .to_string();
        let real_agent_sock = local_base.join("real-agent.sock");
        let mut agent = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&real_agent_sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ssh-agent");
        let deadline = now_ms() + 5_000;
        while !real_agent_sock.exists() {
            assert!(now_ms() < deadline, "ssh-agent never bound its socket");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            Command::new("ssh-add")
                .arg(&key_path)
                .env("SSH_AUTH_SOCK", &real_agent_sock)
                .status()
                .expect("run ssh-add")
                .success(),
            "ssh-add failed to load the key into the real agent"
        );

        // (2) The REAL agent-only remote: `posh server agent --client-id`
        // (the exact tail run_daemon's ssh bootstrap execs as `posh-server
        // agent ...`), detached via its own POSH CONNECT handshake.
        let mut server = Command::new(&posh_bin)
            .args(["server", "-p", "63500:63549", "agent", "--client-id", "m1e2e"])
            .env("LC_ALL", "C.UTF-8")
            .env("POSH_DIR", &remote_base)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn posh server agent");
        let connect = std::io::BufReader::new(server.stdout.take().expect("server stdout piped"))
            .lines()
            .map_while(|line| line.ok())
            .find(|l| l.starts_with("POSH CONNECT "))
            .expect("posh server agent printed POSH CONNECT");
        let _ = server.wait(); // the parent exits right after the double-fork
        let mut fields = connect
            .strip_prefix("POSH CONNECT ")
            .expect("POSH CONNECT prefix")
            .split_whitespace();
        let port: u16 = fields.next().expect("port").parse().expect("port number");
        let udp_key = crate::remote::crypto::Key::from_base64(fields.next().expect("key"))
            .expect("valid base64 key");

        // (3) The mux endpoint: the REAL mux_loop on a thread (run_daemon's
        // grandchild body; the fork + ssh bootstrap are what (2) stands in
        // for), dialing the real remote over loopback and proxying the real
        // ssh-agent. Short linger so teardown can join the thread.
        let mut daemon = None;
        let mut spawned = 0u32;
        let mut spawn = |k: &str| {
            spawned += 1;
            let listener = UnixListener::bind(mux_socket_path_in(&local_base, k)).unwrap();
            let addr = format!("127.0.0.1:{port}").parse().unwrap();
            let conn = Connection::client(addr, &udp_key).unwrap();
            let agent_sock = real_agent_sock.clone();
            let k = k.to_string();
            daemon = Some(std::thread::spawn(move || {
                mux_loop(listener, conn, &agent_sock, 1_000, &k)
            }));
            Ok(MuxSpawn::Spawned)
        };

        // (4) TWO forwarded invocations, ensured sequentially — the second
        // joins the live endpoint (the common shape); two session refs on
        // the one endpoint through the real client half. One spawn only.
        // (The cold-start concurrent race is covered by
        // `ensure_mux_conn_concurrent_cold_start_races_to_one_daemon`.)
        let timeout = std::time::Duration::from_secs(20);
        let invocation1 =
            ensure_mux_conn(&local_base, "m1dest", &mut spawn, timeout, &real_agent_sock)
                .unwrap();
        let invocation2 =
            ensure_mux_conn(&local_base, "m1dest", &mut spawn, timeout, &real_agent_sock)
                .unwrap();
        assert_eq!(spawned, 1, "both invocations share the one endpoint");

        // (5) A real `ssh-add -l` through the remote agent/sock lists the
        // key (retrying through the come-up window: the first heartbeat must
        // reach the remote before it can open channels toward us).
        let forwarded_sock = remote_base.join("agent").join("sock");
        let lists_key = || -> (bool, String) {
            let out = Command::new("timeout")
                .arg("4")
                .arg("ssh-add")
                .arg("-l")
                .env("SSH_AUTH_SOCK", &forwarded_sock)
                .output()
                .expect("run ssh-add -l");
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            (out.status.success() && stdout.contains(&fingerprint), stdout)
        };
        let deadline = now_ms() + 15_000;
        let mut came_up = false;
        let mut last = String::new();
        while now_ms() < deadline {
            let (ok, out) = lists_key();
            last = out;
            if ok {
                came_up = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }

        // The structural claim: agent/sock is owned by the single mux
        // endpoint (deterministically named), and no per-connection
        // srv-<pid> endpoint exists to elect among.
        let owner_before = std::fs::read_link(&forwarded_sock).ok();
        let srv_endpoints: Vec<String> = std::fs::read_dir(remote_base.join("agent"))
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| n.starts_with("srv-"))
                    .collect()
            })
            .unwrap_or_default();

        // (6) Kill one client: dropping the handle closes its IPC conn —
        // the invocation dying and the invocation idling are the same event
        // to the endpoint (auto-unref). The remote observes NOTHING.
        drop(invocation1);

        // (7) Zero handoff window: from the instant of departure, EVERY
        // probe through agent/sock must keep succeeding — a single failed
        // `ssh-add -l` here is the posh#136 SSH_AGENT_FAILURE — and the
        // symlink target must never change (no election, no repoint).
        let mut survived = came_up;
        let mut probes = 0u32;
        let mut owner_moved = false;
        let post_kill_deadline = now_ms() + 2_000;
        while now_ms() < post_kill_deadline {
            let (ok, out) = lists_key();
            probes += 1;
            if !ok {
                survived = false;
                last = out;
                break;
            }
            if std::fs::read_link(&forwarded_sock).ok() != owner_before {
                owner_moved = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let owner_after = std::fs::read_link(&forwarded_sock).ok();

        // (8) Teardown before asserting. Dropping the second ref arms the
        // 1 s linger; the daemon thread exits on its expiry. The detached
        // remote is then SIGTERMed via its recorded pid (the mux-<id>.pid
        // liveness file) rather than waiting out its 60 s peer timeout.
        drop(invocation2);
        if let Some(d) = daemon {
            let _ = d.join();
        }
        if let Ok(pid) = std::fs::read_to_string(remote_base.join("agent").join("mux-m1e2e.pid"))
        {
            if let Ok(pid) = pid.trim().parse::<i32>() {
                // SAFETY: plain kill(2) on a recorded pid; no memory involved.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
        let _ = agent.kill();
        let _ = agent.wait();
        std::fs::remove_dir_all(&local_base).ok();
        std::fs::remove_dir_all(&remote_base).ok();

        assert!(
            came_up,
            "ssh-add -l via the mux-owned agent/sock never listed the key \
             (fingerprint {fingerprint}); last stdout: {last:?}"
        );
        assert_eq!(
            owner_before.as_deref().and_then(|p| p.to_str()),
            Some("mux-m1e2e.sock"),
            "agent/sock is owned by the single mux endpoint"
        );
        assert!(
            srv_endpoints.is_empty(),
            "no per-connection srv endpoints exist to elect among: {srv_endpoints:?}"
        );
        assert!(
            survived,
            "ssh-add -l failed after the first client departed (probe {probes}) — \
             the posh#136 window reappeared; last stdout: {last:?}"
        );
        assert!(probes > 0, "the post-departure window was actually probed");
        assert!(
            !owner_moved && owner_after == owner_before,
            "agent/sock changed owner across the departure ({owner_before:?} -> \
             {owner_after:?}); ownership must be structural, not elected"
        );
    }
}
