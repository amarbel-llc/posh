//! RFC 0001 §3: the TLV capability table that rides behind the EXTENSION
//! bit (0x02) of both datagram directions. Unknown ids are preserved on
//! decode and ignored by consumers; malformed tables reject the message.

use crate::error::{Error, Result};

/// Reserved flags bit (both directions): a capability table follows the
/// flags byte. Permanent; never reuse for anything else.
pub const FLAG_EXTENSION: u8 = 0x02;

/// Capability ids (RFC 0001 registry). 224..=255 are experimental-only.
pub const CAP_PROTOCOL_VERSION: u8 = 0;
pub const CAP_EXIT_STATUS: u8 = 1;
/// Scrollback sync (RFC 0002 §1). Client entry: 1-byte payload requesting a
/// ring depth in units of 256 rows (`0` = server default), advertising that
/// the client understands the `BODY_SCROLLBACK` frame body. Server entry:
/// empty payload, acknowledging it will emit scrollback bodies.
pub const CAP_SCROLLBACK: u8 = 3;
/// Incremental frame sync (#15, prototype). Client entry (empty payload):
/// "I understand the `BODY_MORPH` frame body and will apply its forward
/// escape-delta to my existing terminal model instead of reparsing a full
/// dump". The client advertises this only behind the `POSH_FRAMESYNC=morph`
/// opt-in, so a default session never negotiates it and the byte stream is
/// unchanged. The server emits Morph bodies only when the peer advertised
/// this; a `Full` keyframe is always the fallback (first frame, base
/// mismatch, alt-screen/resize transitions).
pub const CAP_MORPH: u8 = 4;
/// Base-integrity checksums (RFC 0006). Client entry (empty payload): "I verify
/// the diff base of a `BODY_DIFF_SUM`/`BODY_MORPH_SUM` body against my own held
/// dump before applying, and re-ack + request a resync on a mismatch instead of
/// reconstructing against a divergent base". The server emits the checksummed
/// body variants only when the peer advertised this; a baseline peer keeps
/// receiving plain `BODY_DIFF`/`BODY_MORPH`. Catches a base divergence that the
/// content-blind prefix/suffix diff would otherwise mis-apply (#94) or wedge on.
pub const CAP_BASE_SUM: u8 = 5;
/// SSH agent forwarding (FDR 0004). Both directions, empty payload: "I
/// participate in agent forwarding on this connection." Per RFC 0001's rules a
/// side MUST NOT send [`CAP_AGENT_DATA`]/[`CAP_AGENT_ACK`] until it has seen
/// this from the peer; baseline peers skip the unknown id untouched. Ids
/// 6/7/8, not the 2/3/4 the original design proposed — 3/4/5 were taken by
/// SCROLLBACK/MORPH/BASE_SUM, and 2 is held for the anticipated TERM_FEATURES.
///
/// The three agent ids and the chunk helpers below carry `#[allow(dead_code)]`
/// until their callers land: the remote endpoint negotiates and serves them
/// (FDR 0004 work item 3), the client proxy drives them (item 4).
#[allow(dead_code)]
pub const CAP_AGENT_FORWARD: u8 = 6;
/// One contiguous chunk of the sender's agent byte stream (FDR 0004). Payload
/// is a `u64` big-endian stream offset followed by up to [`AGENT_DATA_MAX`]
/// bytes. Multiple entries may appear in one message; their offsets MUST be
/// contiguous within the message. The agent stream itself is the reliable
/// cumulative byte stream (`sync::AgentStream`); these entries are just how it
/// rides inside `ClientMessage`/`ServerFrame` without a body-format change.
#[allow(dead_code)]
pub const CAP_AGENT_DATA: u8 = 7;
/// Cumulative ack of the peer's agent stream (FDR 0004): payload is a single
/// `u64` big-endian offset, one past the last contiguous byte received.
#[allow(dead_code)]
pub const CAP_AGENT_ACK: u8 = 8;
/// Lossy-relay marker (RFC 0008 §3, the Phase 3 frame relay). Client entry
/// (empty payload): "I relay your frames onto a LOSSY link (encrypted UDP to a
/// remote posh client), so do NOT self-ack me — advance my diff base only on a
/// forwarded `Tag::FrameAck`." The session daemon's per-client `FrameProducer`
/// then runs in the same ack-gated, base-anchored mode it uses for a real UDP
/// peer (each new frame supersedes the last unacked one, so the relay keeps only
/// O(1) retransmit state). A reliable local client never sends this, so its
/// stream stays byte-identical to today (self-acked, always-diff, DumpDiff). Id 9
/// — the next free low id after the agent trio (6/7/8); id 2 stays reserved for
/// the anticipated TERM_FEATURES.
pub const CAP_LOSSY: u8 = 9;

/// Scrollback stream v2 (RFC 0009 §1): scrollback leaves the visible frame
/// sequence and gains its own cumulative acknowledgement. Client entry: 10-byte
/// payload — ring depth u8 (256-row units, `0` = server default, as RFC 0002),
/// epoch u8 (the epoch the client is accumulating in), acked_sb_rows u64 LE
/// (cumulative rows accepted this epoch) — the per-message scrollback ack.
/// Server entry: 2-byte payload {0x02, epoch u8} acknowledging v2 and naming
/// the current epoch (bumped on a reflow-invalidating reset, e.g. a width
/// change; the client clears its ring and zeroes its count on a bump).
pub const CAP_SCROLLBACK2: u8 = 10;

