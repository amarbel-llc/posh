---
status: proposed
date: 2026-09-10
---

# Session Resume Cursor (`SessionResume`)

## Abstract

When a posh session's transport is torn down and rebuilt underneath a live
viewport — a mux-wire reconnect or an FDR 0012 re-home — every per-stream offset
that must stay continuous (the frame-numbering ceiling, the applied-input
offset, the echo-ack offset) is carried in one `SessionResume` cursor in the
`SESSION_WIRE_OPEN` body. This document specifies that cursor's fields, its
versioned skew-safe wire encoding, and the reattach contract every durable
stream MUST follow: construct from the cursor, never from a fresh zero. It
replaces the earlier per-stream ad-hoc resume (a frame-only tail; input and echo
reset on reconnect, silently dropping keystrokes typed across the outage).

## Introduction

A posh roaming session survives a transport failure. On a mux-wire death the
local mux daemon retains each riding session channel and re-drives its OPEN on a
fresh wire; the remote `posh-server mux` endpoint accepts the re-driven OPEN and
builds a **fresh** `SessionBridge` in front of the surviving session daemon
(`connect_or_create` is idempotent). The viewport never learns a blip occurred:
frames stall, the "Last contact" banner counts up, the reattach repaint clears
it — mosh-parity with the baseline per-invocation UDP path.

The hazard is that a fresh bridge restarts every stream offset at zero while the
viewport's state continues from where it was:

- **Frames.** The surviving daemon's new producer numbers frames low for the new
  lossy client; the reattach `Full` then lands `frame_num < applied_num` and the
  client drops it as stale and wedges. posh#162 fixed this with a *resume base*:
  the re-driven OPEN carried the client's `applied_num` ceiling and the fresh
  bridge rewrapped its frame numbering above it.
- **Input.** The viewport's reliable input outbox continues at a high offset and
  retransmits its unacked tail at `input_base = N`. A fresh `InputInbox` sits at
  `next = 0`; `accept(base, data)` sees `base > next` and drops the tail as a gap
  — **every keystroke typed across the reconnect is silently lost.**
- **Echo.** The echo-ack offset (the input offset whose application is reflected
  in produced frames — the `always` predictor's confirm boundary) resets the
  same way, so local echo of the durable pending input snaps back inconsistent.

posh#162 solved only the frame offset, per-stream, on the reconnect path.
posh#186 made input and echo persist across an FDR 0012 re-home (the same bridge
is retargeted, so its streams are never rebuilt) — but the reconnect path builds
a *new* bridge, and was never covered. Each offset was handled ad-hoc, and a new
durable stream would silently inherit the same bug.

This specification unifies the three offsets into one cursor and makes carrying
it a **structural, compiler-enforced** requirement: a durable stream's resume
offset is a field of `SessionResume`; the reattach path constructs every stream
from the cursor; and because the type has no blanket `Default`, adding a stream
(a field) makes every reattach construction site and the wire codec fail to
compile until they carry it. The scope is the `SESSION_WIRE_OPEN` body and the
reattach construction contract; it does not change the DATA/CLOSE/SWITCH
opcodes, the frame codecs, or the reliable-stream ack semantics themselves.

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Specification

### 1. The cursor

`SessionResume` is the aggregate of every offset a reattach MUST resume:

```rust
pub struct SessionResume {
    pub frame: u64, // frame-numbering ceiling (posh#162 resume base; client applied_num)
    pub input: u64, // reliable-input offset the daemon has APPLIED
    pub echo:  u64, // echo-ack offset (the `always` predictor's confirm boundary)
}
```

- `frame` is the client's frame ceiling. The reattached producer MUST number its
  frames above `frame` so the reattach `Full` is not `< applied_num` and dropped.
- `input` is the offset the session daemon has **applied**, not the client's
  outbox base. Seeding the fresh input stream here is what makes the resumed tail
  land exactly once: the already-applied prefix is skipped (no duplicate), the
  fresh suffix is accepted (no gap).
- `echo` is the echo-ack offset. Seeding the fresh echo stream here keeps the
  predictor's confirm boundary continuous across the outage.

`SessionResume::INITIAL` is the all-zero cursor and the ONLY all-zero
constructor. A zero cursor MUST therefore always mean "fresh session, nothing to
resume", never a field an implementer forgot to populate. The type MUST NOT
derive or implement a blanket `Default`.

Every offset the cursor carries MUST be one that the receiving side can resume a
stream from independently; an offset that cannot be resumed in isolation does not
belong in the cursor.

### 2. The reattach construction contract

