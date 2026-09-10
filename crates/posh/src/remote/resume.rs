//! `SessionResume`: the aggregate of every offset that must stay CONTINUOUS
//! when a session's transport is torn down and rebuilt underneath a live
//! viewport — a mux-wire reconnect (posh#162) or an FDR 0012 re-home (posh#186).
//!
//! Before this, each such offset was resumed ad-hoc, per stream: the frame
//! ceiling got `resume_base` (posh#162), the input inbox and echo ack happened
//! to persist across a re-home (posh#186) but were RESET on a reconnect (a fresh
//! remote bridge), silently dropping every keystroke typed across the outage.
//! The invariant here is structural: a durable stream's resume offset is a FIELD
//! of `SessionResume`, the reattach path constructs every stream FROM the cursor,
//! and the OPEN wire codec carries the whole cursor. Because there is no blanket
//! `Default`, adding a durable stream (a field) makes every reattach construction
//! site AND the wire codec fail to compile until they carry it — the compiler
//! enforces "you didn't forget the new stream's resume."

/// The offsets a session resumes at when its transport is rebuilt. Extend by
/// adding a field (and handling it in [`encode_open`]/[`decode_open`] and every
/// reattach construction site — the compiler will point at each).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SessionResume {
    /// Frame-numbering ceiling: the fresh producer's frames are rewrapped above
    /// this so the reattach `Full` is not dropped as stale (posh#162; today's
    /// `resume_base`). The client's `applied_num`.
    pub frame: u64,
    /// The reliable input stream offset the daemon has APPLIED. The fresh bridge
    /// seeds its `InputInbox` here so the viewport's re-sent tail (which resumes
    /// at this offset) is accepted, not dropped as a gap — and not re-applied
    /// (this is the daemon-applied offset, not the client's outbox base).
    pub input: u64,
    /// The echo-ack offset (the input offset whose application echo is reflected
    /// in the produced frames — the `always` predictor's confirm boundary). The
    /// fresh bridge seeds its `EchoAck` here so local echo of the durable pending
    /// input stays confirmed-consistent across the reconnect rather than
    /// snapping back to 0.
    pub echo: u64,
}

impl SessionResume {
    /// The initial open: nothing to resume. The ONLY all-zero constructor, so a
    /// zero cursor is always a deliberate "fresh session", never a field someone
    /// forgot to populate.
    pub const INITIAL: SessionResume = SessionResume {
        frame: 0,
        input: 0,
        echo: 0,
    };

    /// Whether this is the initial-open cursor (nothing to resume). Encodes as a
    /// bare target on the wire — byte-identical to the pre-resume format.
    pub fn is_initial(&self) -> bool {
        *self == SessionResume::INITIAL
    }

    /// Advance the cursor from the acknowledgements a relayed frame carries: the
    /// mux daemon calls this for every frame it relays to the foreground viewport
    /// so a later reconnect re-drives the offsets the client actually reached
    /// (RFC 0015 §4). Each offset is `max`-combined and so NEVER decreases — a
    /// reordered or heartbeat `Empty` frame (which repeats the last acks) only
    /// leaves the cursor put, never rewinds it.
    pub fn advance_from_frame(&mut self, frame_num: u64, input_ack: u64, echo_ack: u64) {
        self.frame = self.frame.max(frame_num);
        self.input = self.input.max(input_ack);
        self.echo = self.echo.max(echo_ack);
    }
}

/// Version byte introducing the multi-offset resume block. The pre-versioned
/// posh#162 format (a bare 8-byte frame offset, no version byte) is still
/// decoded for skew with an old client; a new client only ever emits the bare
/// target (initial) or this versioned block (reconnect re-drive).
const RESUME_V1: u8 = 1;
/// Byte length of the v1 resume block after the NUL: version + three u64 LE.
const RESUME_V1_LEN: usize = 1 + 8 * 3;
/// Byte length of the legacy posh#162 resume tail after the NUL: one u64 LE.
const RESUME_LEGACY_LEN: usize = 8;

/// The `SESSION_WIRE_OPEN` body: the RFC 0001 target, and — only when resuming —
/// the [`SessionResume`] cursor. `INITIAL` encodes as the BARE target (byte
/// identical to the original format, so an initial open and an old peer are
/// unaffected). A resume encodes as `target \0 <RESUME_V1> <frame> <input>
/// <echo>` (each u64 little-endian). Session targets never contain NUL, so the
/// split is unambiguous.
pub fn encode_open(target: &[u8], resume: SessionResume) -> Vec<u8> {
    if resume.is_initial() {
        return target.to_vec();
    }
    let mut out = Vec::with_capacity(target.len() + 1 + RESUME_V1_LEN);
    out.extend_from_slice(target);
    out.push(0);
    out.push(RESUME_V1);
    out.extend_from_slice(&resume.frame.to_le_bytes());
    out.extend_from_slice(&resume.input.to_le_bytes());
    out.extend_from_slice(&resume.echo.to_le_bytes());
    out
}

