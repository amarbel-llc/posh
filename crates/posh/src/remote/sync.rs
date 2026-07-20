//! State-synchronization building blocks: datagram fragmentation (port of
//! mosh transportfragment.cc), frame/message encodings, a prefix/suffix
//! binary diff, and the reliable cumulative user-input stream (a simplified
//! mosh UserStream).

use crate::util::{Error, Result};
use posh_proto::caps;
use posh_proto::frame::{decode_flags_and_caps, flags_with_extension};

// The frame wire types (ServerFrame/FrameBody, the prefix/suffix diff, the base
// checksum, the per-frame flag bits, and the keepalive cadence) live in
// posh-proto (github #75) so poshterity can drive the codecs without a
// posh->poshterity->posh dependency cycle. Re-exported here so existing
// `crate::remote::sync::{ServerFrame, FrameBody, make_diff, ...}` paths keep
// resolving and this module's tests exercise the same surface production uses.
// The datagram fragmentation, ClientMessage, and the reliable input/echo/agent
// streams below stay in posh and re-import these.
pub use posh_proto::frame::{
    base_checksum, FrameBody, ServerFrame, FLAG_ECHO, FLAG_OVERLAY, FLAG_SERVER_LOG, FLAG_SHUTDOWN,
    FLAG_WEDGE, HEARTBEAT_INTERVAL,
};
// make_diff/apply_diff are exercised only by tests now (the DumpDiff codec that
// used them moved to posh-proto); test-only re-export keeps `sync::make_diff`
// resolving for the diff/wedge tests in this module and in client.rs without a
// non-test unused-import warning in this binary crate.
#[cfg(test)]
pub use posh_proto::frame::{apply_diff, make_diff};

// ---------------------------------------------------------------------------
// Fragmentation. Header (big-endian, as in mosh): u64 instruction id, then
// u16 with the final bit in the top bit and the fragment number below.

pub const FRAG_HEADER_LEN: usize = 10;
/// Max assembled-payload bytes per fragment: ~1400-byte datagrams minus the
/// crypto overhead (24 bytes), packet timestamps (4) and fragment header.
pub const FRAGMENT_CONTENTS_MAX: usize = 1400 - 24 - 4 - FRAG_HEADER_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    pub id: u64,
    pub num: u16,
    pub is_final: bool,
    pub contents: Vec<u8>,
}

impl Fragment {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FRAG_HEADER_LEN + self.contents.len());
        out.extend_from_slice(&self.id.to_be_bytes());
        // Fragment numbers are capped at 15 bits; the top bit flags "final".
        let combined = ((self.is_final as u16) << 15) | (self.num & 0x7fff);
        out.extend_from_slice(&combined.to_be_bytes());
        out.extend_from_slice(&self.contents);
        out
    }

    pub fn from_bytes(data: &[u8]) -> Result<Fragment> {
        if data.len() < FRAG_HEADER_LEN {
            return Err(Error::from("fragment too short"));
        }
        let id = u64::from_be_bytes(data[..8].try_into().unwrap());
        let combined = u16::from_be_bytes([data[8], data[9]]);
        Ok(Fragment {
            id,
            num: combined & 0x7fff,
            is_final: combined & 0x8000 != 0,
            contents: data[FRAG_HEADER_LEN..].to_vec(),
        })
    }
}

pub struct Fragmenter {
    next_id: u64,
}

impl Default for Fragmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Fragmenter {
    pub fn new() -> Fragmenter {
        Fragmenter { next_id: 0 }
    }

    pub fn make_fragments(&mut self, payload: &[u8], max_contents: usize) -> Vec<Fragment> {
        self.next_id += 1;
        let id = self.next_id;
        let mut out = Vec::new();
        let mut num: u16 = 0;
        let mut rest = payload;
        loop {
            let take = rest.len().min(max_contents);
            let (chunk, tail) = rest.split_at(take);
            let is_final = tail.is_empty();
            out.push(Fragment {
                id,
                num,
                is_final,
                contents: chunk.to_vec(),
            });
            num += 1;
            rest = tail;
            if is_final {
                break;
            }
        }
        out
    }
}

/// Upper bound on fragments per instruction (~11 MB of payload at MTU-sized
/// chunks): bounds allocation driven by a buggy or hostile authenticated
/// peer, which could otherwise force a 32768-slot buffer per id.
const MAX_FRAGMENTS: usize = 8192;

#[derive(Default)]
pub struct FragmentAssembly {
    current_id: Option<u64>,
    fragments: Vec<Option<Vec<u8>>>,
    arrived: usize,
    total: Option<usize>,
}

impl FragmentAssembly {
    pub fn new() -> FragmentAssembly {
        FragmentAssembly::default()
    }

    /// Adds one fragment; returns the reassembled payload once complete.
    /// A fragment from a newer (or just different) id discards the partial
    /// assembly in progress: only one instruction is in flight at a time.
    pub fn add(&mut self, frag: Fragment) -> Option<Vec<u8>> {
        if self.current_id != Some(frag.id) {
            self.current_id = Some(frag.id);
            self.fragments.clear();
            self.arrived = 0;
            self.total = None;
        }
        let idx = frag.num as usize;
        // Drop fragments that cannot belong to a well-formed instruction:
        // past the allocation cap, past a known final index, or a final
        // contradicting fragments already received beyond it. Bogus
        // fragments must not grow the buffer or poison the completion gate.
        if idx >= MAX_FRAGMENTS
            || self.total.is_some_and(|t| idx >= t)
            || (frag.is_final
                && (self.fragments.len() > idx + 1 || self.total.is_some_and(|t| t != idx + 1)))
        {
            return None;
        }
        if self.fragments.len() <= idx {
            self.fragments.resize(idx + 1, None);
        }
        if frag.is_final {
            self.total = Some(idx + 1);
        }
        if self.fragments[idx].is_none() {
            self.fragments[idx] = Some(frag.contents);
            self.arrived += 1;
        }
        if self.total == Some(self.arrived) && self.fragments.len() == self.arrived {
            let mut out = Vec::new();
            for piece in self.fragments.drain(..) {
                out.extend_from_slice(&piece.unwrap());
            }
            self.current_id = None;
            self.arrived = 0;
            self.total = None;
            Some(out)
        } else {
            None
        }
    }
}

/// Server-side echo-ack tracker (port of mosh's `Complete` echo ack with
/// `ECHO_TIMEOUT`): input written to the application is considered echoed
/// into the screen state once a grace period has elapsed, so the client can
/// validate predictions against a frame that should contain the echo.
pub const ECHO_TIMEOUT: u64 = 50; // ms, as in mosh

#[derive(Default)]
pub struct EchoAck {
    pending: std::collections::VecDeque<(u64, u64)>, // (offset, written_at)
    acked: u64,
}

impl EchoAck {
    pub fn new() -> EchoAck {
        EchoAck::default()
    }

    /// Records that input through `offset` was written to the application.
    pub fn record(&mut self, offset: u64, now: u64) {
        let newest = self.pending.back().map_or(self.acked, |&(o, _)| o);
        if offset > newest {
            self.pending.push_back((offset, now));
        }
    }

    /// Advances the ack over entries older than `ECHO_TIMEOUT`. Returns
    /// true when the ack moved (the server should send a fresh frame).
    pub fn update(&mut self, now: u64) -> bool {
        let mut changed = false;
        while let Some(&(offset, at)) = self.pending.front() {
            if now.saturating_sub(at) < ECHO_TIMEOUT {
                break;
            }
            self.acked = offset;
            self.pending.pop_front();
            changed = true;
        }
        changed
    }

    pub fn ack(&self) -> u64 {
        self.acked
    }