/// Kitty keyboard capability of the client's REAL terminal (RFC 0010). Client
/// entry: 1-byte payload carrying the kitty keyboard progressive-enhancement
/// flag set the client's outer terminal supports, as the low 5 bits
/// (disambiguate=1, report-events=2, report-alternate=4, report-all=8,
/// report-text=16). A `0` payload means "the terminal implements the kitty
/// keyboard protocol but reports no enhancement flags"; the cap's ABSENCE means
/// "unknown / not advertised" (a terminal without the protocol MUST NOT
/// advertise it). The daemon answers the in-session app's `CSI ? u` query from
/// the effective (conservative-intersection across attached frame clients)
/// value, since under frame transport the raw query never reaches the real
/// terminal. Complements FDR 0013 (the outbound flag mirror).
pub const CAP_KITTY_KEYBOARD: u8 = 11;

/// Local write-buffer coalescing (posh#137). Client entry (empty payload): "I am
/// a local stream-socket client — do NOT self-ack me; advance my diff base only
/// on my `Tag::FrameAck`, and coalesce my queued visible frames so a burst
/// cannot grow `write_buf` past `MAX_CLIENT_BACKLOG` and get me dropped." Unlike
/// [`CAP_LOSSY`] (the UDP relay, which also selects MorphDelta, stamps
/// `base_sum`, and runs lossy scrollback), a coalescing client keeps the plain
/// local semantics: DumpDiff, no `base_sum`, reliable in-order scrollback. It
/// exists so the local client gets the mosh-style latest-state-only bound
/// WITHOUT inheriting the relay's wire-negotiated behaviors. The daemon
/// truncates a still-un-sent trailing visible frame and re-encodes the latest
/// against the acked base rather than appending a second (never touching a
/// partially-sent frame). Runtime-toggleable via [`FRAME_ACK_COALESCE_OFF`], so
/// a wedged coalescing path can fall back to today's self-ack+append behavior
/// without dropping the session. Id 12 — next free after
/// [`CAP_KITTY_KEYBOARD`]. See auto-memory posh-client-backlog-disconnect.
pub const CAP_COALESCE: u8 = 12;

/// Mask a received [`CAP_KITTY_KEYBOARD`] payload to the valid low-5-bit flag
/// range (RFC 0010 Security Considerations): a malformed or oversized payload is
/// treated as "capability absent" (`None`), never trusted out of range.
pub fn decode_kitty_keyboard(payload: &[u8]) -> Option<u8> {
    match payload {
        [flags] => Some(flags & 0x1f),
        _ => None,
    }
}

/// The client's [`CAP_SCROLLBACK2`] entry, decoded (RFC 0009 §1/§3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scrollback2Client {
    pub ring_depth: u8,
    pub epoch: u8,
    pub acked_rows: u64,
}

/// Encode the client's per-message SCROLLBACK2 entry (advertisement + ack).
pub fn encode_scrollback2_client(c: &Scrollback2Client) -> Cap {
    let mut payload = Vec::with_capacity(10);
    payload.push(c.ring_depth);
    payload.push(c.epoch);
    payload.extend_from_slice(&c.acked_rows.to_le_bytes());
    Cap {
        id: CAP_SCROLLBACK2,
        payload,
    }
}

/// Decode a client SCROLLBACK2 payload. Exactly 10 bytes; anything else is
/// malformed (authenticated peer: corruption or an unknown future version) and
/// the entry is ignored by the consumer.
pub fn decode_scrollback2_client(payload: &[u8]) -> Result<Scrollback2Client> {
    if payload.len() != 10 {
        return Err(Error::from("SCROLLBACK2 client payload is not 10 bytes"));
    }
    Ok(Scrollback2Client {
        ring_depth: payload[0],
        epoch: payload[1],
        acked_rows: u64::from_le_bytes(payload[2..10].try_into().unwrap()),
    })
}

/// Encode the server's SCROLLBACK2 acknowledgement: version byte 0x02 + the
/// current epoch.
pub fn encode_scrollback2_ack(epoch: u8) -> Cap {
    Cap {
        id: CAP_SCROLLBACK2,
        payload: vec![0x02, epoch],
    }
}

/// Decode a server SCROLLBACK2 acknowledgement, returning the epoch. The
/// version byte must be 0x02.
pub fn decode_scrollback2_ack(payload: &[u8]) -> Result<u8> {
    if payload.len() != 2 || payload[0] != 0x02 {
        return Err(Error::from("SCROLLBACK2 ack payload is not {0x02, epoch}"));
    }
    Ok(payload[1])
}

/// Max agent-stream bytes carried by one [`CAP_AGENT_DATA`] entry: the table's
/// `len: u8` budget (255) minus the 8-byte `u64` offset prefix. Keeping agent
/// data as length-prefixed entries (rather than a negotiated body-layout
/// change) leaves the message bodies byte-identical in every negotiation
/// state, at ~1.2% framing overhead.
#[allow(dead_code)]
pub const AGENT_DATA_MAX: usize = u8::MAX as usize - AGENT_DATA_OFFSET_LEN; // 247