/// Decode an [`encode_open`] body into `(target_bytes, SessionResume)`. Handles:
/// no NUL ⇒ bare target, `INITIAL`; a v1 block ⇒ the full cursor; the legacy
/// posh#162 8-byte frame-only tail ⇒ frame set, input/echo 0 (an old client
/// against a new remote keeps its frame continuity, just not the new
/// input/echo continuity it never had). Any other tail is treated defensively
/// as part of the target with `INITIAL` (the pre-existing tolerance — the target
/// still resolves or the open cleanly fails to the per-invocation fallback).
pub fn decode_open(payload: &[u8]) -> (&[u8], SessionResume) {
    let Some(nul) = payload.iter().position(|b| *b == 0) else {
        return (payload, SessionResume::INITIAL);
    };
    let target = &payload[..nul];
    let tail = &payload[nul + 1..];
    if tail.len() == RESUME_V1_LEN && tail[0] == RESUME_V1 {
        let u = |lo: usize| u64::from_le_bytes(tail[lo..lo + 8].try_into().unwrap());
        return (
            target,
            SessionResume {
                frame: u(1),
                input: u(9),
                echo: u(17),
            },
        );
    }
    if tail.len() == RESUME_LEGACY_LEN {
        return (
            target,
            SessionResume {
                frame: u64::from_le_bytes(tail.try_into().unwrap()),
                input: 0,
                echo: 0,
            },
        );
    }
    (payload, SessionResume::INITIAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_encodes_as_bare_target_and_roundtrips() {
        // The initial open is byte-identical to a bare target (old-peer safe).
        let enc = encode_open(b"host:dev", SessionResume::INITIAL);
        assert_eq!(enc, b"host:dev");
        assert_eq!(decode_open(&enc), (b"host:dev".as_slice(), SessionResume::INITIAL));
    }

    #[test]
    fn full_cursor_roundtrips() {
        let r = SessionResume {
            frame: 512,
            input: 40,
            echo: 37,
        };
        let enc = encode_open(b"work/s-1", r);
        let (target, got) = decode_open(&enc);
        assert_eq!(target, b"work/s-1");
        assert_eq!(got, r);
    }

    #[test]
    fn legacy_frame_only_tail_still_decodes() {
        // An old (posh#162) client emits `target \0 <frame u64>` with no version
        // byte; a new remote must still honor its frame continuity (input/echo 0).
        let mut old = b"host:dev".to_vec();
        old.push(0);
        old.extend_from_slice(&512u64.to_le_bytes());
        assert_eq!(
            decode_open(&old),
            (b"host:dev".as_slice(), SessionResume { frame: 512, input: 0, echo: 0 })
        );
    }

    #[test]
    fn advance_from_frame_is_monotonic_per_offset() {
        // The daemon's producer duty (RFC 0015 §4): peek each relayed frame's
        // acks into the cursor, monotonically. A later frame carrying HIGHER acks
        // advances every offset; a reordered/heartbeat frame with LOWER or equal
        // acks (an Empty repeats the last acks) must never rewind any of them.
        let mut r = SessionResume::INITIAL;
        r.advance_from_frame(300, 40, 37);
        assert_eq!(r, SessionResume { frame: 300, input: 40, echo: 37 });
        // A later frame: input/echo climb, frame climbs.
        r.advance_from_frame(305, 43, 40);
        assert_eq!(r, SessionResume { frame: 305, input: 43, echo: 40 });
        // A stale/reordered frame (lower everywhere) leaves the cursor put.
        r.advance_from_frame(301, 41, 38);
        assert_eq!(r, SessionResume { frame: 305, input: 43, echo: 40 });
        // Mixed: only the offset that actually advanced moves; the others hold.
        r.advance_from_frame(305, 50, 39);
        assert_eq!(r, SessionResume { frame: 305, input: 50, echo: 40 });
    }

    #[test]
    fn unknown_tail_is_tolerated_as_target() {
        // A future/garbled tail must not panic; it falls back to the defensive
        // whole-payload-as-target + INITIAL (open then cleanly fails/falls back).
        let mut weird = b"host:dev".to_vec();
        weird.push(0);
        weird.extend_from_slice(&[9u8; 3]); // neither 8 nor the v1 length
        let (_t, r) = decode_open(&weird);
        assert_eq!(r, SessionResume::INITIAL);
    }
}