    /// Time until the next pending entry matures, for poll deadlines.
    pub fn wait_time(&self, now: u64) -> Option<u64> {
        self.pending
            .front()
            .map(|&(_, at)| (at + ECHO_TIMEOUT).saturating_sub(now))
    }
}

// ---------------------------------------------------------------------------
// Client->server messages: frame ack, current terminal size, and the unacked
// tail of the cumulative input byte stream.

/// Client requests a clean shutdown (Ctrl-^ . quit sequence): the server
/// hangs up the shell and acknowledges with `FLAG_SHUTDOWN` frames.
pub const CLIENT_FLAG_SHUTDOWN: u8 = 1;

/// Client requests an escape-to-shell overlay (Ctrl-^ s, FDR 0008): the server
/// spawns the configured escape command in the session cwd and broadcasts it
/// until it exits. Sticky until the client sees `FLAG_OVERLAY` (so it survives
/// UDP loss); the server's "already in overlay" guard makes repeats idempotent.
/// `0x04` is the next free runtime bit (0x01 = SHUTDOWN, 0x02 = caps EXTENSION).
pub const CLIENT_FLAG_ESCAPE: u8 = 4;

/// Toggle the *server's* debug logging at runtime (#3): the palette's "Server
/// debug logging" command sets one of these for a single message. Idempotent on
/// the server (setting ON when already on is a no-op), so they need not be
/// sticky — a lost request just means the user re-toggles. The server reports
/// the resulting state back via `FLAG_SERVER_LOG`. `0x08`/`0x10` are the next
/// free client runtime bits.
pub const CLIENT_FLAG_LOG_ON: u8 = 8;
pub const CLIENT_FLAG_LOG_OFF: u8 = 16;

/// Client asks the server to send a fresh `Full` keyframe (the palette "Reset &
/// resync" command): the client's apply state is wedged on a base it cannot
/// apply and the automatic stale-ack -> `Full` recovery did not fire. One-shot
/// like `CLIENT_FLAG_ESCAPE` — cleared after one send (a lost request just means
/// the user retries). On receipt the server drops its acked baseline so the next
/// frame must be a `Full`, which the client applies unconditionally. `0x20` is
/// the next free client runtime bit after `CLIENT_FLAG_LOG_OFF`.
pub const CLIENT_FLAG_RESYNC: u8 = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientMessage {
    /// Runtime signal bits only (CLIENT_FLAG_SHUTDOWN); the EXTENSION bit
    /// is handled by encode/decode like on [`ServerFrame`].
    pub flags: u8,
    /// RFC 0001 §3 capability table; empty == baseline (v0) format.
    pub caps: Vec<caps::Cap>,
    pub acked_frame: u64,
    pub rows: u16,
    pub cols: u16,
    pub input_base: u64,
    pub input: Vec<u8>,
}

impl ClientMessage {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(21 + self.input.len());
        out.push(flags_with_extension(self.flags, &self.caps));
        if !self.caps.is_empty() {
            out.extend_from_slice(&caps::encode_table(&self.caps));
        }
        out.extend_from_slice(&self.acked_frame.to_le_bytes());
        out.extend_from_slice(&self.rows.to_le_bytes());
        out.extend_from_slice(&self.cols.to_le_bytes());
        out.extend_from_slice(&self.input_base.to_le_bytes());
        out.extend_from_slice(&self.input);
        out
    }

    pub fn decode(data: &[u8]) -> Result<ClientMessage> {
        let (flags, caps, at) = decode_flags_and_caps(data)?;
        if data.len() < at + 20 {
            return Err(Error::from("client message too short"));
        }
        Ok(ClientMessage {
            flags,
            caps,
            acked_frame: u64::from_le_bytes(data[at..at + 8].try_into().unwrap()),
            rows: u16::from_le_bytes([data[at + 8], data[at + 9]]),
            cols: u16::from_le_bytes([data[at + 10], data[at + 11]]),
            input_base: u64::from_le_bytes(data[at + 12..at + 20].try_into().unwrap()),
            input: data[at + 20..].to_vec(),
        })
    }
}

/// Client side of the reliable input stream: keystrokes accumulate at
/// monotonically increasing byte offsets; the unacked tail is retransmitted
/// until the server acknowledges its end offset.
#[derive(Default)]
pub struct InputOutbox {
    base: u64,
    buf: Vec<u8>,
}