/// Bytes of `u64` big-endian stream offset prefixing every [`CAP_AGENT_DATA`]
/// payload. Subtract it from an entry's payload length to get the agent bytes
/// it carries — which is how the senders count what they actually emitted for
/// the posh#142 telemetry, since a payload is never shorter than this prefix.
pub const AGENT_DATA_OFFSET_LEN: usize = 8;

/// Max [`CAP_AGENT_DATA`] entries one message carries. The table length is a
/// `count: u8`, so the whole table (agent data + protocol version + scrollback +
/// any other caps) must stay under 256 entries; emitting the entire unacked
/// agent tail unbounded would overflow that count and silently corrupt the
/// frame. We cap agent data well below 255 to leave headroom for the handful
/// of non-agent caps, bounding one message at ~`MAX_AGENT_DATA_CAPS *
/// AGENT_DATA_MAX` ≈ 59 KB of agent bytes; the unsent tail rides the next
/// message (the stream is cumulative, so a prefix is always valid).
#[allow(dead_code)]
pub const MAX_AGENT_DATA_CAPS: usize = 239;

/// Server transport-state piggyback (#6, diagnostic). Experimental id (224, the
/// bottom of RFC 0001's 224..=255 experimental range) rather than a registered
/// low id: this is a debug-only, off-by-default aid whose payload layout may
/// change, so it intentionally avoids the stable registry. Client entry (empty
/// payload): "attach your live transport state to each frame so my SIGUSR2 dump
/// can show both sides of a wedge." A remote `posh-server` has no local socket
/// to `SIGUSR2`, so its `current_num`/`acked_num`/`outstanding`/`term_gen`/
/// `pty_open` are otherwise a blind spot when triaging a stall from the client.
/// The client advertises this in a debug posture (POSH_DEBUG_LOG set, or
/// POSH_WEDGE_WATCHDOG explicitly on — its #117 default-on state does not
/// count) and whenever agent forwarding is active (FDR 0004: to
/// power the agent-forwarding diagnostic with the server endpoint's state), so a
/// default session — no debug, no forwarding — never negotiates it and pays no
/// per-frame overhead. Server entry: a [`ServerDiag`] payload (see
/// [`encode_server_diag`]); its v2 form also carries the [`AgentDiag`] endpoint
/// state.
pub const CAP_DIAG: u8 = 224;

/// Evolved-predictor metric forwarding (RFC 0007 §3). Experimental id (like
/// CAP_DIAG) because the payload tracks the metric-vector schema and may evolve.
/// Client entry (empty payload): "I run an evolved GP predictor; attach the
/// remote-host metric terminals to each frame." Advertised only when a GP
/// species is active, so a default session never negotiates it. Server entry:
/// the [`encode_metrics`] payload (load/mem/frontmost-app/proc-count/fg-proc,
/// plus the v2 server counters retransmit-rate/dump-vt-us).
pub const CAP_METRICS: u8 = 225;

/// The post-table format version we implement (payload of
/// [`CAP_PROTOCOL_VERSION`]).
pub const PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cap {
    pub id: u8,
    pub payload: Vec<u8>,
}

/// count:u8, then count × (id:u8, len:u8, payload).
pub fn encode_table(caps: &[Cap]) -> Vec<u8> {
    debug_assert!(caps.len() <= u8::MAX as usize);
    let mut out = vec![caps.len() as u8];
    for c in caps {
        debug_assert!(c.payload.len() <= u8::MAX as usize);
        out.push(c.id);
        out.push(c.payload.len() as u8);
        out.extend_from_slice(&c.payload);
    }
    out
}

/// Parses a table from the head of `data`; returns the entries and the
/// number of bytes consumed. Bounds-checked: count/len are peer-controlled
/// (RFC 0001 security considerations) — truncation is an error, never an
/// over-read or panic.
pub fn decode_table(data: &[u8]) -> Result<(Vec<Cap>, usize)> {
    let Some(&count) = data.first() else {
        return Err(Error::from("capability table truncated"));
    };
    let mut caps = Vec::with_capacity(count as usize);
    let mut at = 1;
    for _ in 0..count {
        let (Some(&id), Some(&len)) = (data.get(at), data.get(at + 1)) else {
            return Err(Error::from("capability entry truncated"));
        };
        at += 2;
        let end = at + len as usize;
        let Some(payload) = data.get(at..end) else {
            return Err(Error::from("capability payload truncated"));
        };
        caps.push(Cap {
            id,
            payload: payload.to_vec(),
        });
        at = end;
    }
    Ok((caps, at))
}

/// The table this build sends in every message: protocol version plus the
/// given direction-specific capabilities.
pub fn own_table(extra: &[Cap]) -> Vec<Cap> {
    let mut t = vec![Cap {
        id: CAP_PROTOCOL_VERSION,
        payload: vec![PROTOCOL_VERSION],
    }];
    t.extend_from_slice(extra);
    t
}

pub fn find(caps: &[Cap], id: u8) -> Option<&Cap> {
    caps.iter().find(|c| c.id == id)
}

