//! State-synchronization building blocks: datagram fragmentation (port of
//! mosh transportfragment.cc), frame/message encodings, a prefix/suffix
//! binary diff, and the reliable cumulative user-input stream (a simplified
//! mosh UserStream).

use crate::remote::caps;
use crate::util::{Error, Result};

/// Keepalive cadence shared by both ends of the protocol: each side emits
/// an empty message/frame when this long has passed since its last send.
pub const HEARTBEAT_INTERVAL: u64 = 3000; // ms

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

// ---------------------------------------------------------------------------
// Prefix/suffix binary diff: [u32 LE prefix][u32 LE suffix][middle bytes],
// where prefix/suffix are shared with the base and middle replaces the rest.

pub fn make_diff(old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut prefix = 0usize;
    let max_prefix = old.len().min(new.len());
    while prefix < max_prefix && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    let max_suffix = max_prefix - prefix;
    while suffix < max_suffix && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix] {
        suffix += 1;
    }
    let mut out = Vec::with_capacity(8 + new.len() - prefix - suffix);
    out.extend_from_slice(&(prefix as u32).to_le_bytes());
    out.extend_from_slice(&(suffix as u32).to_le_bytes());
    out.extend_from_slice(&new[prefix..new.len() - suffix]);
    out
}

pub fn apply_diff(old: &[u8], diff: &[u8]) -> Option<Vec<u8>> {
    if diff.len() < 8 {
        return None;
    }
    let prefix = u32::from_le_bytes(diff[0..4].try_into().ok()?) as usize;
    let suffix = u32::from_le_bytes(diff[4..8].try_into().ok()?) as usize;
    if prefix + suffix > old.len() {
        return None;
    }
    let middle = &diff[8..];
    let mut out = Vec::with_capacity(prefix + middle.len() + suffix);
    out.extend_from_slice(&old[..prefix]);
    out.extend_from_slice(middle);
    out.extend_from_slice(&old[old.len() - suffix..]);
    Some(out)
}

// ---------------------------------------------------------------------------
// Server->client frames. A frame is the unit of screen-state sync: either a
// full dump_vt stream, a diff against the client-acked frame, or an empty
// ack/heartbeat carrier.

pub const FLAG_SHUTDOWN: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameBody {
    Full(Vec<u8>),
    Diff { base: u64, diff: Vec<u8> },
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFrame {
    /// Runtime signal bits only (FLAG_SHUTDOWN). The EXTENSION bit is a
    /// wire-format detail: set on encode when `caps` is non-empty,
    /// stripped on decode.
    pub flags: u8,
    /// RFC 0001 §3 capability table; empty == baseline (v0) format.
    pub caps: Vec<caps::Cap>,
    pub frame_num: u64,
    /// Input-stream offset received (clears the client's outbox).
    pub input_ack: u64,
    /// Input-stream offset whose application echo is reflected in this
    /// frame's screen state (mosh's echo ack; validates predictions).
    pub echo_ack: u64,
    pub body: FrameBody,
}

const BODY_FULL: u8 = 0;
const BODY_DIFF: u8 = 1;
const BODY_EMPTY: u8 = 2;

impl ServerFrame {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(flags_with_extension(self.flags, &self.caps));
        if !self.caps.is_empty() {
            out.extend_from_slice(&caps::encode_table(&self.caps));
        }
        out.extend_from_slice(&self.frame_num.to_le_bytes());
        out.extend_from_slice(&self.input_ack.to_le_bytes());
        out.extend_from_slice(&self.echo_ack.to_le_bytes());
        match &self.body {
            FrameBody::Full(bytes) => {
                out.push(BODY_FULL);
                out.extend_from_slice(bytes);
            }
            FrameBody::Diff { base, diff } => {
                out.push(BODY_DIFF);
                out.extend_from_slice(&base.to_le_bytes());
                out.extend_from_slice(diff);
            }
            FrameBody::Empty => out.push(BODY_EMPTY),
        }
        out
    }

    pub fn decode(data: &[u8]) -> Result<ServerFrame> {
        let (flags, caps, at) = decode_flags_and_caps(data)?;
        if data.len() < at + 25 {
            return Err(Error::from("server frame too short"));
        }
        let frame_num = u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
        let input_ack = u64::from_le_bytes(data[at + 8..at + 16].try_into().unwrap());
        let echo_ack = u64::from_le_bytes(data[at + 16..at + 24].try_into().unwrap());
        let at = at + 24;
        let body = match data[at] {
            BODY_FULL => FrameBody::Full(data[at + 1..].to_vec()),
            BODY_DIFF => {
                if data.len() < at + 9 {
                    return Err(Error::from("diff frame too short"));
                }
                let base = u64::from_le_bytes(data[at + 1..at + 9].try_into().unwrap());
                FrameBody::Diff {
                    base,
                    diff: data[at + 9..].to_vec(),
                }
            }
            BODY_EMPTY => FrameBody::Empty,
            _ => return Err(Error::from("unknown frame body kind")),
        };
        Ok(ServerFrame {
            flags,
            caps,
            frame_num,
            input_ack,
            echo_ack,
            body,
        })
    }
}

/// Sets the EXTENSION bit when a table will follow the flags byte.
fn flags_with_extension(flags: u8, table: &[caps::Cap]) -> u8 {
    debug_assert_eq!(flags & caps::FLAG_EXTENSION, 0, "0x02 is reserved");
    if table.is_empty() {
        flags
    } else {
        flags | caps::FLAG_EXTENSION
    }
}

/// Reads the flags byte and, when the EXTENSION bit is set, the capability
/// table behind it. Returns (runtime flags, table, offset of the fixed
/// fields). Baseline (v0) peers never set the bit, so they parse with an
/// empty table at offset 1 — exactly the pre-capability format.
fn decode_flags_and_caps(data: &[u8]) -> Result<(u8, Vec<caps::Cap>, usize)> {
    let Some(&first) = data.first() else {
        return Err(Error::from("message too short"));
    };
    if first & caps::FLAG_EXTENSION == 0 {
        return Ok((first, Vec::new(), 1));
    }
    let (table, used) = caps::decode_table(&data[1..])?;
    Ok((first & !caps::FLAG_EXTENSION, table, 1 + used))
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
        ];
        for frame in cases {
            assert_eq!(ServerFrame::decode(&frame.encode()).unwrap(), frame);
        }
        assert!(ServerFrame::decode(b"x").is_err());
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
}