impl InputOutbox {
    pub fn new() -> InputOutbox {
        InputOutbox::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Drops everything below the server-acknowledged offset.
    pub fn ack(&mut self, upto: u64) {
        if upto > self.base {
            let n = ((upto - self.base) as usize).min(self.buf.len());
            self.buf.drain(..n);
            self.base += n as u64;
        }
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    /// Offset one past the newest byte pushed (the server's next expected
    /// offset once everything pending is delivered).
    pub fn end_offset(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    pub fn pending(&self) -> &[u8] {
        &self.buf
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// Client side of the scrollback accumulation model (RFC 0002 §3): a local,
/// **partial, monotonically growing** view of the server's primary-screen
/// row space. Rows arrive in `BODY_SCROLLBACK` bodies and are appended to
/// the bottom in order; once the ring reaches `capacity` it evicts its own
/// oldest rows (those are gone — this revision has no back-fill). The view
/// is explicitly partial: on a fresh attach it starts empty and grows
/// forward, and "scrolled past the top of what I hold" is the end of
/// locally-available scrollback, not an error. A `Full` visible reset MUST
/// NOT clear it (the ring is the durable local accumulation); a width
/// resize MUST (RFC 0002 §4 — the caller re-accumulates at the new width).
#[derive(Debug)]
pub struct ScrollbackRing {
    rows: std::collections::VecDeque<Vec<u8>>,
    capacity: usize,
}

impl ScrollbackRing {
    pub fn new(capacity: usize) -> ScrollbackRing {
        ScrollbackRing {
            rows: std::collections::VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Appends rows to the bottom of the ring, evicting the oldest past the
    /// capacity bound. Each row is the self-contained `dump_scrollback_row`
    /// byte stream the server shipped (RFC 0002 §3: the client appends the
    /// bytes the body carried; it does not derive them from the visible
    /// body).
    pub fn append(&mut self, rows: &[Vec<u8>]) {
        for row in rows {
            if self.rows.len() >= self.capacity {
                self.rows.pop_front();
            }
            self.rows.push_back(row.clone());
        }
    }

    pub fn clear(&mut self) {
        self.rows.clear();
    }

    // The read side of the ring (`len`/`is_empty`/`row`) is the accumulated
    // history the client's wheel scroll-view renders from. That renderer is
    // FDR 0005's local viewport, deliberately out of this wire-contract
    // change, so these are exercised by the conformance tests but not yet by
    // a non-test caller.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The `i`th retained row (0 = oldest still held), or `None` past the end.
    #[allow(dead_code)]
    pub fn row(&self, i: usize) -> Option<&[u8]> {
        self.rows.get(i).map(Vec::as_slice)
    }
}

/// Server side: tracks the next expected input offset and extracts only the
/// not-yet-applied suffix from (possibly retransmitted) client messages.
#[derive(Default)]
pub struct InputInbox {
    next: u64,
}

impl InputInbox {
    pub fn new() -> InputInbox {
        InputInbox::default()
    }

    pub fn next_offset(&self) -> u64 {
        self.next
    }

    pub fn accept<'a>(&mut self, base: u64, data: &'a [u8]) -> Option<&'a [u8]> {
        if base > self.next {
            // Gap: cannot happen while the client retransmits from the acked
            // base, but guard against it rather than corrupt the stream.
            return None;
        }
        let skip = (self.next - base) as usize;
        if skip >= data.len() {
            return None;
        }
        self.next = base + data.len() as u64;
        Some(&data[skip..])
    }
}

// ---------------------------------------------------------------------------
// Agent-channel record codec (FDR 0004 phase 1). The reliable agent byte
// stream — carried in both directions by a mirror of the input-stream
// machinery (InputOutbox/InputInbox above) — frames its content as channel
// records:
//
//     channel: u32   kind: u8   len: u32   payload: len bytes   (all BE)
//
// Channels map 1:1 to unix connections accepted on the remote agent socket;
// records are protocol-agnostic byte pipes (no agent-message parsing here).
// Per ADR-0003 a record header or payload may straddle a stream-accept
// boundary, so decoding is a byte-fed state machine that buffers a partial
// record across `push` calls — never "one accept == one record".
//
// This is FDR 0004 work item 1: the pure codec + stream mirror, landed and
// tested ahead of its non-test callers (the caps wiring is item 2, the remote
// endpoint item 3, the client proxy item 4). Until those land the public
// surface here has only test callers, hence the `#[allow(dead_code)]` on each
// item — the same pattern `ScrollbackRing` uses for its conformance-tested but
// not-yet-wired read side.

/// Fixed record header: channel:u32 + kind:u8 + len:u32, big-endian to match
/// the fragment header's framing precedent.
#[allow(dead_code)]
pub const AGENT_RECORD_HEADER_LEN: usize = 9;

/// Upper bound on a single record's payload, matching the per-channel buffer
/// cap (FDR 0004: OpenSSH's max agent message, 256 KB). A header advertising
/// more than this from an authenticated-but-buggy/hostile peer is rejected
/// rather than allowed to drive a multi-megabyte allocation.
#[allow(dead_code)]
pub const AGENT_RECORD_PAYLOAD_MAX: usize = 256 * 1024;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    /// Remote: a new unix client connected to the agent socket. Opens `channel`.
    Open,
    /// One opaque chunk of `channel`'s byte pipe, either direction.
    Data,
    /// Either side closing `channel`; half-close collapses to full close (the
    /// agent protocol is strict request/response).
    Close,
    /// Client: the local agent is unreachable. The remote end answers the unix
    /// client with `SSH_AGENT_FAILURE` and closes `channel`.
    Fail,
}

#[allow(dead_code)]
impl RecordKind {
    fn to_byte(self) -> u8 {
        match self {
            RecordKind::Open => 0,
            RecordKind::Data => 1,
            RecordKind::Close => 2,
            RecordKind::Fail => 3,
        }
    }

    fn from_byte(b: u8) -> Result<RecordKind> {
        match b {
            0 => Ok(RecordKind::Open),
            1 => Ok(RecordKind::Data),
            2 => Ok(RecordKind::Close),
            3 => Ok(RecordKind::Fail),
            other => Err(Error::from(format!("agent record: unknown kind {other}"))),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRecord {
    pub channel: u32,
    pub kind: RecordKind,
    pub payload: Vec<u8>,
}

#[allow(dead_code)]
impl AgentRecord {
    /// Appends this record's framed bytes to `out` (the agent outbox buffer).
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        debug_assert!(self.payload.len() <= AGENT_RECORD_PAYLOAD_MAX);
        out.extend_from_slice(&self.channel.to_be_bytes());
        out.push(self.kind.to_byte());
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.payload);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(AGENT_RECORD_HEADER_LEN + self.payload.len());
        self.encode_into(&mut out);
        out
    }
}

/// Streaming reassembler for the agent record stream (ADR-0003). Bytes drained
/// from the agent inbox are fed in via `push`; whole records come back out as
/// they complete. A partial header or payload is held until the rest arrives,
/// so callers never have to align stream-accept boundaries to record edges.
///
/// On a malformed header (unknown kind, or a `len` past
/// [`AGENT_RECORD_PAYLOAD_MAX`]) `push` returns `Err` and the decoder is left
/// poisoned: the cumulative byte stream is authenticated and in-order, so a
/// bad header means the stream is corrupt, not recoverable by resync. The
/// caller drops the connection.
#[allow(dead_code)]
#[derive(Default)]
pub struct RecordDecoder {
    buf: Vec<u8>,
}

#[allow(dead_code)]
impl RecordDecoder {
    pub fn new() -> RecordDecoder {
        RecordDecoder::default()
    }

    /// Feeds freshly-applied stream bytes and returns every record that became
    /// complete. Records may span multiple `push` calls; multiple records may
    /// complete in one call.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<AgentRecord>> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < AGENT_RECORD_HEADER_LEN {
                break;
            }
            let channel = u32::from_be_bytes(self.buf[0..4].try_into().unwrap());
            let kind = RecordKind::from_byte(self.buf[4])?;
            let len = u32::from_be_bytes(self.buf[5..9].try_into().unwrap()) as usize;
            if len > AGENT_RECORD_PAYLOAD_MAX {
                return Err(Error::from(format!(
                    "agent record: payload len {len} exceeds cap {AGENT_RECORD_PAYLOAD_MAX}"
                )));
            }
            let total = AGENT_RECORD_HEADER_LEN + len;
            if self.buf.len() < total {
                break; // header parsed, payload not yet fully arrived
            }
            let payload = self.buf[AGENT_RECORD_HEADER_LEN..total].to_vec();
            self.buf.drain(..total);
            out.push(AgentRecord {
                channel,
                kind,
                payload,
            });
        }
        Ok(out)
    }
}

/// One end's view of the bidirectional agent byte stream (FDR 0004 phase 1).
/// Both client and server hold one of these per connection: records are
/// encoded into the outbox (the unacked tail rides every `AGENT_DATA` send
/// and drops on `AGENT_ACK`, exactly like keystrokes), and the peer's stream
/// is fed through the inbox into the decoder so only fresh, in-order bytes
/// reach the record reassembler. Reliability and roaming are inherited whole
/// from the input-stream machinery (constraint C3) — this type adds no new
/// retransmission logic, only the record framing on top.
#[allow(dead_code)]
#[derive(Default)]
pub struct AgentStream {
    outbox: InputOutbox,
    inbox: InputInbox,
    decoder: RecordDecoder,
    /// Cumulative bytes handed to the wire, INCLUDING re-sends. Fed by
    /// [`mark_sent`](Self::mark_sent) at each encode site, because only the
    /// caller knows what it actually emitted — `pending()` is a `&self` view and
    /// may be read without sending. See [`resent_bytes`](Self::resent_bytes).
    sent_bytes: u64,
}

#[allow(dead_code)]
impl AgentStream {
    pub fn new() -> AgentStream {
        AgentStream::default()
    }

    /// Frames a record onto the send side. It becomes part of the unacked
    /// tail until the peer's `AGENT_ACK` reaches past it.
    pub fn send(&mut self, record: &AgentRecord) {
        self.outbox.push(&record.to_bytes());
    }

    /// The outbound stream's acked base — emit as the `AGENT_DATA` offset.
    pub fn send_base(&self) -> u64 {
        self.outbox.base()
    }

    /// Bytes not yet known-acked by the peer — the `AGENT_DATA` payload to
    /// (re)send. Empty when the peer is caught up.
    pub fn pending(&self) -> &[u8] {
        self.outbox.pending()
    }

    pub fn has_pending(&self) -> bool {
        !self.outbox.is_empty()
    }

    /// Records that `len` bytes of [`pending`](Self::pending) were actually
    /// emitted. Call once per `AGENT_DATA` encode; re-sends of the same tail
    /// count each time, which is the whole point.
    pub fn mark_sent(&mut self, len: usize) {
        self.sent_bytes = self.sent_bytes.saturating_add(len as u64);
    }

    /// Cumulative bytes emitted, including re-sends (posh#142 telemetry).
    pub fn sent_bytes(&self) -> u64 {
        self.sent_bytes
    }

    /// Cumulative DISTINCT bytes ever queued — the outbox's monotonic end
    /// offset, unaffected by acking (which only drops the acked prefix).
    pub fn queued_bytes(&self) -> u64 {
        self.outbox.end_offset()
    }

    /// What cumulative-only acknowledgement has cost so far: bytes put on the
    /// wire that the peer had already been sent. The unacked tail rides EVERY
    /// message until acked, so on a lossy path this grows as the square of the
    /// stall — it is the quantity a selective ack would eliminate, and the
    /// evidence posh#142 should be decided on rather than first principles.
    pub fn resent_bytes(&self) -> u64 {
        self.sent_bytes.saturating_sub(self.queued_bytes())
    }

    /// Offset to advertise in `AGENT_ACK`: one past the last byte handed to
    /// the decoder, i.e. everything received in order so far.
    pub fn recv_ack(&self) -> u64 {
        self.inbox.next_offset()
    }

    /// Drops the acked prefix of the send side on the peer's `AGENT_ACK`.
    pub fn ack(&mut self, upto: u64) {
        self.outbox.ack(upto);
    }

    /// Accepts a peer `AGENT_DATA` chunk `(base, data)`: dedupes against what
    /// the inbox has already seen, then feeds the fresh suffix to the record
    /// decoder. Returns the records that completed. A malformed record stream
    /// surfaces the decoder's `Err` (the caller drops the connection).
    pub fn recv(&mut self, base: u64, data: &[u8]) -> Result<Vec<AgentRecord>> {
        match self.inbox.accept(base, data) {
            Some(fresh) => self.decoder.push(fresh),
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_roundtrip_single() {
        let mut fr = Fragmenter::new();
        let mut asm = FragmentAssembly::new();
        let frags = fr.make_fragments(b"small payload", 100);
        assert_eq!(frags.len(), 1);
        assert!(frags[0].is_final);
        let wire = frags[0].to_bytes();
        let parsed = Fragment::from_bytes(&wire).unwrap();
        assert_eq!(asm.add(parsed), Some(b"small payload".to_vec()));
    }

    #[test]
    fn fragment_roundtrip_multi_and_out_of_order() {
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let mut fr = Fragmenter::new();
        let mut asm = FragmentAssembly::new();
        let mut frags = fr.make_fragments(&payload, 1370);
        assert!(frags.len() > 2);
        frags.reverse(); // worst-case reordering
        let mut result = None;
        for f in frags {
            let f = Fragment::from_bytes(&f.to_bytes()).unwrap();
            if let Some(p) = asm.add(f) {
                result = Some(p);
            }
        }
        assert_eq!(result, Some(payload));
    }

    #[test]
    fn fragment_duplicate_tolerated_and_new_id_resets() {
        let mut fr = Fragmenter::new();
        let mut asm = FragmentAssembly::new();
        let frags_a = fr.make_fragments(&[1u8; 300], 100);
        // Deliver only part of A, then all of B; B must still assemble.
        assert_eq!(asm.add(frags_a[0].clone()), None);
        assert_eq!(asm.add(frags_a[0].clone()), None); // duplicate
        let frags_b = fr.make_fragments(&[2u8; 150], 100);
        assert_eq!(asm.add(frags_b[0].clone()), None);
        assert_eq!(asm.add(frags_b[1].clone()), Some(vec![2u8; 150]));
    }

    #[test]
    fn fragment_past_final_rejected_without_wedging_assembly() {
        let frag = |num: u16, is_final: bool, contents: &[u8]| Fragment {
            id: 1,
            num,
            is_final,
            contents: contents.to_vec(),
        };
        let mut asm = FragmentAssembly::new();
        assert_eq!(asm.add(frag(2, true, b"c")), None);
        // A stray fragment past the final index must be dropped, not grow
        // the buffer and make the completion gate unsatisfiable.
        assert_eq!(asm.add(frag(5, false, b"x")), None);
        // ...as must a second final contradicting the known total.
        assert_eq!(asm.add(frag(4, true, b"y")), None);
        assert_eq!(asm.add(frag(1, false, b"b")), None);
        assert_eq!(
            asm.add(frag(0, false, b"a")).as_deref(),
            Some(b"abc".as_slice()),
            "assembly wedged after rejecting bogus fragments"
        );
    }

    // RFC 0011 §4, verification. The RFC's normative reassembly requirement
    // rests on a claim about TODAY's behaviour: that two instructions in flight
    // together destroy each other, because `FragmentAssembly` keeps one
    // `current_id` and clears the partial assembly whenever a fragment bearing a
    // different id arrives.
    //
    // `fragment_duplicate_tolerated_and_new_id_resets` above shows the reset in
    // the sequential case (all of A, then all of B) and treats it as intended —
    // which it is, while only one instruction is ever in flight. This test shows
    // the INTERLEAVED case that multiplexing introduces: neither instruction
    // completes, no matter how many times its fragments arrive. That is the
    // corruption §4 forbids an implementation from shipping into.
    #[test]
    fn interleaved_instructions_destroy_each_other_today() {
        let mut fr = Fragmenter::new();
        let mut asm = FragmentAssembly::new();
        let a = fr.make_fragments(&[0xaa; 300], 100); // 3 fragments, id 1
        let b = fr.make_fragments(&[0xbb; 300], 100); // 3 fragments, id 2
        assert_eq!(a.len(), 3);
        assert_eq!(b.len(), 3);

        // Perfectly interleaved delivery — no loss, no reordering within an
        // instruction. Every fragment of both instructions is delivered.
        let mut completed = Vec::new();
        for i in 0..3 {
            if let Some(p) = asm.add(a[i].clone()) {
                completed.push(p);
            }
            if let Some(p) = asm.add(b[i].clone()) {
                completed.push(p);
            }
        }

        assert!(
            completed.is_empty(),
            "with the current single-`current_id` buffer, interleaving must lose \
             BOTH instructions despite every fragment arriving; got {} completion(s). \
             If this now passes, RFC 0011 §4 has been implemented and this test \
             should be inverted.",
            completed.len()
        );
    }

    // RFC 0011 §1/§2, verification. The RFC asserts that `ClientMessage` and
    // `ServerFrame` become the `session` channel's payload "verbatim" — i.e.
    // prepending a 9-byte envelope and decoding from an offset needs no change
    // to either codec. Both decoders take `&[u8]`, so this should hold; assert
    // it rather than assume, since the whole compatibility argument rests on it.
    #[test]
    fn a_nine_byte_envelope_prefix_leaves_both_codecs_verbatim() {
        // RFC 0011 §2: ver:u8 + channel:u64 LE.
        let envelope = |channel: u64| {
            let mut e = vec![0x01u8];
            e.extend_from_slice(&channel.to_le_bytes());
            e
        };
        assert_eq!(envelope(0).len(), 9, "the §2 envelope is 9 bytes");

        let cm = ClientMessage {
            flags: 0,
            caps: caps::own_table(&[]),
            acked_frame: 42,
            rows: 24,
            cols: 80,
            input_base: 7,
            input: b"hello".to_vec(),
        };
        let mut wire = envelope(0x0102_0304_0506_0708);
        wire.extend_from_slice(&cm.encode());
        assert_eq!(
            ClientMessage::decode(&wire[9..]).unwrap(),
            cm,
            "ClientMessage must decode unchanged from behind the envelope"
        );

        let sf = posh_proto::frame::ServerFrame {
            flags: 0,
            caps: caps::own_table(&[]),
            frame_num: 9,
            input_ack: 5,
            echo_ack: 4,
            body: posh_proto::frame::FrameBody::Full(b"screen".to_vec()),
        };
        let mut wire = envelope(1);
        wire.extend_from_slice(&sf.encode());
        assert_eq!(
            posh_proto::frame::ServerFrame::decode(&wire[9..]).unwrap(),
            sf,
            "ServerFrame must decode unchanged from behind the envelope"
        );
    }

    // posh#142 telemetry. The agent outbox re-sends its whole unacked tail on
    // every message until the peer acks, so cumulative-only acknowledgement's
    // cost is exactly "bytes emitted minus distinct bytes queued". These counters
    // exist so that cost can be read off a real connection instead of argued
    // about from first principles.
    #[test]
    fn agent_stream_counts_resent_bytes_until_acked() {
        let mut s = AgentStream::new();
        s.send(&AgentRecord {
            channel: 1,
            kind: RecordKind::Data,
            payload: vec![0xab; 10],
        });
        let queued = s.queued_bytes();
        assert!(queued > 0, "the record framed some bytes");
        assert_eq!(s.sent_bytes(), 0, "nothing emitted yet");
        assert_eq!(s.resent_bytes(), 0);

        // First emission: all new, nothing re-sent.
        let n = s.pending().len();
        s.mark_sent(n);
        assert_eq!(s.sent_bytes(), queued);
        assert_eq!(s.resent_bytes(), 0, "a first send is not a re-send");

        // Peer did not ack, so the same tail rides the next two messages.
        s.mark_sent(s.pending().len());
        s.mark_sent(s.pending().len());
        assert_eq!(
            s.resent_bytes(),
            queued * 2,
            "each unacked repeat is counted as overhead"
        );

        // Once acked the tail drops, so further messages carry nothing and the
        // overhead stops growing.
        s.ack(queued);
        assert_eq!(s.pending().len(), 0);
        s.mark_sent(s.pending().len());
        assert_eq!(s.resent_bytes(), queued * 2, "acking halts the bleeding");
        assert_eq!(
            s.queued_bytes(),
            queued,
            "queued is cumulative-distinct and unaffected by acking"
        );
    }

    #[test]
    fn fragment_count_capped() {
        let mut asm = FragmentAssembly::new();
        assert_eq!(
            asm.add(Fragment {
                id: 1,
                num: 0x7fff,
                is_final: false,
                contents: vec![0u8; 8],
            }),
            None
        );
        assert!(
            asm.fragments.len() <= MAX_FRAGMENTS,
            "fragment buffer grew past the cap: {}",
            asm.fragments.len()
        );
    }

    #[test]
    fn empty_payload_still_produces_final_fragment() {
        let mut fr = Fragmenter::new();
        let mut asm = FragmentAssembly::new();
        let frags = fr.make_fragments(b"", 100);
        assert_eq!(frags.len(), 1);
        assert_eq!(asm.add(frags[0].clone()), Some(vec![]));
    }

    #[test]
    fn diff_apply_roundtrip() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"hello world", b"hello brave world"),
            (b"", b"something"),
            (b"something", b""),
            (b"identical", b"identical"),
            (b"abcdef", b"xyz"),
            (b"prefix-mid-suffix", b"prefix-MIDDLE-suffix"),
        ];
        for (old, new) in cases {
            let d = make_diff(old, new);
            assert_eq!(
                apply_diff(old, &d).as_deref(),
                Some(*new),
                "old={old:?} new={new:?}"
            );
        }
    }

    #[test]
    fn diff_is_compact_for_appends() {
        let old = vec![7u8; 5000];
        let mut new = old.clone();
        new.extend_from_slice(b"tail");
        let d = make_diff(&old, &new);
        assert!(d.len() <= 8 + 4 + 8); // header + middle, not the whole 5KB
        assert_eq!(apply_diff(&old, &d), Some(new));
    }

    #[test]
    fn apply_diff_rejects_bad_input() {
        assert_eq!(apply_diff(b"short", &[0, 0, 0]), None);
        let mut d = Vec::new();
        d.extend_from_slice(&100u32.to_le_bytes());
        d.extend_from_slice(&100u32.to_le_bytes());
        assert_eq!(apply_diff(b"tiny", &d), None);
    }

    #[test]
    fn base_checksum_is_deterministic_and_sensitive() {
        // RFC 0006: same bytes -> same tag; any change (including transposition)
        // -> different tag, enough to catch an accidental diff-base divergence.
        assert_eq!(base_checksum(b"hello"), base_checksum(b"hello"));
        assert_ne!(base_checksum(b"hello"), base_checksum(b"hellp"));
        assert_ne!(base_checksum(b""), base_checksum(b"x"));
        assert_ne!(base_checksum(b"ab"), base_checksum(b"ba"));
    }

    // A title change is a length-varying edit to the middle of a dump (the OSC
    // title sits between unchanged prefix/suffix screen state). On a matching
    // base it round-trips fine — the normal path.
    #[test]
    fn apply_diff_title_change_round_trips_on_matching_base() {
        let base = b"\x1b[2J\x1b[H\x1b]2;old title\x07cursor-state".to_vec();
        let new = b"\x1b[2J\x1b[H\x1b]2;a noticeably longer new title\x07cursor-state".to_vec();
        let d = make_diff(&base, &new);
        assert_eq!(apply_diff(&base, &d).as_deref(), Some(new.as_slice()));
    }

    // The #90 apply-stall, at the byte level: a diff built against a base the
    // client does not hold (here a SHORTER base than prefix+suffix) returns
    // None -> the DumpDiff applier surfaces ReackAndWait -> the screen wedges.
    // This is the exact condition observed live (prefix+suffix > applied_len).
    #[test]
    fn apply_diff_short_base_returns_none_the_wedge() {
        let server_base = b"PREFIX_oldmiddle_SUFFIX";
        let new = b"PREFIX_newmiddle_SUFFIX";
        let d = make_diff(server_base, new);
        // prefix("PREFIX_") + suffix("_SUFFIX") = 14 > 6.
        let client_base_short = b"PREFIX";
        assert_eq!(apply_diff(client_base_short, &d), None);
    }

    // The #94 hazard: apply_diff is content-blind — it validates only
    // prefix+suffix <= base.len(), never that the base matches what make_diff
    // saw. An EQUAL-length base that differs in content yields Some(garbage)
    // with no error: silent screen corruption (term_gen advances, no wedge).
    #[test]
    fn apply_diff_equal_len_divergence_corrupts_silently() {
        let server_base = b"PREFIX_old_SUFFIX"; // len 17
        let new = b"PREFIX_new_SUFFIX";
        let d = make_diff(server_base, new);
        let client_base_wrong = b"PREFXX_old_SUFFIX"; // len 17, byte 4 I->X
        let got = apply_diff(client_base_wrong, &d).expect("equal-len base never returns None");
        assert_ne!(
            got.as_slice(),
            new.as_slice(),
            "content-blind splice must mis-reconstruct on a divergent base (#94)",
        );
        assert_eq!(got, b"PREFXX_new_SUFFIX", "splices new middle between wrong prefix/suffix");
    }

    // Reproduction attempt for the apply-stall origin (#90/#2): reassembling a
    // single instruction's fragments must reproduce the payload byte-for-byte
    // regardless of arrival order or duplicates. A truncated or misordered
    // reassembly is a prime candidate for the client holding a shorter/wrong
    // dump than the server sent (FragmentAssembly had a wedge bug in #12).
    #[test]
    fn fragment_reassembly_round_trips_under_reorder_and_dup() {
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![42],
            (0u32..=255).map(|i| i as u8).collect(),
            (0..FRAGMENT_CONTENTS_MAX).map(|i| i as u8).collect(), // one full chunk
            (0..FRAGMENT_CONTENTS_MAX + 1).map(|i| i as u8).collect(), // spills to two
            (0..FRAGMENT_CONTENTS_MAX * 5 + 7).map(|i| i.wrapping_mul(31) as u8).collect(),
        ];
        // Deterministic LCG so a failure is reproducible.
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut rng = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for payload in &cases {
            let mut fr = Fragmenter::new();
            let frags = fr.make_fragments(payload, FRAGMENT_CONTENTS_MAX);
            for trial in 0..16 {
                let mut order = frags.clone();
                match trial {
                    0 => {}
                    1 => order.reverse(),
                    _ => {
                        for i in (1..order.len()).rev() {
                            let j = rng() % (i + 1);
                            order.swap(i, j);
                        }
                        if !order.is_empty() {
                            let a = rng() % order.len();
                            let b = rng() % order.len();
                            order.push(order[a].clone());
                            order.push(order[b].clone());
                        }
                    }
                }
                let mut asm = FragmentAssembly::new();
                let mut got = None;
                for f in order {
                    if let Some(p) = asm.add(f) {
                        got = Some(p);
                        break;
                    }
                }
                assert_eq!(
                    got.as_deref(),
                    Some(payload.as_slice()),
                    "payload {} bytes, trial {trial}: reassembly mismatch",
                    payload.len(),
                );
            }
        }
    }

