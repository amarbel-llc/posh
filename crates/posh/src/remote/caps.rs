//! RFC 0001 §3: the TLV capability table that rides behind the EXTENSION
//! bit (0x02) of both datagram directions. Unknown ids are preserved on
//! decode and ignored by consumers; malformed tables reject the message.

use crate::util::{Error, Result};

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

/// Max agent-stream bytes carried by one [`CAP_AGENT_DATA`] entry: the table's
/// `len: u8` budget (255) minus the 8-byte `u64` offset prefix. Keeping agent
/// data as length-prefixed entries (rather than a negotiated body-layout
/// change) leaves the message bodies byte-identical in every negotiation
/// state, at ~1.2% framing overhead.
#[allow(dead_code)]
pub const AGENT_DATA_MAX: usize = u8::MAX as usize - 8; // 247

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
/// empty `data` produces no entries (nothing to (re)transmit). The number of
/// entries one message can hold is bounded by the table's `count: u8`.
#[allow(dead_code)]
pub fn encode_agent_data(base: u64, data: &[u8]) -> Vec<Cap> {
    let mut out = Vec::new();
    let mut offset = base;
    for chunk in data.chunks(AGENT_DATA_MAX) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