/// Every entry with the given id, in table order. Agent data arrives as
/// multiple [`CAP_AGENT_DATA`] entries per message (unlike the at-most-once
/// caps [`find`] serves), so consumers iterate these in order to rebuild the
/// contiguous chunk run.
#[allow(dead_code)]
pub fn find_all(caps: &[Cap], id: u8) -> impl Iterator<Item = &Cap> {
    caps.iter().filter(move |c| c.id == id)
}

// ---------------------------------------------------------------------------
// Agent-stream chunking (FDR 0004). Agent bytes ride inside the existing
// caps table as length-prefixed entries, so the message bodies never change
// shape. These helpers convert between a `(stream offset, bytes)` view of the
// agent stream and the AGENT_DATA/AGENT_ACK cap payloads — they know nothing
// about `sync::AgentStream`, exactly as the rest of this module knows nothing
// about its consumers.

/// Splits a contiguous agent-stream run starting at `base` into AGENT_DATA
/// caps of at most [`AGENT_DATA_MAX`] bytes each, with contiguous offsets. An
/// empty `data` produces no entries (nothing to (re)transmit). At most
/// [`MAX_AGENT_DATA_CAPS`] entries are emitted: the table count is a `u8`, so a
/// large unacked tail is sent as a prefix here and the rest rides the next
/// message (the stream is cumulative — a prefix is always a valid send, and
/// retransmission carries the remainder). Without this cap a >~59 KB pending
/// buffer would produce >255 entries and overflow the table's count byte.
#[allow(dead_code)]
pub fn encode_agent_data(base: u64, data: &[u8]) -> Vec<Cap> {
    let mut out = Vec::new();
    let mut offset = base;
    for chunk in data.chunks(AGENT_DATA_MAX).take(MAX_AGENT_DATA_CAPS) {
        let mut payload = Vec::with_capacity(8 + chunk.len());
        payload.extend_from_slice(&offset.to_be_bytes());
        payload.extend_from_slice(chunk);
        out.push(Cap {
            id: CAP_AGENT_DATA,
            payload,
        });
        offset += chunk.len() as u64;
    }
    out
}

/// Reads one AGENT_DATA payload into its `(offset, bytes)` view. A payload
/// shorter than the 8-byte offset prefix is malformed (the peer is
/// authenticated, so this is corruption, not an attack to absorb).
#[allow(dead_code)]
pub fn decode_agent_data(payload: &[u8]) -> Result<(u64, &[u8])> {
    let Some(head) = payload.get(..8) else {
        return Err(Error::from("AGENT_DATA payload shorter than offset prefix"));
    };
    let offset = u64::from_be_bytes(head.try_into().unwrap());
    Ok((offset, &payload[8..]))
}

/// The AGENT_ACK cap for a cumulative-ack offset.
#[allow(dead_code)]
pub fn encode_agent_ack(offset: u64) -> Cap {
    Cap {
        id: CAP_AGENT_ACK,
        payload: offset.to_be_bytes().to_vec(),
    }
}

/// Reads an AGENT_ACK payload (a bare `u64`). A wrong-length payload is
/// malformed.
#[allow(dead_code)]
pub fn decode_agent_ack(payload: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = payload
        .try_into()
        .map_err(|_| Error::from("AGENT_ACK payload is not a u64"))?;
    Ok(u64::from_be_bytes(bytes))
}

// ---------------------------------------------------------------------------
// Server transport-state piggyback (#6). The server's live frame/ack/pty state
// rides one CAP_DIAG entry per frame so the client's SIGUSR2 dump can show the
// far side of a wedge it cannot SIGUSR2 directly. A plain fixed-layout payload
// (no length-prefixed chunking like agent data) — it is always one entry.

/// The server transport state the client mirrors into its dump (#6). These are
/// exactly the fields the server's own SIGUSR2 dump reports that the client
/// cannot otherwise see: is the server still producing frames (`current_num`
/// advancing), what does it think we have acked (`acked_num`), how many frames
/// are in flight / being retransmitted (`outstanding`), is its terminal still
/// changing (`term_gen`), and is the shell still alive (`pty_open`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerDiag {
    pub current_num: u64,
    pub acked_num: u64,
    pub term_gen: u64,
    pub outstanding: u32,
    pub pty_open: bool,
    /// The server process's pid (#83 debug): lets a client wedge be mapped to
    /// the exact remote `posh-server` log for a session, without a port→pid
    /// lookup on the server host. 0 from a pre-pid (v1/v2) server.
    pub pid: u32,
    /// The server's agent-forwarding endpoint state (FDR 0004), present
    /// when the server is forwarding (it has an `AgentEndpoint`). `None` from a
    /// server with forwarding disabled, or a v1 (transport-only) payload.
    pub agent: Option<AgentDiag>,
}