    #[test]
    fn server_frame_roundtrip() {
        let cases = vec![
            ServerFrame {
                flags: 0,
                caps: vec![],
                frame_num: 7,
                input_ack: 99,
                echo_ack: 95,
                body: FrameBody::Full(b"dump".to_vec()),
            },
            ServerFrame {
                flags: FLAG_SHUTDOWN,
                caps: vec![],
                frame_num: 8,
                input_ack: 100,
                echo_ack: 100,
                body: FrameBody::Diff {
                    base: 7,
                    base_sum: None,
                    diff: b"delta".to_vec(),
                },
            },
            ServerFrame {
                flags: 0,
                caps: vec![],
                frame_num: 0,
                input_ack: 0,
                echo_ack: 0,
                body: FrameBody::Empty,
            },
            // FLAG_ECHO alone and combined with FLAG_SHUTDOWN both survive the
            // flags byte without disturbing caps decode (FDR 0006).
            ServerFrame {
                flags: FLAG_ECHO,
                caps: vec![],
                frame_num: 9,
                input_ack: 3,
                echo_ack: 3,
                body: FrameBody::Empty,
            },
            ServerFrame {
                flags: FLAG_SHUTDOWN | FLAG_ECHO,
                caps: vec![],
                frame_num: 10,
                input_ack: 4,
                echo_ack: 4,
                body: FrameBody::Full(b"x".to_vec()),
            },
            // Morph body (#15): base + forward escape-delta round-trip, base
            // distinct from frame_num as on the wire.
            ServerFrame {
                flags: 0,
                caps: vec![],
                frame_num: 11,
                input_ack: 5,
                echo_ack: 5,
                body: FrameBody::Morph {
                    base: 9,
                    base_sum: None,
                    escapes: b"\x1b[2;3Hx".to_vec(),
                },
            },
            // An empty escape-delta (no visible change but the base advanced)
            // must round-trip without being mistaken for a truncated body.
            ServerFrame {
                flags: 0,
                caps: vec![],
                frame_num: 12,
                input_ack: 6,
                echo_ack: 6,
                body: FrameBody::Morph {
                    base: 11,
                    base_sum: None,
                    escapes: vec![],
                },
            },
            // RFC 0006: checksummed Diff/Morph (BODY_DIFF_SUM / BODY_MORPH_SUM)
            // carry a u32 base checksum after the base; it must round-trip.
            ServerFrame {
                flags: 0,
                caps: vec![],
                frame_num: 13,
                input_ack: 7,
                echo_ack: 7,
                body: FrameBody::Diff {
                    base: 12,
                    base_sum: Some(0xdead_beef),
                    diff: b"sumdelta".to_vec(),
                },
            },
            ServerFrame {
                flags: 0,
                caps: vec![],
                frame_num: 14,
                input_ack: 8,
                echo_ack: 8,
                body: FrameBody::Morph {
                    base: 13,
                    base_sum: Some(0x0123_4567),
                    escapes: b"\x1b[5;5Hy".to_vec(),
                },
            },
        ];
        for frame in cases {
            assert_eq!(ServerFrame::decode(&frame.encode()).unwrap(), frame);
        }
        assert!(ServerFrame::decode(b"x").is_err());
    }

