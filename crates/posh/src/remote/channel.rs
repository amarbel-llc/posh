//! RFC 0011 multiplexed-channel primitives: the 9-byte channel envelope
//! (§2), the self-describing partitioned `u64` channel identifier and its
//! per-role allocator (§3), and the agent-channel payload codec (§5).
//!
//! Wire-only building blocks: nothing here touches sockets or event loops.
//! The envelope sits between fragment reassembly and message decode, so the
//! codecs in posh-proto stay untouched (verified by
//! `remote::sync::tests::a_nine_byte_envelope_prefix_leaves_both_codecs_verbatim`).

// Consumed by the event loops from the wire-increment plan's Tasks 4-5
// (docs/plans/2026-07-28-rfc0011-wire-increment-impl.md); the allow goes
// with the first non-test consumer.
#![allow(dead_code)]

use crate::util::{Error, Result};

/// §2: the only envelope version this implementation speaks.
pub const VER_1: u8 = 0x01;
/// §2: ver byte + u64 LE channel identifier.
pub const ENVELOPE_LEN: usize = 9;

/// §3.2: channel kinds.
pub const KIND_SESSION: u8 = 0;
pub const KIND_AGENT: u8 = 1;

/// §3: bit 0 initiator (0 = client, 1 = server), bits 1..7 kind,
/// bits 8..63 ordinal. Ordinal 0 is reserved, so raw id 0 (the
/// connection-control identifier) is never a data channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChannelId(pub u64);

impl ChannelId {
    /// §3.1: reserved for connection-level control; never carries data.
    pub const CONTROL: ChannelId = ChannelId(0);

    pub fn new(server_initiated: bool, kind: u8, ordinal: u64) -> ChannelId {
        ChannelId((server_initiated as u64) | ((kind as u64 & 0x7f) << 1) | (ordinal << 8))
    }

    pub fn server_initiated(self) -> bool {
        self.0 & 1 == 1
    }

    pub fn kind(self) -> u8 {
        ((self.0 >> 1) & 0x7f) as u8
    }

    pub fn ordinal(self) -> u64 {
        self.0 >> 8
    }

    /// A data channel has a nonzero ordinal (§3.1).
    pub fn is_data(self) -> bool {
        self.ordinal() != 0
    }
}

/// The role a peer allocates identifiers for (§3.1: only its own space).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Client,
    Server,
}

/// §3.1: per-(initiator, kind) monotonic ordinal allocation starting at 1.
pub struct ChannelAllocator {
    role: Role,
    next_ordinal: [u64; 128],
}

impl ChannelAllocator {
    pub fn new(role: Role) -> ChannelAllocator {
        ChannelAllocator {
            role,
            next_ordinal: [1; 128],
        }
    }

    pub fn next(&mut self, kind: u8) -> ChannelId {
        let slot = &mut self.next_ordinal[(kind & 0x7f) as usize];
        let ordinal = *slot;
        // §3.1: never reuse or wrap; 2^56 makes exhaustion unreachable, and
        // the checked add turns the impossible case into a loud failure
        // rather than a silent wrap.
        *slot = slot.checked_add(1).expect("channel ordinal space exhausted");
        ChannelId::new(self.role == Role::Server, kind, ordinal)
    }
}

/// §2: the envelope carried once per instruction, above fragmentation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Envelope {
    pub ver: u8,
    pub channel: ChannelId,
}

impl Envelope {
    pub fn new(channel: ChannelId) -> Envelope {
        Envelope {
            ver: VER_1,
            channel,
        }
    }

    /// Prepends this envelope to `out` (call before appending the message).
    pub fn encode_to(&self, out: &mut Vec<u8>) {
        out.push(self.ver);
        out.extend_from_slice(&self.channel.0.to_le_bytes());
    }

    /// Parses an envelope off the front of a reassembled instruction,
    /// returning the remaining payload. Rejects any `ver` other than
    /// `VER_1` and inputs shorter than the envelope (§2: discard the
    /// instruction, never tear down the connection — that policy is the
    /// caller's; this just reports the error).
    pub fn parse(input: &[u8]) -> Result<(Envelope, &[u8])> {
        if input.len() < ENVELOPE_LEN {
            return Err(Error::from("truncated channel envelope"));
        }
        if input[0] != VER_1 {
            return Err(Error::from("unknown channel envelope version"));
        }
        let channel = ChannelId(u64::from_le_bytes(input[1..9].try_into().unwrap()));
        Ok((
            Envelope {
                ver: input[0],
                channel,
            },
            &input[ENVELOPE_LEN..],
        ))
    }
}

/// §5 agent-channel payload flag bits.
pub const AGENT_FLAG_OPEN: u8 = 0x01;
pub const AGENT_FLAG_CLOSE: u8 = 0x02;
pub const AGENT_FLAG_FAIL: u8 = 0x04;
const AGENT_FLAGS_KNOWN: u8 = AGENT_FLAG_OPEN | AGENT_FLAG_CLOSE | AGENT_FLAG_FAIL;
/// §5 header: flags u8 + send_base u64 LE + recv_ack u64 LE.
pub const AGENT_PAYLOAD_HEADER_LEN: usize = 17;

/// §5: the payload of one `agent` channel instruction. `send_base` is the
/// offset of `data`'s first byte in this channel's cumulative outbound
/// stream; `recv_ack` cumulatively acknowledges the peer's stream.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AgentPayload {
    pub flags: u8,
    pub send_base: u64,
    pub recv_ack: u64,
    pub data: Vec<u8>,
}

