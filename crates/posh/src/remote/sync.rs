//! State-synchronization building blocks: datagram fragmentation (port of
//! mosh transportfragment.cc), frame/message encodings, a prefix/suffix
//! binary diff, and the reliable cumulative user-input stream (a simplified
//! mosh UserStream).

use crate::util::{Error, Result};

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
    pub flags: u8,
    pub frame_num: u64,
    pub input_ack: u64,
    pub body: FrameBody,
}

const BODY_FULL: u8 = 0;
const BODY_DIFF: u8 = 1;
const BODY_EMPTY: u8 = 2;

impl ServerFrame {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(self.flags);
        out.extend_from_slice(&self.frame_num.to_le_bytes());
        out.extend_from_slice(&self.input_ack.to_le_bytes());
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
        if data.len() < 18 {
            return Err(Error::from("server frame too short"));
        }
        let flags = data[0];
        let frame_num = u64::from_le_bytes(data[1..9].try_into().unwrap());
        let input_ack = u64::from_le_bytes(data[9..17].try_into().unwrap());
        let body = match data[17] {
            BODY_FULL => FrameBody::Full(data[18..].to_vec()),
            BODY_DIFF => {
                if data.len() < 26 {
                    return Err(Error::from("diff frame too short"));
                }
                let base = u64::from_le_bytes(data[18..26].try_into().unwrap());
                FrameBody::Diff {
                    base,
                    diff: data[26..].to_vec(),
                }
            }
            BODY_EMPTY => FrameBody::Empty,
            _ => return Err(Error::from("unknown frame body kind")),
        };
        Ok(ServerFrame {
            flags,
            frame_num,
            input_ack,
            body,
        })
    }
}

// ---------------------------------------------------------------------------
// Client->server messages: frame ack, current terminal size, and the unacked
// tail of the cumulative input byte stream.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientMessage {
    pub acked_frame: u64,
    pub rows: u16,
    pub cols: u16,
    pub input_base: u64,
    pub input: Vec<u8>,
}

impl ClientMessage {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(20 + self.input.len());
        out.extend_from_slice(&self.acked_frame.to_le_bytes());
        out.extend_from_slice(&self.rows.to_le_bytes());
        out.extend_from_slice(&self.cols.to_le_bytes());
        out.extend_from_slice(&self.input_base.to_le_bytes());
        out.extend_from_slice(&self.input);
        out
    }

    pub fn decode(data: &[u8]) -> Result<ClientMessage> {
        if data.len() < 20 {
            return Err(Error::from("client message too short"));
        }
        Ok(ClientMessage {
            acked_frame: u64::from_le_bytes(data[0..8].try_into().unwrap()),
            rows: u16::from_le_bytes([data[8], data[9]]),
            cols: u16::from_le_bytes([data[10], data[11]]),
            input_base: u64::from_le_bytes(data[12..20].try_into().unwrap()),
            input: data[20..].to_vec(),
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
                frame_num: 7,
                input_ack: 99,
                body: FrameBody::Full(b"dump".to_vec()),
            },
            ServerFrame {
                flags: FLAG_SHUTDOWN,
                frame_num: 8,
                input_ack: 100,
                body: FrameBody::Diff {
                    base: 7,
                    diff: b"delta".to_vec(),
                },
            },
            ServerFrame {
                flags: 0,
                frame_num: 0,
                input_ack: 0,
                body: FrameBody::Empty,
            },
        ];
        for frame in cases {
            assert_eq!(ServerFrame::decode(&frame.encode()).unwrap(), frame);
        }
        assert!(ServerFrame::decode(b"x").is_err());
    }

    #[test]
    fn client_message_roundtrip() {
        let msg = ClientMessage {
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