    #[test]
    fn scrollback_body_roundtrip() {
        // RFC 0002 §2: `base`, the appended count, and each row's len/bytes
        // survive encode→decode, including the empty (appended = 0) body.
        let cases = vec![
            ServerFrame {
                flags: 0,
                caps: vec![],
                frame_num: 5,
                input_ack: 10,
                echo_ack: 9,
                body: FrameBody::Scrollback {
                    base: 4,
                    rows: vec![b"first row\r\n".to_vec(), b"\x1b[31msecond\x1b[0m\r\n".to_vec()],
                },
            },
            ServerFrame {
                flags: 0,
                caps: vec![],
                frame_num: 6,
                input_ack: 0,
                echo_ack: 0,
                // appended = 0 is a valid no-op body and must roundtrip.
                body: FrameBody::Scrollback {
                    base: 6,
                    rows: vec![],
                },
            },
        ];
        for frame in cases {
            assert_eq!(ServerFrame::decode(&frame.encode()).unwrap(), frame);
        }
    }

    #[test]
    fn scrollback_body_rejects_row_past_body() {
        // RFC 0002 §2: a row length extending past the body must fail to
        // decode rather than over-read or panic.
        let good = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 1,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Scrollback {
                base: 0,
                rows: vec![b"hello".to_vec()],
            },
        }
        .encode();
        // Truncate inside the single row's bytes: the length header still
        // claims 5 bytes but only some remain.
        let mut truncated = good.clone();
        truncated.truncate(truncated.len() - 2);
        assert!(ServerFrame::decode(&truncated).is_err());
        // A bogus appended count far larger than the body is rejected before
        // any allocation, not parsed into a giant vector.
        let mut huge = good;
        let at = huge.len() - 5 /* "hello" */ - 2 /* row len */ - 4 /* appended */;
        huge[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(ServerFrame::decode(&huge).is_err());
    }