impl AgentPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(AGENT_PAYLOAD_HEADER_LEN + self.data.len());
        out.push(self.flags);
        out.extend_from_slice(&self.send_base.to_le_bytes());
        out.extend_from_slice(&self.recv_ack.to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// Rejects a truncated header. Unknown flag bits do NOT reject here —
    /// §5 says the receiver ignores such instructions rather than guessing,
    /// so the caller checks `has_unknown_flags()` and discards.
    pub fn decode(input: &[u8]) -> Result<AgentPayload> {
        if input.len() < AGENT_PAYLOAD_HEADER_LEN {
            return Err(Error::from("truncated agent-channel payload"));
        }
        Ok(AgentPayload {
            flags: input[0],
            send_base: u64::from_le_bytes(input[1..9].try_into().unwrap()),
            recv_ack: u64::from_le_bytes(input[9..17].try_into().unwrap()),
            data: input[AGENT_PAYLOAD_HEADER_LEN..].to_vec(),
        })
    }

    pub fn has_unknown_flags(&self) -> bool {
        self.flags & !AGENT_FLAGS_KNOWN != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_partition_roundtrips_initiator_kind_ordinal() {
        let c = ChannelId::new(false, KIND_SESSION, 1);
        assert!(!c.server_initiated());
        assert_eq!(c.kind(), KIND_SESSION);
        assert_eq!(c.ordinal(), 1);
        assert!(c.is_data());

        let s = ChannelId::new(true, KIND_AGENT, 0x00ff_ffff_ffff_ffff);
        assert!(s.server_initiated());
        assert_eq!(s.kind(), KIND_AGENT);
        assert_eq!(s.ordinal(), 0x00ff_ffff_ffff_ffff);

        // The two spaces never collide even at equal kind/ordinal.
        assert_ne!(
            ChannelId::new(false, KIND_AGENT, 7),
            ChannelId::new(true, KIND_AGENT, 7)
        );
    }

    #[test]
    fn allocator_starts_at_one_and_is_monotonic_per_kind() {
        let mut a = ChannelAllocator::new(Role::Server);
        let first_agent = a.next(KIND_AGENT);
        let second_agent = a.next(KIND_AGENT);
        let first_session = a.next(KIND_SESSION);
        assert_eq!(first_agent.ordinal(), 1);
        assert_eq!(second_agent.ordinal(), 2);
        assert_eq!(first_session.ordinal(), 1, "ordinals are per kind");
        assert!(first_agent.server_initiated());
        assert!(first_agent.is_data());
        assert_ne!(first_agent, second_agent);
    }

    #[test]
    fn envelope_roundtrip_prefixes_nine_bytes() {
        let id = ChannelId::new(false, KIND_SESSION, 1);
        let mut wire = Vec::new();
        Envelope::new(id).encode_to(&mut wire);
        assert_eq!(wire.len(), ENVELOPE_LEN);
        wire.extend_from_slice(b"payload");
        let (env, rest) = Envelope::parse(&wire).unwrap();
        assert_eq!(env.ver, VER_1);
        assert_eq!(env.channel, id);
        assert_eq!(rest, b"payload");
    }

    #[test]
    fn envelope_rejects_unknown_ver_and_truncation() {
        let id = ChannelId::new(true, KIND_AGENT, 3);
        let mut wire = Vec::new();
        Envelope::new(id).encode_to(&mut wire);
        wire[0] = 0x02;
        assert!(Envelope::parse(&wire).is_err(), "unknown ver must be rejected");
        let mut ok = Vec::new();
        Envelope::new(id).encode_to(&mut ok);
        assert!(
            Envelope::parse(&ok[..ENVELOPE_LEN - 1]).is_err(),
            "truncated envelope must be rejected"
        );
        // An envelope with no payload is well-formed (empty payload).
        assert_eq!(Envelope::parse(&ok).unwrap().1, b"");
    }

    #[test]
    fn agent_payload_roundtrips_including_empty_data() {
        for data in [Vec::new(), b"agent bytes".to_vec()] {
            let p = AgentPayload {
                flags: AGENT_FLAG_OPEN,
                send_base: 0,
                recv_ack: 42,
                data,
            };
            let decoded = AgentPayload::decode(&p.encode()).unwrap();
            assert_eq!(decoded, p);
            assert!(!decoded.has_unknown_flags());
        }
    }

    #[test]
    fn agent_payload_rejects_truncated_header() {
        let wire = AgentPayload {
            flags: 0,
            send_base: 1,
            recv_ack: 2,
            data: vec![9],
        }
        .encode();
        assert!(AgentPayload::decode(&wire[..AGENT_PAYLOAD_HEADER_LEN - 1]).is_err());
    }

    #[test]
    fn agent_payload_flags_unknown_bits_detected() {
        let p = AgentPayload {
            flags: AGENT_FLAG_CLOSE | 0x40,
            send_base: 3,
            recv_ack: 4,
            data: Vec::new(),
        };
        let decoded = AgentPayload::decode(&p.encode()).unwrap();
        assert!(decoded.has_unknown_flags(), "reserved bit 0x40 must surface");
    }

    #[test]
    fn agent_payload_larger_than_retired_247_budget_roundtrips() {
        let p = AgentPayload {
            flags: 0,
            send_base: 1000,
            recv_ack: 2000,
            data: vec![0x5a; 4096],
        };
        let decoded = AgentPayload::decode(&p.encode()).unwrap();
        assert_eq!(decoded.data.len(), 4096, "no 247-byte entry ceiling remains");
        assert_eq!(decoded, p);
    }

    #[test]
    fn control_identifier_is_never_allocated_and_is_rejected_as_data() {
        assert!(!ChannelId::CONTROL.is_data());
        let mut a = ChannelAllocator::new(Role::Client);
        for kind in [KIND_SESSION, KIND_AGENT] {
            let id = a.next(kind);
            assert_ne!(id, ChannelId::CONTROL);
            assert!(id.is_data());
        }
    }
}