/// The server `AgentEndpoint` state the client mirrors into its agent-forwarding
/// diagnostic (FDR 0004): how many channels are live, the next channel
/// id it will assign, and whether `agent/sock` still points at the server's own
/// socket (false once a roam/takeover stole it, or if it is missing/dangling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentDiag {
    pub live_channels: u32,
    /// Also the cumulative count of channels ever opened, plus one (ids start at
    /// 1 and never repeat). On an idle connection this MUST stay put — a steadily
    /// climbing value with no agent use is the shipped signal for a spurious-open
    /// regression like posh#147, which went unnoticed only because nobody read it.
    pub next_channel_id: u32,
    pub symlink_ok: bool,
    /// Cumulative agent-stream bytes handed to the wire, **including re-sends**.
    pub bytes_sent: u64,
    /// Cumulative DISTINCT agent-stream bytes ever queued. `bytes_sent` minus
    /// this is what cumulative-only acknowledgement costs: the unacked tail is
    /// re-encoded onto every message until the peer acks it, so on a lossy path
    /// the difference is the retransmission overhead a selective ack would avoid
    /// (posh#142). Equal values mean nothing was ever re-sent.
    pub bytes_queued: u64,
}

impl AgentDiag {
    /// Agent bytes put on the wire that the peer had already been sent — what
    /// cumulative-only acknowledgement has cost (posh#142).
    ///
    /// The saturation is load-bearing, not defensive dressing: bytes can be
    /// QUEUED and never emitted (a connection that dies before its next
    /// message), which makes `queued` exceed `sent`. Nothing was re-sent in that
    /// case, and zero is the honest answer.
    pub fn resent(&self) -> u64 {
        self.bytes_sent.saturating_sub(self.bytes_queued)
    }
}

/// Versioned by length. The 29-byte transport core is current_num | acked_num |
/// term_gen (u64 BE each) | outstanding (u32 BE) | pty_open (u8). A u32 BE `pid`
/// (#83) appends 4 bytes; the [`AgentDiag`] (FDR 0004, when `d.agent` is `Some`)
/// appends 9, and its stream counters (posh#142) a further 16 (two u64 BE). This
/// encoder always writes the pid and, with an agent, always the counters — so it
/// emits 33 bytes (no agent) or 58 (with agent); the decoder still accepts the
/// older 29/38/42. Well under the 255-byte cap budget.
///
/// A peer too old to know length 58 drops the whole record rather than the two
/// new fields — the same trade the agent block itself made when it introduced
/// 38/42. Acceptable for `CAP_DIAG` specifically: it is an experimental-range id
/// (224) carrying diagnostics, so losing a report on version skew costs
/// visibility, never correctness.
pub fn encode_server_diag(d: &ServerDiag) -> Cap {
    let mut payload = Vec::with_capacity(58);
    payload.extend_from_slice(&d.current_num.to_be_bytes());
    payload.extend_from_slice(&d.acked_num.to_be_bytes());
    payload.extend_from_slice(&d.term_gen.to_be_bytes());
    payload.extend_from_slice(&d.outstanding.to_be_bytes());
    payload.push(d.pty_open as u8);
    payload.extend_from_slice(&d.pid.to_be_bytes());
    if let Some(a) = &d.agent {
        payload.extend_from_slice(&a.live_channels.to_be_bytes());
        payload.extend_from_slice(&a.next_channel_id.to_be_bytes());
        payload.push(a.symlink_ok as u8);
        payload.extend_from_slice(&a.bytes_sent.to_be_bytes());
        payload.extend_from_slice(&a.bytes_queued.to_be_bytes());
    }
    Cap {
        id: CAP_DIAG,
        payload,
    }
}

/// Reads a [`ServerDiag`] payload. Five valid shapes (by length): 29 = core
/// only (pre-pid); 33 = core + pid; 38 = core + agent (pre-pid); 42 = core +
/// pid + agent (pre-counters); 58 = core + pid + agent + stream counters. A
/// pid-less peer decodes with `pid = 0`; a counter-less peer decodes with zeroed
/// byte counters, which read as "nothing sent, nothing re-sent" and so cannot be
/// mistaken for a measurement. Any other length is malformed (the peer is
/// authenticated, so this is corruption or an unknown future version, not an
/// attack to absorb) and is dropped by the consumer.
pub fn decode_server_diag(payload: &[u8]) -> Result<ServerDiag> {
    let u64_at = |o: usize| u64::from_be_bytes(payload[o..o + 8].try_into().unwrap());
    let u32_at = |o: usize| u32::from_be_bytes(payload[o..o + 4].try_into().unwrap());
    // (pid, agent-block offset, counters present) keyed on the payload length.
    let (pid, agent_off, counters) = match payload.len() {
        29 => (0u32, None, false),
        33 => (u32_at(29), None, false),
        38 => (0u32, Some(29usize), false),
        42 => (u32_at(29), Some(33usize), false),
        58 => (u32_at(29), Some(33usize), true),
        _ => return Err(Error::from("CAP_DIAG payload is not 29/33/38/42/58 bytes")),
    };
    let agent = agent_off.map(|o| AgentDiag {
        live_channels: u32_at(o),
        next_channel_id: u32_at(o + 4),
        symlink_ok: payload[o + 8] != 0,
        bytes_sent: if counters { u64_at(o + 9) } else { 0 },
        bytes_queued: if counters { u64_at(o + 17) } else { 0 },
    });
    Ok(ServerDiag {
        current_num: u64_at(0),
        acked_num: u64_at(8),
        term_gen: u64_at(16),
        outstanding: u32_at(24),
        pty_open: payload[28] != 0,
        pid,
        agent,
    })
}