    #[test]
    fn client_message_caps_roundtrip_and_v0_compat() {
        // v0 bytes (no extension bit) decode to an empty table.
        let v0 = ClientMessage {
            flags: 0,
            caps: vec![],
            acked_frame: 1,
            rows: 24,
            cols: 80,
            input_base: 0,
            input: b"x".to_vec(),
        };
        let enc = v0.encode();
        assert_eq!(
            enc[0] & caps::FLAG_EXTENSION,
            0,
            "empty table must not set the bit"
        );
        assert_eq!(ClientMessage::decode(&enc).unwrap(), v0);

        // v1: table rides behind the bit; fixed fields and input survive,
        // and the EXTENSION bit never leaks into the decoded flags.
        let v1 = ClientMessage {
            flags: CLIENT_FLAG_SHUTDOWN,
            caps: caps::own_table(&[caps::Cap {
                id: caps::CAP_EXIT_STATUS,
                payload: vec![],
            }]),
            acked_frame: 9,
            rows: 50,
            cols: 132,
            input_base: 7,
            input: b"hi".to_vec(),
        };
        let enc = v1.encode();
        assert_ne!(enc[0] & caps::FLAG_EXTENSION, 0);
        let dec = ClientMessage::decode(&enc).unwrap();
        assert_eq!(dec, v1);
        assert_eq!(dec.flags & caps::FLAG_EXTENSION, 0);
    }

