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
}