/// Number of `f64` metric fields in a [`CAP_METRICS`] payload (RFC 0007 §3,
/// in order): load1, mem_avail_frac, frontmost_app, proc_count, fg_proc_id,
/// then the v2 server-side counters retransmit_rate (server retransmits/sec
/// over the sample window) and dump_vt_us (the server's most-recent frame-dump
/// cost). Absent terminals are encoded as `NaN`.
pub const METRICS_FIELDS: usize = 7;
/// `CAP_METRICS` payload version. v2 appended the two server-side counters
/// (retransmit_rate, dump_vt_us) after v1's five `remote_*` host terminals.
const METRICS_VERSION: u8 = 2;

/// Encode the remote metric terminals as a [`CAP_METRICS`] payload: a version
/// byte then `METRICS_FIELDS` little-endian `f64`s. Categorical ids are passed
/// as their `f64` value; absent terminals as `NaN`.
pub fn encode_metrics(fields: [f64; METRICS_FIELDS]) -> Cap {
    let mut payload = Vec::with_capacity(1 + 8 * METRICS_FIELDS);
    payload.push(METRICS_VERSION);
    for f in fields {
        payload.extend_from_slice(&f.to_le_bytes());
    }
    Cap {
        id: CAP_METRICS,
        payload,
    }
}