    #[test]
    fn server_frame_caps_roundtrip_and_v0_compat() {
        let table = caps::own_table(&[caps::Cap {
            id: caps::CAP_EXIT_STATUS,
            payload: vec![7],
        }]);
        for body in [
            FrameBody::Full(b"dump".to_vec()),
            FrameBody::Diff {
                base: 3,
                base_sum: None,
                diff: b"delta".to_vec(),
            },
            FrameBody::Empty,
        ] {
            for caps_case in [vec![], table.clone()] {
                let frame = ServerFrame {
                    flags: FLAG_SHUTDOWN,
                    caps: caps_case,
                    frame_num: 8,
                    input_ack: 100,
                    echo_ack: 99,
                    body: body.clone(),
                };
                let dec = ServerFrame::decode(&frame.encode()).unwrap();
                assert_eq!(dec, frame);
                assert_eq!(dec.flags, FLAG_SHUTDOWN);
            }
        }
    }

    #[test]
    fn truncated_caps_reject_the_message() {
        let mut enc = ClientMessage {
            flags: 0,
            caps: vec![caps::Cap {
                id: 1,
                payload: vec![1, 2, 3],
            }],
            acked_frame: 0,
            rows: 1,
            cols: 1,
            input_base: 0,
            input: vec![],
        }
        .encode();
        enc.truncate(3); // cut inside the table
        assert!(ClientMessage::decode(&enc).is_err());
        assert!(ServerFrame::decode(&enc).is_err());
    }