An implementation MUST expose, for each durable stream, a resume constructor that
takes exactly that stream's cursor slot (e.g. `InputInbox::resume(next: u64)`,
`EchoAck::resume(acked: u64)`). The reattach path (a fresh bridge built in
response to a resuming OPEN) MUST construct every durable stream through its
resume constructor, seeded from the corresponding `SessionResume` field. The
reattach path MUST NOT construct a durable stream through its fresh/initial
constructor.

Construction of a `SessionResume` on the reattach path MUST be a full struct
literal (every field named). Adding a field is thereby a compile error at every
construction site and in the wire codec until each is updated — this
exhaustiveness IS the enforcement mechanism and MUST be preserved (no `..Default`
rest pattern, no blanket `Default`).

### 3. The `SESSION_WIRE_OPEN` body

The session-channel wire opcodes are unchanged: `SESSION_WIRE_DATA` (0),
`SESSION_WIRE_OPEN` (1), `SESSION_WIRE_CLOSE` (2), `SESSION_WIRE_SWITCH` (3).
This section specifies only the OPEN body (the bytes after the opcode).

The OPEN body is the RFC 0001 session target, optionally followed by a resume
block. A session target MUST NOT contain a NUL byte (RFC 0001), so the first NUL
unambiguously separates the target from the resume block.

**Initial open (`INITIAL`).** The body MUST be the bare target bytes, with no NUL
and no resume block. This is byte-identical to the pre-resume format, so an
initial open is indistinguishable on the wire from a legacy client's open and a
legacy remote decodes it unchanged.

**Resuming open (v1).** The body MUST be:

```
<target bytes> 0x00 0x01 <frame: u64 LE> <input: u64 LE> <echo: u64 LE>
```

where `0x01` is the resume-block version byte (`RESUME_V1`) and the three u64
fields are the cursor in little-endian. The resume block is exactly 25 bytes
after the NUL.

**Decoding.** A decoder MUST classify the body as follows:

| Body shape | Decodes to |
|------------|------------|
| no NUL | `(body, INITIAL)` |
| NUL, tail = 25 bytes, `tail[0] == 0x01` | `(target, {frame, input, echo})` from the LE fields |
| NUL, tail = 8 bytes | `(target, {frame, 0, 0})` — the legacy posh#162 frame-only tail |
| NUL, any other tail | `(body, INITIAL)` — defensive |

A conforming producer MUST emit only the bare-target form (initial) or the v1
form (resume). A producer MUST NOT emit the legacy 8-byte frame-only tail; it is
retained in the decode table solely for skew with an old client. An unrecognized
tail MUST be tolerated (treated as part of the target with `INITIAL`) and MUST
NOT panic; the open then resolves the target normally or fails cleanly to the
per-invocation fallback.

**Examples.**

```
target "host:dev", INITIAL
  → 68 6f 73 74 3a 64 65 76                            ("host:dev")

target "work/s-1", {frame:512, input:40, echo:37}
  → 77 6f 72 6b 2f 73 2d 31                            ("work/s-1")
    00                                                 (NUL)
    01                                                 (RESUME_V1)
    00 02 00 00 00 00 00 00                            (frame = 512)
    28 00 00 00 00 00 00 00                            (input = 40)
    25 00 00 00 00 00 00 00                            (echo  = 37)

legacy old-client open, target "host:dev", frame 512 only
  → 68 6f 73 74 3a 64 65 76  00  00 02 00 00 00 00 00 00
    decodes to {frame:512, input:0, echo:0}
```

### 4. Producer duties (the mux daemon)

The local mux daemon relays frames from the remote endpoint to the foreground
viewport and MUST track a `SessionResume` per riding session channel. As it
relays each frame it MUST advance the cursor monotonically from the frame's
acknowledgements: `frame` from the frame number, `input` from the frame's
input-ack, `echo` from the frame's echo-ack (each `max`-combined, never
decreased). On a wire-death reconnect the daemon MUST re-drive the OPEN carrying
the tracked cursor via the §3 encoding. A channel that has never relayed a frame
MUST re-drive with `INITIAL`.

### 5. Consumer duties (the remote endpoint)

On receiving a `SESSION_WIRE_OPEN` the remote `posh-server mux` endpoint MUST
decode the body per §3 and, when building the fresh `SessionBridge`, seed every
durable stream from the cursor per §2: the producer's frame numbering above
`frame`, the input inbox at `input`, the echo ack at `echo`. With an `INITIAL`
cursor every stream MUST start at zero — identical to a first-ever open.