/// Decode a [`CAP_METRICS`] payload. Returns `None` on a version mismatch or a
/// short payload (the client then keeps its previous values), never panicking
/// on peer-controlled bytes.
pub fn decode_metrics(payload: &[u8]) -> Option<[f64; METRICS_FIELDS]> {
    if payload.first().copied()? != METRICS_VERSION {
        return None;
    }
    let body = payload.get(1..1 + 8 * METRICS_FIELDS)?;
    let mut out = [f64::NAN; METRICS_FIELDS];
    for (slot, chunk) in out.iter_mut().zip(body.chunks_exact(8)) {
        *slot = f64::from_le_bytes(chunk.try_into().unwrap());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_roundtrip_preserves_values_and_nan() {
        let fields = [0.5, f64::NAN, 12345.0, 3.0, 67890.0, 7.5, 250.0];
        let cap = encode_metrics(fields);
        assert_eq!(cap.id, CAP_METRICS);
        let got = decode_metrics(&cap.payload).unwrap();
        assert_eq!(got[0], 0.5);
        assert!(got[1].is_nan());
        assert_eq!(got[2], 12345.0);
        assert_eq!(got[3], 3.0);
        assert_eq!(got[4], 67890.0);
        assert_eq!(got[5], 7.5); // retransmit_rate
        assert_eq!(got[6], 250.0); // dump_vt_us
    }

    #[test]
    fn metrics_decode_rejects_bad_version_and_short_payload() {
        assert!(decode_metrics(&[]).is_none());
        assert!(decode_metrics(&[1]).is_none()); // old version (v1) rejected
        assert!(decode_metrics(&[METRICS_VERSION, 0, 0]).is_none()); // truncated body
    }

    #[test]
    fn roundtrip_with_trailing_body() {
        let caps = vec![
            Cap {
                id: CAP_PROTOCOL_VERSION,
                payload: vec![1],
            },
            Cap {
                id: CAP_EXIT_STATUS,
                payload: vec![],
            },
        ];
        let mut bytes = encode_table(&caps);
        bytes.extend_from_slice(b"BODY");
        let (got, used) = decode_table(&bytes).unwrap();
        assert_eq!(got, caps);
        assert_eq!(&bytes[used..], b"BODY");
    }

    #[test]
    fn unknown_ids_are_preserved_and_skippable() {
        // A future peer's entry must parse by length and not disturb
        // anything after it.
        let caps = vec![
            Cap {
                id: 199,
                payload: vec![9, 9, 9],
            },
            Cap {
                id: CAP_EXIT_STATUS,
                payload: vec![7],
            },
        ];
        let (got, _) = decode_table(&encode_table(&caps)).unwrap();
        assert_eq!(find(&got, CAP_EXIT_STATUS).unwrap().payload, vec![7]);
        assert_eq!(find(&got, 199).unwrap().payload.len(), 3);
        assert!(find(&got, 42).is_none());
    }

    #[test]
    fn malformed_tables_reject() {
        assert!(decode_table(&[]).is_err()); // no count
        assert!(decode_table(&[1]).is_err()); // count without entry
        assert!(decode_table(&[1, 5, 4, 0, 0]).is_err()); // len 4, 2 bytes
        assert!(decode_table(&[2, 0, 0]).is_err()); // second entry missing
    }

    #[test]
    fn empty_table_is_one_byte() {
        let (caps, used) = decode_table(&encode_table(&[])).unwrap();
        assert!(caps.is_empty());
        assert_eq!(used, 1);
    }

    #[test]
    fn kitty_keyboard_payload_decodes_and_masks() {
        // A valid 1-byte payload decodes to its low 5 bits.
        assert_eq!(decode_kitty_keyboard(&[0]), Some(0));
        assert_eq!(decode_kitty_keyboard(&[0b1_0001]), Some(0b1_0001));
        // Out-of-range high bits are masked off (RFC 0010 Security).
        assert_eq!(decode_kitty_keyboard(&[0xff]), Some(0x1f));
        // Absent / malformed (wrong length) ⇒ None (treated as unadvertised).
        assert_eq!(decode_kitty_keyboard(&[]), None);
        assert_eq!(decode_kitty_keyboard(&[1, 2]), None);
    }

    #[test]
    fn own_table_leads_with_protocol_version() {
        let t = own_table(&[Cap {
            id: CAP_EXIT_STATUS,
            payload: vec![],
        }]);
        assert_eq!(t[0].id, CAP_PROTOCOL_VERSION);
        assert_eq!(t[0].payload, vec![PROTOCOL_VERSION]);
        assert_eq!(t[1].id, CAP_EXIT_STATUS);
    }

    #[test]
    fn agent_data_single_chunk_roundtrips() {
        let caps = encode_agent_data(42, b"sign-request");
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].id, CAP_AGENT_DATA);
        let (offset, bytes) = decode_agent_data(&caps[0].payload).unwrap();
        assert_eq!(offset, 42);
        assert_eq!(bytes, b"sign-request");
    }

    #[test]
    fn agent_data_splits_into_contiguous_chunks() {
        // A run past one entry's budget splits into ≤AGENT_DATA_MAX chunks
        // whose offsets chain contiguously from the base.
        let base = 1000u64;
        let data: Vec<u8> = (0..AGENT_DATA_MAX * 2 + 5).map(|i| i as u8).collect();
        let caps = encode_agent_data(base, &data);
        assert_eq!(caps.len(), 3); // 247 + 247 + 5

        let mut expected_offset = base;
        let mut reassembled = Vec::new();
        for cap in &caps {
            assert_eq!(cap.id, CAP_AGENT_DATA);
            assert!(cap.payload.len() - 8 <= AGENT_DATA_MAX);
            let (offset, bytes) = decode_agent_data(&cap.payload).unwrap();
            assert_eq!(offset, expected_offset, "offsets must be contiguous");
            expected_offset += bytes.len() as u64;
            reassembled.extend_from_slice(bytes);
        }
        assert_eq!(reassembled, data);
        assert_eq!(expected_offset, base + data.len() as u64);
    }

    #[test]
    fn agent_data_empty_run_emits_nothing() {
        assert!(encode_agent_data(7, b"").is_empty());
    }

    #[test]
    fn agent_data_caps_bounded_so_table_count_cannot_overflow() {
        // A pending tail far larger than one message can carry must NOT produce
        // more than MAX_AGENT_DATA_CAPS entries — otherwise the table's count:u8
        // overflows and silently corrupts the frame. The emitted prefix stays
        // contiguous from the base; the remainder rides a later message.
        let huge = vec![0u8; AGENT_DATA_MAX * (MAX_AGENT_DATA_CAPS + 50)];
        let caps = encode_agent_data(0, &huge);
        assert_eq!(caps.len(), MAX_AGENT_DATA_CAPS);
        // Whole table (agent data + a generous slack for other caps) fits a u8.
        assert!(caps.len() + 16 <= u8::MAX as usize);
        // The emitted entries are still a contiguous prefix from offset 0.
        let mut expected = 0u64;
        for cap in &caps {
            let (offset, bytes) = decode_agent_data(&cap.payload).unwrap();
            assert_eq!(offset, expected);
            expected += bytes.len() as u64;
        }
    }

    #[test]
    fn agent_data_chunk_holds_exactly_max() {
        let data = vec![0xcd; AGENT_DATA_MAX];
        let caps = encode_agent_data(0, &data);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].payload.len(), u8::MAX as usize); // 8 + 247, the cap budget
        let (_, bytes) = decode_agent_data(&caps[0].payload).unwrap();
        assert_eq!(bytes, &data[..]);
    }

    #[test]
    fn agent_ack_roundtrips() {
        let cap = encode_agent_ack(0xdead_beef_cafe);
        assert_eq!(cap.id, CAP_AGENT_ACK);
        assert_eq!(decode_agent_ack(&cap.payload).unwrap(), 0xdead_beef_cafe);
    }

    #[test]
    fn agent_payloads_reject_malformed() {
        assert!(decode_agent_data(&[0, 0, 0]).is_err()); // shorter than offset prefix
        assert!(decode_agent_ack(&[1, 2, 3]).is_err()); // not a u64
        assert!(decode_agent_ack(&[0; 9]).is_err()); // too long for a u64
    }

    #[test]
    fn find_all_returns_every_agent_data_in_order() {
        let mut table = vec![encode_agent_ack(5)];
        table.extend(encode_agent_data(0, &vec![1u8; AGENT_DATA_MAX + 10]));
        let datas: Vec<_> = find_all(&table, CAP_AGENT_DATA).collect();
        assert_eq!(datas.len(), 2);
        let (first_off, _) = decode_agent_data(&datas[0].payload).unwrap();
        let (second_off, _) = decode_agent_data(&datas[1].payload).unwrap();
        assert_eq!(first_off, 0);
        assert_eq!(second_off, AGENT_DATA_MAX as u64);
    }

    #[test]
    fn server_diag_roundtrips() {
        let d = ServerDiag {
            current_num: 0xdead_beef,
            acked_num: 0xdead_beed,
            term_gen: 9_001,
            outstanding: 3,
            pty_open: true,
            pid: 1_069_334,
            agent: None,
        };
        let cap = encode_server_diag(&d);
        assert_eq!(cap.id, CAP_DIAG);
        assert_eq!(cap.payload.len(), 33);
        assert_eq!(decode_server_diag(&cap.payload).unwrap(), d);
        // A pre-pid (29-byte) peer still decodes, with pid = 0.
        let old = decode_server_diag(&cap.payload[..29]).unwrap();
        assert_eq!(old.pid, 0);
        assert_eq!(old.current_num, d.current_num);
    }

    #[test]
    fn server_diag_v2_with_agent_roundtrips() {
        let d = ServerDiag {
            current_num: 7,
            acked_num: 6,
            term_gen: 50,
            outstanding: 1,
            pty_open: true,
            pid: 4242,
            agent: Some(AgentDiag {
                live_channels: 4,
                next_channel_id: 9,
                symlink_ok: true,
                bytes_sent: 4096,
                bytes_queued: 3000,
            }),
        };
        let cap = encode_server_diag(&d);
        assert_eq!(cap.payload.len(), 58);
        assert_eq!(decode_server_diag(&cap.payload).unwrap(), d);
        // A pre-counter (42-byte) peer still decodes; the counters read zero,
        // which is "nothing sent, nothing re-sent" and cannot be mistaken for a
        // measurement of zero overhead on a busy stream (posh#142).
        let old = decode_server_diag(&cap.payload[..42]).unwrap();
        let a = old.agent.unwrap();
        assert_eq!(a.live_channels, 4);
        assert_eq!((a.bytes_sent, a.bytes_queued), (0, 0));
        // The same diag without agent state encodes as a 33-byte payload (core +
        // pid) and decodes with `agent = None`.
        let no_agent = ServerDiag { agent: None, ..d };
        let cap_na = encode_server_diag(&no_agent);
        assert_eq!(cap_na.payload.len(), 33);
        let got_na = decode_server_diag(&cap_na.payload).unwrap();
        assert!(got_na.agent.is_none());
        assert_eq!(got_na.pid, 4242);
    }

    #[test]
    fn server_diag_pty_closed_roundtrips() {
        let d = ServerDiag {
            current_num: 1,
            acked_num: 1,
            term_gen: 0,
            outstanding: 0,
            pty_open: false,
            pid: 7,
            agent: None,
        };
        let got = decode_server_diag(&encode_server_diag(&d).payload).unwrap();
        assert!(!got.pty_open);
        assert_eq!(got, d);
    }

    #[test]
    fn server_diag_rejects_wrong_length() {
        // Valid lengths are 29/33/38/42/58 (core, +pid, +agent, +pid+agent,
        // +stream counters).
        for bad in [0usize, 28, 30, 34, 37, 39, 41, 43, 50, 57, 59] {
            assert!(
                decode_server_diag(&vec![0u8; bad]).is_err(),
                "len {bad} should be rejected"
            );
        }
        for ok in [29usize, 33, 38, 42, 58] {
            assert!(
                decode_server_diag(&vec![0u8; ok]).is_ok(),
                "len {ok} should decode"
            );
        }
    }

    #[test]
    fn server_diag_survives_table_roundtrip_with_trailing_body() {
        // It rides the ordinary caps table beside other entries, ahead of the
        // frame body, and parses back intact (experimental id, preserved).
        let d = ServerDiag {
            current_num: 42,
            acked_num: 40,
            term_gen: 100,
            outstanding: 2,
            pty_open: true,
            pid: 99,
            agent: None,
        };
        let table = own_table(&[encode_server_diag(&d)]);
        let mut bytes = encode_table(&table);
        bytes.extend_from_slice(b"BODY");
        let (got, used) = decode_table(&bytes).unwrap();
        assert_eq!(&bytes[used..], b"BODY");
        let cap = find(&got, CAP_DIAG).unwrap();
        assert_eq!(decode_server_diag(&cap.payload).unwrap(), d);
    }

    #[test]
    fn multiple_agent_data_caps_survive_table_roundtrip() {
        // The whole point of the chunked-TLV approach: agent data rides the
        // ordinary caps table with no body-format change. Two AGENT_DATA
        // entries plus an ack must encode and decode intact alongside a
        // trailing body.
        let mut table = own_table(&[Cap {
            id: CAP_AGENT_FORWARD,
            payload: vec![],
        }]);
        table.extend(encode_agent_data(100, &vec![7u8; AGENT_DATA_MAX + 1]));
        table.push(encode_agent_ack(100));

        let mut bytes = encode_table(&table);
        bytes.extend_from_slice(b"BODY");
        let (got, used) = decode_table(&bytes).unwrap();
        assert_eq!(&bytes[used..], b"BODY");

        assert!(find(&got, CAP_AGENT_FORWARD).is_some());
        assert_eq!(find_all(&got, CAP_AGENT_DATA).count(), 2);
        assert_eq!(decode_agent_ack(&find(&got, CAP_AGENT_ACK).unwrap().payload).unwrap(), 100);
    }
}
