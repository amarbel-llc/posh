//! Server->client frame wire types, extracted from `posh`'s `remote::sync`
//! (github #75) so `poshterity` can drive the codecs without a dependency
//! cycle. Holds the `FrameBody`/`ServerFrame` encodings, the prefix/suffix
//! binary diff the `DumpDiff` codec builds on, and the base-integrity checksum.
//! The datagram fragmentation, the reliable input/echo/agent streams, and the
//! client->server `ClientMessage` stay in `posh`'s `remote::sync`, which
//! re-imports these types.

use crate::caps;
use crate::error::{Error, Result};

/// Keepalive cadence shared by both ends of the protocol: each side emits
/// an empty message/frame when this long has passed since its last send.
pub const HEARTBEAT_INTERVAL: u64 = 3000; // ms

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

/// FNV-1a 32-bit over a diff base's bytes (RFC 0006, `CAP_BASE_SUM`). A cheap,
/// dependency-free integrity tag so the client can confirm it holds the same
/// diff base the server diffed against before applying a content-blind
/// prefix/suffix diff. Not cryptographic — the datagram is already AEAD-sealed;
/// this only needs to catch an accidental base divergence.
pub fn base_checksum(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ---------------------------------------------------------------------------
// Server->client frames. A frame is the unit of screen-state sync: either a
// full dump_vt stream, a diff against the client-acked frame, or an empty
// ack/heartbeat carrier.

pub const FLAG_SHUTDOWN: u8 = 1;
/// The remote PTY's line-discipline `ECHO` is on, reported per frame. Lets an
/// optimistic-echo client (`POSH_PREDICTION=optimistic`, FDR 0006) know it is
/// safe to echo keystrokes locally; cleared at password prompts and raw-mode
/// apps so their input is not shown. `0x02` is the reserved caps EXTENSION bit,
/// so this is the next free runtime bit.
pub const FLAG_ECHO: u8 = 4;
/// An escape-to-shell overlay is active (FDR 0008): the frames carry the
/// transient shell's screen, not the live session. Echoed back to the client so
/// it knows its `CLIENT_FLAG_ESCAPE` request was honored (and can stop
/// retransmitting it). `0x08` is the next free runtime bit after FLAG_ECHO (0x02
/// is the reserved caps EXTENSION bit).
pub const FLAG_OVERLAY: u8 = 8;
/// The server's debug logging (the `POSH_DEBUG_LOG` sink) is currently on,
/// reported per frame so the client's "Server debug logging" palette command can
/// show the true state and confirm a toggle (#3). `0x10` is the next free
/// runtime bit after FLAG_OVERLAY.
pub const FLAG_SERVER_LOG: u8 = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameBody {
    Full(Vec<u8>),
    /// `base_sum` (RFC 0006, `CAP_BASE_SUM`): when `Some`, `base_checksum` of the
    /// server's diff base. The client verifies it against its own held dump
    /// before applying and re-acks + resyncs on a mismatch, turning a silent
    /// base divergence (#94) or a short-base wedge into a clean recovery. `None`
    /// for a baseline peer (plain `BODY_DIFF`).
    Diff {
        base: u64,
        base_sum: Option<u32>,
        diff: Vec<u8>,
    },
    Empty,
    /// Incremental visible-frame sync (#15, `CAP_MORPH`): a minimal forward
    /// escape-delta (`display::new_frame`) that morphs the client's existing
    /// terminal model from frame `base` to this frame, instead of shipping a
    /// full dump for it to reparse. `base` is the acked frame the escapes were
    /// computed against; the client applies them only when it is exactly at
    /// `base` (`base == applied_num`), so a retransmitted or superseding body
    /// is anchored exactly like `Diff`. On a base mismatch the client re-acks
    /// and the server falls back to a `Full` keyframe.
    Morph {
        base: u64,
        base_sum: Option<u32>,
        escapes: Vec<u8>,
    },
    /// Scrollback growth (RFC 0002 §2): the rows that newly entered the
    /// server's primary-screen scrollback since the frame the client last
    /// acknowledged. `base` is that acked frame number — the client appends
    /// `rows` only when it is exactly at `base` (`base == applied_num`),
    /// which makes a retransmitted or superseding body idempotent under
    /// loss, exactly as `Diff` is anchored to its base. Each row is a
    /// self-contained `dump_scrollback_row` byte stream (wrap implied by the
    /// absence of a trailing newline). Visible screen state is unchanged by
    /// this body.
    Scrollback { base: u64, rows: Vec<Vec<u8>> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFrame {
    /// Runtime signal bits only (FLAG_SHUTDOWN, FLAG_ECHO). The EXTENSION bit
    /// is a wire-format detail: set on encode when `caps` is non-empty,
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
const BODY_SCROLLBACK: u8 = 3;
const BODY_MORPH: u8 = 4;
/// Checksummed visible-body variants (RFC 0006, `CAP_BASE_SUM`): a `BODY_DIFF` /
/// `BODY_MORPH` whose `base` u64 is immediately followed by a u32
/// `base_checksum` of the server's diff base, letting the client detect a
/// divergent base before applying.
const BODY_DIFF_SUM: u8 = 5;
const BODY_MORPH_SUM: u8 = 6;

/// Upper bound on rows in one `BODY_SCROLLBACK` body. A single frame's
/// payload is already bounded by the fragmentation layer, but `appended` is
/// attacker-controlled by an authenticated peer (RFC 0002 Security
/// Considerations): cap it so a hostile count cannot drive an unbounded
/// allocation before the rows themselves are parsed.
const MAX_SCROLLBACK_ROWS: usize = 1 << 20;

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
            FrameBody::Diff {
                base,
                base_sum,
                diff,
            } => {
                out.push(if base_sum.is_some() { BODY_DIFF_SUM } else { BODY_DIFF });
                out.extend_from_slice(&base.to_le_bytes());
                if let Some(sum) = base_sum {
                    out.extend_from_slice(&sum.to_le_bytes());
                }
                out.extend_from_slice(diff);
            }
            FrameBody::Morph {
                base,
                base_sum,
                escapes,
            } => {
                out.push(if base_sum.is_some() {
                    BODY_MORPH_SUM
                } else {
                    BODY_MORPH
                });
                out.extend_from_slice(&base.to_le_bytes());
                if let Some(sum) = base_sum {
                    out.extend_from_slice(&sum.to_le_bytes());
                }
                out.extend_from_slice(escapes);
            }
            FrameBody::Empty => out.push(BODY_EMPTY),
            FrameBody::Scrollback { base, rows } => {
                out.push(BODY_SCROLLBACK);
                out.extend_from_slice(&base.to_le_bytes());
                out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
                for row in rows {
                    out.extend_from_slice(&(row.len() as u16).to_le_bytes());
                    out.extend_from_slice(row);
                }
            }
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
                    base_sum: None,
                    diff: data[at + 9..].to_vec(),
                }
            }
            BODY_DIFF_SUM => {
                if data.len() < at + 13 {
                    return Err(Error::from("diff-sum frame too short"));
                }
                let base = u64::from_le_bytes(data[at + 1..at + 9].try_into().unwrap());
                let sum = u32::from_le_bytes(data[at + 9..at + 13].try_into().unwrap());
                FrameBody::Diff {
                    base,
                    base_sum: Some(sum),
                    diff: data[at + 13..].to_vec(),
                }
            }
            BODY_MORPH => {
                if data.len() < at + 9 {
                    return Err(Error::from("morph frame too short"));
                }
                let base = u64::from_le_bytes(data[at + 1..at + 9].try_into().unwrap());
                FrameBody::Morph {
                    base,
                    base_sum: None,
                    escapes: data[at + 9..].to_vec(),
                }
            }
            BODY_MORPH_SUM => {
                if data.len() < at + 13 {
                    return Err(Error::from("morph-sum frame too short"));
                }
                let base = u64::from_le_bytes(data[at + 1..at + 9].try_into().unwrap());
                let sum = u32::from_le_bytes(data[at + 9..at + 13].try_into().unwrap());
                FrameBody::Morph {
                    base,
                    base_sum: Some(sum),
                    escapes: data[at + 13..].to_vec(),
                }
            }
            BODY_EMPTY => FrameBody::Empty,
            BODY_SCROLLBACK => {
                if data.len() < at + 13 {
                    return Err(Error::from("scrollback frame too short"));
                }
                let base = u64::from_le_bytes(data[at + 1..at + 9].try_into().unwrap());
                let appended = u32::from_le_bytes(data[at + 9..at + 13].try_into().unwrap()) as usize;
                // Reject a count that could not possibly be backed by the
                // remaining bytes (each row is ≥2 bytes of length header)
                // before reserving for it, then parse row by row, treating a
                // length that runs past the body as a discard (never an
                // over-read). RFC 0002 Security Considerations.
                if appended > MAX_SCROLLBACK_ROWS || appended * 2 > data.len() - (at + 13) {
                    return Err(Error::from("scrollback row count exceeds body"));
                }
                let mut rows = Vec::with_capacity(appended);
                let mut p = at + 13;
                for _ in 0..appended {
                    let (Some(&lo), Some(&hi)) = (data.get(p), data.get(p + 1)) else {
                        return Err(Error::from("scrollback row header truncated"));
                    };
                    let len = u16::from_le_bytes([lo, hi]) as usize;
                    p += 2;
                    let end = p + len;
                    let Some(bytes) = data.get(p..end) else {
                        return Err(Error::from("scrollback row truncated"));
                    };
                    rows.push(bytes.to_vec());
                    p = end;
                }
                FrameBody::Scrollback { base, rows }
            }
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

/// Sets the EXTENSION bit when a table will follow the flags byte. `pub` so
/// `posh`'s `ClientMessage` (which stays in `remote::sync`) shares the exact
/// flags-byte framing.
pub fn flags_with_extension(flags: u8, table: &[caps::Cap]) -> u8 {
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
/// empty table at offset 1 — exactly the pre-capability format. `pub` so
/// `posh`'s `ClientMessage` decode shares it.
pub fn decode_flags_and_caps(data: &[u8]) -> Result<(u8, Vec<caps::Cap>, usize)> {
    let Some(&first) = data.first() else {
        return Err(Error::from("message too short"));
    };
    if first & caps::FLAG_EXTENSION == 0 {
        return Ok((first, Vec::new(), 1));
    }
    let (table, used) = caps::decode_table(&data[1..])?;
    Ok((first & !caps::FLAG_EXTENSION, table, 1 + used))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(apply_diff(old, &d).as_deref(), Some(*new), "old={old:?} new={new:?}");
        }
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
        assert_eq!(base_checksum(b"hello"), base_checksum(b"hello"));
        assert_ne!(base_checksum(b"hello"), base_checksum(b"hellp"));
        assert_ne!(base_checksum(b""), base_checksum(b"x"));
        assert_ne!(base_checksum(b"ab"), base_checksum(b"ba"));
    }

    #[test]
    fn server_frame_roundtrip_across_body_kinds() {
        let table = caps::own_table(&[caps::Cap {
            id: caps::CAP_EXIT_STATUS,
            payload: vec![7],
        }]);
        let bodies = [
            FrameBody::Full(b"dump".to_vec()),
            FrameBody::Diff {
                base: 7,
                base_sum: None,
                diff: b"delta".to_vec(),
            },
            FrameBody::Diff {
                base: 7,
                base_sum: Some(0xdead_beef),
                diff: b"delta".to_vec(),
            },
            FrameBody::Morph {
                base: 9,
                base_sum: None,
                escapes: b"\x1b[2;3Hx".to_vec(),
            },
            FrameBody::Morph {
                base: 9,
                base_sum: Some(0x0123_4567),
                escapes: vec![],
            },
            FrameBody::Empty,
            FrameBody::Scrollback {
                base: 4,
                rows: vec![b"first\r\n".to_vec(), b"\x1b[31msecond\x1b[0m\r\n".to_vec()],
            },
            FrameBody::Scrollback { base: 6, rows: vec![] },
        ];
        for body in bodies {
            for caps_case in [vec![], table.clone()] {
                let frame = ServerFrame {
                    flags: FLAG_SHUTDOWN | FLAG_ECHO,
                    caps: caps_case,
                    frame_num: 11,
                    input_ack: 5,
                    echo_ack: 4,
                    body: body.clone(),
                };
                assert_eq!(ServerFrame::decode(&frame.encode()).unwrap(), frame);
            }
        }
        assert!(ServerFrame::decode(b"x").is_err());
    }

    #[test]
    fn scrollback_body_rejects_truncation_and_bogus_count() {
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
        let mut truncated = good.clone();
        truncated.truncate(truncated.len() - 2);
        assert!(ServerFrame::decode(&truncated).is_err());
        let mut huge = good;
        let at = huge.len() - 5 /* "hello" */ - 2 /* row len */ - 4 /* appended */;
        huge[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(ServerFrame::decode(&huge).is_err());
    }
}