The relay path (per-invocation UDP, no mux daemon) opens with `INITIAL` and its
FDR 0012 re-home retargets the same bridge (streams are not rebuilt); an
implementation SHOULD thread the same `SessionResume` shape through the re-home
so a future relay-side reconnect inherits the invariant without new per-stream
code.

### 6. Version skew

- **New producer ↔ new consumer:** full cursor; all three offsets resume.
- **New producer ↔ old consumer:** an initial open is byte-identical, so it is
  unaffected. A *resuming* open carries the v1 block, which an old strict decoder
  does not recognize; it MUST fail its decode and fall back (garbage
  target → `SESSION_WIRE_CLOSE` → the client's per-invocation fallback), the same
  skew cost the frame-only resume already paid. Session continuity across a
  reconnect is thus a both-new capability; skew degrades to a fresh reconnect,
  never to a crash.
- **Old producer (legacy frame-only tail) ↔ new consumer:** the 8-byte tail
  decodes to `{frame, 0, 0}`, preserving the frame continuity the old producer
  had and defaulting the input/echo continuity it never emitted.

## Security Considerations

The `SESSION_WIRE_OPEN` body travels inside the established, authenticated,
encrypted datagram channel envelope (RFC 0011); this specification adds no new
transport, endpoint, or trust boundary. The cursor carries only three
monotonic stream offsets — no terminal contents, credentials, or identity — so
it discloses nothing beyond session liveness, which the channel's mere existence
already reveals.

The offsets are attacker-influenceable only by a party already inside the
channel (who can inject arbitrary session bytes regardless). A forged or
corrupted cursor cannot escalate: an over-large `input`/`echo` makes the inbox
skip past legitimate input (a gap the reliable stream surfaces, not a
mis-apply), and an over-large `frame` at worst makes the client treat a
subsequent frame as already-applied (a stale-drop, self-correcting on the next
`Full`). The decoder MUST NOT panic on any tail (§3), bounding a malformed body
to a clean fallback rather than a denial of service. The echo offset seeds only
the prediction confirm boundary; it cannot cause unconfirmed input to be treated
as confirmed in a way that reveals more than the local viewport already renders,
and it is subordinate to the RFC 0007 §5.1 safety gate (echo-off / alt-screen
suppression) which is unchanged by this document.

## Compatibility

This specification supersedes the ad-hoc per-stream resume that preceded it (the
posh#162 frame-only OPEN tail; the posh#186 re-home-only input/echo persistence).
The frame-only tail remains **decodable** (§3, §6) for skew with an old producer,
but MUST NOT be emitted by a conforming producer.

The wire change is backward-compatible for the common case: an initial open is
byte-identical to the previous format, so mixed-version fleets are unaffected
until a session actually reconnects, and even then degrade to a fresh reconnect
rather than failing (§6). No migration step is required; the capability is
acquired by both ends running a build that speaks the v1 resume block.

Future durable streams extend the cursor by adding a field and a corresponding
resume constructor. Doing so MUST bump `RESUME_V1` (defining a longer, higher-
versioned block) so that a decoder can distinguish block versions by length and
version byte, keeping the §3 skew rules intact; the compiler enforces that every
construction site and the codec are updated in lockstep (§2).

The specification is exercised by the Rust unit tests in
`crates/posh/src/remote/resume.rs` (the encode/decode round-trip, the
initial-as-bare-target case, the legacy frame-only tail, and unknown-tail
tolerance) and by the reattach-seeding integration tests in
`crates/posh/src/remote/server.rs` and `crates/posh/src/remote/mux.rs`. The
interface has no CLI surface that reads or writes the OPEN body directly, so no
`bats`/`bats-emo` conformance suite applies.

## References

### Normative

- [RFC 0001] posh Target Grammar and Capability Table — the session target that
  forms the leading bytes of the OPEN body, and the guarantee it contains no NUL.
- [RFC 0011] posh Multiplexed Datagram Channels — the channel envelope carrying
  the session wire, and the session-channel opcodes this body attaches to.
- [RFC 0008] posh Unified Session Frame Transport, §3 / §3.1 — the relay and the
  FDR 0012 retarget's `frame_offset`, the frame-continuity mechanism this cursor
  generalizes.

### Informative

- [FDR 0012] Session layer collapse — the in-place re-home whose retarget
  preserves the same bridge's streams (posh#186), the counterpart to the
  reconnect this document makes durable.
- [FDR 0006] Optimistic local echo — the `always` predictor whose confirm
  boundary is the `echo` offset's on-screen manifestation.
- posh#162 — the frame-only resume base this cursor supersedes.
- posh#186 — the re-home input/echo persistence this cursor unifies with the
  reconnect path.