    #[test]
    fn frame_echo_ack_distinct_from_input_ack() {
        // The echo ack lags the input ack; both must roundtrip independently.
        let frame = ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: 3,
            input_ack: 42,
            echo_ack: 17,
            body: FrameBody::Empty,
        };
        let decoded = ServerFrame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.input_ack, 42);
        assert_eq!(decoded.echo_ack, 17);
    }

    #[test]
    fn client_message_roundtrip() {
        let msg = ClientMessage {
            flags: CLIENT_FLAG_SHUTDOWN,
            caps: vec![],
            acked_frame: 12,
            rows: 50,
            cols: 132,
            input_base: 1024,
            input: b"ls -la\r".to_vec(),
        };
        assert_eq!(ClientMessage::decode(&msg.encode()).unwrap(), msg);
        assert!(ClientMessage::decode(b"nope").is_err());
    }

    #[test]
    fn escape_overlay_flags_roundtrip_clear_of_extension() {
        // FDR 0008: the new runtime bits must not collide with the reserved
        // caps EXTENSION bit (0x02), and must survive encode/decode even when a
        // caps table is present (which sets EXTENSION on the wire).
        assert_eq!(CLIENT_FLAG_ESCAPE & caps::FLAG_EXTENSION, 0);
        assert_eq!(FLAG_OVERLAY & caps::FLAG_EXTENSION, 0);

        let table = caps::own_table(&[]); // non-empty (leads with protocol version)
        let cmsg = ClientMessage {
            flags: CLIENT_FLAG_ESCAPE | CLIENT_FLAG_SHUTDOWN,
            caps: table.clone(),
            acked_frame: 7,
            rows: 24,
            cols: 80,
            input_base: 3,
            input: b"hi".to_vec(),
        };
        let cback = ClientMessage::decode(&cmsg.encode()).unwrap();
        assert_eq!(cback.flags, CLIENT_FLAG_ESCAPE | CLIENT_FLAG_SHUTDOWN);
        assert_eq!(cback, cmsg);

        let frame = ServerFrame {
            flags: FLAG_OVERLAY | FLAG_ECHO,
            caps: table,
            frame_num: 9,
            input_ack: 2,
            echo_ack: 1,
            body: FrameBody::Empty,
        };
        let fback = ServerFrame::decode(&frame.encode()).unwrap();
        assert_eq!(fback.flags, FLAG_OVERLAY | FLAG_ECHO);
        assert_eq!(fback, frame);
    }

    #[test]
    fn echo_ack_advances_after_timeout() {
        let mut echo = EchoAck::new();
        assert_eq!(echo.ack(), 0);
        echo.record(5, 1000);
        // Not yet matured.
        assert!(!echo.update(1000 + ECHO_TIMEOUT - 1));
        assert_eq!(echo.ack(), 0);
        assert_eq!(echo.wait_time(1000), Some(ECHO_TIMEOUT));
        // Matured.
        assert!(echo.update(1000 + ECHO_TIMEOUT));
        assert_eq!(echo.ack(), 5);
        assert_eq!(echo.wait_time(2000), None);
        // No double-advance.
        assert!(!echo.update(10_000));
    }

    #[test]
    fn echo_ack_collapses_multiple_entries() {
        let mut echo = EchoAck::new();
        echo.record(3, 100);
        echo.record(3, 120); // duplicate offset ignored
        echo.record(9, 130);
        assert!(echo.update(100 + ECHO_TIMEOUT));
        assert_eq!(echo.ack(), 3);
        assert!(echo.update(130 + ECHO_TIMEOUT));
        assert_eq!(echo.ack(), 9);
    }

    #[test]
    fn input_outbox_ack_drains_prefix() {
        let mut ob = InputOutbox::new();
        ob.push(b"abc");
        ob.push(b"def");
        assert_eq!(ob.base(), 0);
        assert_eq!(ob.pending(), b"abcdef");
        ob.ack(4);
        assert_eq!(ob.base(), 4);
        assert_eq!(ob.pending(), b"ef");
        ob.ack(2); // stale ack ignored
        assert_eq!(ob.base(), 4);
        ob.ack(100); // overshoot clamps
        assert!(ob.is_empty());
        assert_eq!(ob.base(), 6);
    }

    #[test]
    fn scrollback_ring_appends_in_order_and_evicts_oldest() {
        let mut ring = ScrollbackRing::new(3);
        assert!(ring.is_empty());
        ring.append(&[b"a".to_vec(), b"b".to_vec()]);
        ring.append(&[b"c".to_vec()]);
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.row(0), Some(&b"a"[..]));
        assert_eq!(ring.row(2), Some(&b"c"[..]));
        // Past capacity the oldest rows fall off the front; order is kept.
        ring.append(&[b"d".to_vec(), b"e".to_vec()]);
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.row(0), Some(&b"c"[..]));
        assert_eq!(ring.row(1), Some(&b"d"[..]));
        assert_eq!(ring.row(2), Some(&b"e"[..]));
        assert_eq!(ring.row(3), None);
        ring.clear();
        assert!(ring.is_empty());
    }

    #[test]
    fn input_inbox_dedupes_retransmissions() {
        let mut ib = InputInbox::new();
        assert_eq!(ib.accept(0, b"abc"), Some(&b"abc"[..]));
        // Retransmission of the same tail plus new bytes.
        assert_eq!(ib.accept(0, b"abcdef"), Some(&b"def"[..]));
        // Pure retransmission: nothing new.
        assert_eq!(ib.accept(0, b"abcdef"), None);
        assert_eq!(ib.next_offset(), 6);
        // A gap is refused.
        assert_eq!(ib.accept(10, b"zz"), None);
        assert_eq!(ib.next_offset(), 6);
    }

    #[test]
    fn outbox_inbox_end_to_end() {
        let mut ob = InputOutbox::new();
        let mut ib = InputInbox::new();
        let mut applied = Vec::new();
        ob.push(b"first ");
        if let Some(new) = ib.accept(ob.base(), ob.pending()) {
            applied.extend_from_slice(new);
        }
        ob.push(b"second"); // ack for "first " lost: retransmit both
        if let Some(new) = ib.accept(ob.base(), ob.pending()) {
            applied.extend_from_slice(new);
        }
        ob.ack(ib.next_offset());
        assert!(ob.is_empty());
        assert_eq!(applied, b"first second");
    }

    fn rec(channel: u32, kind: RecordKind, payload: &[u8]) -> AgentRecord {
        AgentRecord {
            channel,
            kind,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn agent_record_roundtrip_each_kind() {
        for kind in [
            RecordKind::Open,
            RecordKind::Data,
            RecordKind::Close,
            RecordKind::Fail,
        ] {
            let r = rec(0xdead_beef, kind, b"payload");
            let mut dec = RecordDecoder::new();
            let got = dec.push(&r.to_bytes()).unwrap();
            assert_eq!(got, vec![r]);
        }
    }

    #[test]
    fn agent_record_empty_payload_roundtrips() {
        let r = rec(7, RecordKind::Close, b"");
        assert_eq!(r.to_bytes().len(), AGENT_RECORD_HEADER_LEN);
        let mut dec = RecordDecoder::new();
        assert_eq!(dec.push(&r.to_bytes()).unwrap(), vec![r]);
    }

    #[test]
    fn agent_record_multiple_in_one_push() {
        let a = rec(1, RecordKind::Open, b"");
        let b = rec(1, RecordKind::Data, b"hello");
        let c = rec(2, RecordKind::Data, b"world!!");
        let mut wire = a.to_bytes();
        wire.extend(b.to_bytes());
        wire.extend(c.to_bytes());
        let mut dec = RecordDecoder::new();
        assert_eq!(dec.push(&wire).unwrap(), vec![a, b, c]);
    }

    // ADR-0003: a read() boundary can fall anywhere — inside the header,
    // inside the payload, between records. The decoder must hold the partial
    // record until the rest arrives and never mis-frame.
    #[test]
    fn agent_record_reassembles_across_split_reads() {
        let r = rec(0x0102_0304, RecordKind::Data, b"a longer agent message body");
        let wire = r.to_bytes();
        for split in 1..wire.len() {
            let mut dec = RecordDecoder::new();
            let first = dec.push(&wire[..split]).unwrap();
            assert!(first.is_empty(), "premature record at split {split}");
            let second = dec.push(&wire[split..]).unwrap();
            assert_eq!(second, vec![r.clone()], "bad reassembly at split {split}");
        }
    }

    #[test]
    fn agent_record_reassembles_byte_at_a_time() {
        let a = rec(5, RecordKind::Data, b"chunk-one");
        let b = rec(5, RecordKind::Close, b"");
        let mut wire = a.to_bytes();
        wire.extend(b.to_bytes());
        let mut dec = RecordDecoder::new();
        let mut got = Vec::new();
        for &byte in &wire {
            got.extend(dec.push(&[byte]).unwrap());
        }
        assert_eq!(got, vec![a, b]);
    }

    // The codec rides the same cumulative byte stream as keystrokes: prove a
    // record survives outbox retransmission + inbox dedupe and decodes once.
    #[test]
    fn agent_record_through_outbox_inbox_stream() {
        let a = rec(1, RecordKind::Open, b"");
        let b = rec(1, RecordKind::Data, b"sign-request-bytes");
        let mut ob = InputOutbox::new();
        let mut ib = InputInbox::new();
        let mut dec = RecordDecoder::new();
        let mut got = Vec::new();

        ob.push(&a.to_bytes());
        if let Some(fresh) = ib.accept(ob.base(), ob.pending()) {
            got.extend(dec.push(fresh).unwrap());
        }
        // Ack for the first record is lost: the outbox retransmits it ahead of
        // the new bytes; the inbox must hand the decoder only the fresh suffix,
        // so the first record is not decoded twice.
        ob.push(&b.to_bytes());
        if let Some(fresh) = ib.accept(ob.base(), ob.pending()) {
            got.extend(dec.push(fresh).unwrap());
        }
        ob.ack(ib.next_offset());
        assert!(ob.is_empty());
        assert_eq!(got, vec![a, b]);
    }

    #[test]
    fn agent_record_unknown_kind_is_rejected() {
        // channel 0, kind 9 (undefined), len 0.
        let mut wire = 0u32.to_be_bytes().to_vec();
        wire.push(9);
        wire.extend(0u32.to_be_bytes());
        let mut dec = RecordDecoder::new();
        assert!(dec.push(&wire).is_err());
    }

    #[test]
    fn agent_record_oversized_len_is_rejected_not_allocated() {
        // A header claiming a payload past the cap must be refused before any
        // attempt to buffer toward it — never trust an authenticated peer's len.
        let mut wire = 0u32.to_be_bytes().to_vec();
        wire.push(RecordKind::Data.to_byte());
        wire.extend(((AGENT_RECORD_PAYLOAD_MAX + 1) as u32).to_be_bytes());
        let mut dec = RecordDecoder::new();
        assert!(dec.push(&wire).is_err());
    }

    #[test]
    fn agent_record_payload_at_cap_is_accepted() {
        let r = rec(3, RecordKind::Data, &vec![0xab; AGENT_RECORD_PAYLOAD_MAX]);
        let mut dec = RecordDecoder::new();
        assert_eq!(dec.push(&r.to_bytes()).unwrap(), vec![r]);
    }

    // The wrapper bundles outbox+inbox+decoder; drive a full request/response
    // round-trip across the pair (client opens a channel and sends a request;
    // server replies; acks lag a step) and confirm each side decodes the
    // other's records exactly once despite the retransmitted unacked tail.
    #[test]
    fn agent_stream_bidirectional_request_response() {
        let mut client = AgentStream::new();
        let mut server = AgentStream::new();

        // Client -> server: OPEN then a request, both still unacked.
        client.send(&rec(1, RecordKind::Open, b""));
        client.send(&rec(1, RecordKind::Data, b"sign please"));
        let got = server
            .recv(client.send_base(), client.pending())
            .unwrap();
        assert_eq!(
            got,
            vec![rec(1, RecordKind::Open, b""), rec(1, RecordKind::Data, b"sign please")]
        );
        // Server acks what it consumed; client's outbox drains.
        client.ack(server.recv_ack());
        assert!(!client.has_pending());

        // Server -> client: the signature, plus close.
        server.send(&rec(1, RecordKind::Data, b"signature"));
        server.send(&rec(1, RecordKind::Close, b""));
        // First delivery lost: the client never acks, so the server's next
        // send retransmits the whole unacked tail. The client must still
        // surface each record once, not twice.
        let _dropped = server.pending().to_vec();
        let got = client
            .recv(server.send_base(), server.pending())
            .unwrap();
        assert_eq!(
            got,
            vec![rec(1, RecordKind::Data, b"signature"), rec(1, RecordKind::Close, b"")]
        );
        // A pure retransmission of the same tail yields nothing new.
        assert!(client.recv(server.send_base(), server.pending()).unwrap().is_empty());
        client.ack(server.recv_ack());
        server.ack(client.recv_ack());
        assert!(!server.has_pending());
    }

    #[test]
    fn agent_stream_surfaces_decoder_error() {
        let mut s = AgentStream::new();
        // A record with an undefined kind byte reaches the decoder and errors.
        let mut wire = 0u32.to_be_bytes().to_vec();
        wire.push(9);
        wire.extend(0u32.to_be_bytes());
        assert!(s.recv(0, &wire).is_err());
    }
}
