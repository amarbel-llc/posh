---
status: experimental
date: 2026-06-15
---

# Incremental Frame Sync (MorphDelta)

## Abstract

This document specifies an optional server→client screen-sync codec for posh's
roaming UDP transport. Instead of the server shipping a complete `dump_vt`
screen serialization that the client re-parses into a fresh terminal every
frame, the server ships a minimal forward escape-delta — the bytes that morph
the client's screen from the last client-acked frame to the current one — which
the client applies to its existing terminal model with no full re-parse. The
codec is selected by capability negotiation and is byte-for-byte inert for a
client that does not request it, so it is a strict, opt-in extension of the
frame protocol defined in RFC 0001.

## Introduction

posh's transport (RFC 0001) carries the server's visible screen state to the
client as a `ServerFrame` whose body is one of `Full` (a complete `dump_vt`
escape stream), `Diff` (a prefix/suffix **byte** diff of that stream against the
client-acked frame's bytes), `Empty`, or `Scrollback` (RFC 0002). For every
visible `Full`/`Diff` frame the client reconstructs the full `dump_vt` bytes and
feeds them to a **fresh** `posh_term::Terminal` via `process()`, rebuilding the
entire screen model from scratch. That work is O(whole screen) per frame
regardless of how little changed: measured at ~75 µs for a 24×80 screen and
~525 µs for a 50×212 screen on a developer machine.

mosh avoids this. Its server computes a minimal escape-delta between the
last-acked framebuffer and the current one (`Complete::diff_from` →
`Display::new_frame`) and the client applies that delta to its existing
framebuffer. The per-frame cost is proportional to what changed, not to the
screen size.

posh already owns the equivalent delta generator: `display::new_frame(last,
next)` emits the minimal escape sequence that morphs one `Snapshot` into another
(it is the same routine that paints the real terminal). This specification
defines a frame body that carries that delta and the rules for producing and
applying it safely. With it, the measured client apply cost drops to a flat
~1.3 µs per frame independent of screen size (it scales with the delta, not the
area). After this change the dominant per-frame client cost becomes the
compose-time `Snapshot::from_term` rebuild, which is out of scope here.

Scope: this document specifies the `Morph` frame body, the `CAP_MORPH`
capability that negotiates it, and the encoder/applier conformance rules. It
does not change the cryptographic framing, the ack/echo-ack semantics, the
scrollback channel (RFC 0002), or any client-side input path.

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Specification

### 1. Terminology

- **Snapshot** — the client-rendered screen state: the visible cell grid, the
  cursor, and the handful of terminal modes the renderer keeps in sync (mouse
  mode/encoding, bracketed paste, alternate scroll, application cursor/keypad,
  reverse video, title, hyperlinks). It does NOT carry alt-screen membership,
  dimensions, scroll region, charsets, origin mode, or tab stops.
- **Morph escapes** — the byte string returned by `display::new_frame(true,
  acked_snapshot, current_snapshot, false)`: the minimal escape sequence that,
  applied to a terminal whose state equals `acked_snapshot`, yields
  `current_snapshot`.
- **Acked baseline** — the frame number the client most recently acknowledged,
  together with the server's rendered Snapshot and off-Snapshot state
  (alt-screen flag, dimensions) at that frame. The server holds it only while it
  still retains that frame's state.
- **DumpDiff codec** — the pre-existing behavior: `Full`/`Diff` bodies over
  `dump_vt` bytes, applied by re-parsing into a fresh terminal. It remains the
  default and the keyframe path.

### 2. Capability negotiation

This protocol extends the RFC 0001 capability table with one entry:

| id | Name | Direction / payload | Meaning |
|----|------|---------------------|---------|
| 4 | `CAP_MORPH` | client→server: 0 bytes | The client understands and requests `Morph` frame bodies. |

- A client MUST advertise `CAP_MORPH` in a `ClientMessage`'s capability table if
  and only if it is configured to use the MorphDelta codec (the reference
  implementation gates this on the `POSH_FRAMESYNC=morph` environment variable).
- As with `CAP_SCROLLBACK` (RFC 0002), the advertisement is per-message, not
  sticky: a server MUST treat the codec as requested only while the most
  recently received client message carried `CAP_MORPH`.
- A server MUST NOT emit a `Morph` body to a client whose most recent message
  did not advertise `CAP_MORPH`. For such a client the server MUST use the
  DumpDiff codec, producing a byte stream identical to a peer that does not
  implement this RFC.

### 3. The `Morph` frame body

This protocol adds one visible-frame body kind to the RFC 0001 set:

```
body_kind: u8 = 0x04   (BODY_MORPH)
base:      u64 LE      — the frame number the morph is computed against
escapes:   bytes       — the morph escapes (remainder of the body)
```

The layout mirrors `BODY_DIFF` (0x01), differing only in the discriminator and
in that `escapes` is a self-contained forward escape stream rather than a
prefix/suffix byte patch. `escapes` MAY be empty (a frame that advanced state
the Snapshot does not reflect); an empty `escapes` MUST decode to a valid
`Morph` body.

The discriminators `0x00` (`Full`), `0x01` (`Diff`), `0x02` (`Empty`), and
`0x03` (`Scrollback`, RFC 0002) are unchanged. `0x04` MUST NOT appear on the
wire toward a client that did not advertise `CAP_MORPH`.

### 4. Encoder rules (server)

When the peer has requested MorphDelta (§2) and the server is producing a
visible frame, it MUST choose the body as follows:

1. If there is **no acked baseline** (the first frame of a session, or the
   server no longer retains the acked frame's state after loss), the server
   MUST emit a `Full` body (a complete `dump_vt`). This is the keyframe.
2. Otherwise, if the acked→current transition is **not morph-expressible**, the
   server MUST emit a `Full` body. A transition is morph-expressible only if
   ALL of the following hold:
   - the alt-screen membership is unchanged (`acked.alt_screen ==
     current.alt_screen`); and
   - the dimensions are unchanged (`acked.rows == current.rows && acked.cols ==
     current.cols`).
   A change in either is NOT morph-expressible because `Snapshot` — and
   therefore `new_frame` — does not carry that state; emitting a morph across
   such a transition would desync the client's terminal model.
3. Otherwise, the server MUST emit a `Morph { base, escapes }` body where `base`
   is the acked frame number and `escapes` is `new_frame(true, acked_snapshot,
   current_snapshot, false)` (the `wheel` argument MUST be `false`; outer-tty
   wheel reporting is a client render concern, not server state).

The set of transitions in rule 2 is a MUST-include lower bound: an
implementation MAY treat additional transitions as non-morph-expressible and
fall back to `Full`, but MUST NOT emit a `Morph` for any transition it cannot
faithfully express.

### 5. Applier rules (client)

On receiving a visible-frame body, a client that advertised `CAP_MORPH`:

- For a `Full` or `Diff` body, MUST apply it exactly as the DumpDiff codec does
  (reconstruct the `dump_vt` bytes, re-parse into the screen model, and record
  the bytes as the new diff base). These are the keyframe/fallback paths.
- For a `Morph { base, escapes }` body:
  - If `base` does not equal the client's currently applied frame number, the
    client MUST NOT apply the morph. It MUST re-acknowledge its current state and
    wait for the server to fall back to a `Full` keyframe — identical to the
    handling of a `Diff` whose base the client does not hold.
  - Otherwise the client MUST apply `escapes` to its **existing** screen model
    via `process(escapes)`. It MUST NOT allocate a fresh terminal and MUST NOT
    re-serialize the model (`dump_vt`) as part of applying a morph.
  - The client's recorded diff base (`applied_data`) is unchanged by a `Morph`.
    A MorphDelta session emits only `Full`/`Morph` visible bodies, so no `Diff`
    body — the sole consumer of that base — is ever received; the base remains
    whatever the last `Full` keyframe set.
  - The frame's `input_ack`/`echo_ack` MUST be processed as for any other body.

### 6. Determinism and drift

Correctness of MorphDelta rests on a determinism invariant: applying a server's
morph escapes to the client model reproduces the server's state exactly.

- Implementations MUST use the same terminal emulator (`posh_term`) on both
  ends, such that `process()` of a given byte stream is deterministic across
  server and client.
- Because `Morph` mutates the existing model rather than rebuilding it, a single
  divergence would persist. The `Full` keyframe is the drift bound: any
  non-morph-expressible transition (§4) re-establishes exact state from a
  complete `dump_vt`, and a `base` mismatch (§5) forces a `Full`. Implementations
  MUST route every non-expressible transition through a `Full` and MUST NOT
  attempt to "patch up" a diverged model with further morphs.

### 7. Interaction with other protocols

- **RFC 0001 (capability table):** `CAP_MORPH` is a new capability id (4) using
  the existing table encoding; no other framing changes.
- **RFC 0002 (scrollback sync):** unchanged. Scrolled-off rows travel on the
  separate `BODY_SCROLLBACK` channel; `Morph` carries only visible-grid state.
  The two are independent and MAY both be negotiated in the same session.

### 8. Examples

A steady-state typing frame (one new line of output, a color change, a cursor
move) on a 50×212 screen: the server emits `Morph { base, escapes }` with
`escapes` on the order of tens of bytes, versus an ~11 KB `Full` dump. The
client applies it with a single `process(escapes)`.

Entering a full-screen application (`vim`): the alt-screen flag flips, so the
transition is not morph-expressible (§4 rule 2) and the server emits a `Full`
keyframe; the client rebuilds via the DumpDiff path. Subsequent in-editor frames
are `Morph` bodies again (alt-screen unchanged frame-to-frame).

A reconnect: the server no longer holds the client's acked baseline (§4 rule 1),
so the first post-reconnect frame is a `Full` keyframe.

## Security Considerations

This protocol changes only the encoding of already-authorized screen state on an
already-encrypted, authenticated channel (RFC 0001); it introduces no new data,
trust boundary, or peer capability. The `escapes` field is server-authored
terminal output that the client already feeds to `posh_term::process()` for
every `Full`/`Diff` frame, so it widens no parsing surface.

A malicious or buggy server could send a `Morph` whose escapes do not match the
claimed `base`, corrupting the client's rendered screen. This is a display-
integrity issue only — it cannot escape the emulator or the client model — and
is no worse than a server sending a corrupt `Full` dump today. The `base`-match
requirement (§5) and the `Full`-keyframe drift bound (§6) limit the blast radius
to frames until the next keyframe. The codec is gated by client opt-in
(`CAP_MORPH`), so a default session's exposure is unchanged.

`CAP_MORPH` carries no payload and leaks no information; its presence reveals
only that the client requested incremental sync.

## Conformance Testing

A reference implementation is the `posh` binary, which implements both the
server-side encoder and the client-side applier. Because the protocol is between
the two roles of a single binary (not a CLI surface or a cross-implementation
wire), conformance is verified by in-crate tests rather than a `bats-emo`
binary-injection suite.

The conformance tests live in `crates/posh/src/remote/framesync/` (the
`framesync::tests` module):

| Requirement | Test | Description |
|-------------|------|-------------|
| §4 rule 3 + §5 (round-trip) | `morph_roundtrip_reproduces_state_over_a_table` | For a table of morph-expressible transitions, server-encode then client-apply reproduces the target Snapshot with zero field divergence (text, SGR, cursor, mode toggles, wide/combining chars, OSC-8 hyperlinks, scroll). |
| §4 rule 2 | `encoder_chooses_full_for_alt_screen_and_resize` | The encoder emits `Full`, not `Morph`, across alt-screen enter/exit and resize. |
| §4 rule 1 | `encoder_chooses_full_with_no_baseline` | With no acked baseline the encoder emits `Full`. |
| §2 (default-off) | `framesync_env_parse_defaults_off` | Only `POSH_FRAMESYNC=morph` advertises `CAP_MORPH`; unset/empty/other do not. |
| §5 (base mismatch) | `undecodable_diff_reacks_and_waits` | An unapplicable body surfaces as re-ack-and-wait rather than corrupting the model. |

A separate `#[ignore]`d timing probe (`perf_probe.rs`, run via `just
debug-perf-compose`) measures the apply-cost win but is not a conformance gate.

## Compatibility

This is a strict, opt-in extension of RFC 0001. A client that does not advertise
`CAP_MORPH` — including any implementation predating this RFC — negotiates
nothing new, and the server's byte stream for such a client is identical to
DumpDiff. No migration is required and there is no version bump.

The codec seam is intentionally general: the encoder is handed the acked
baseline plus the current state, and the applier owns the client model. A future
**CellDelta** codec — which would carry structured Snapshot cell/cursor/mode ops
on the wire and let the client hold a persistent `Snapshot` as its model
(eliminating both the `dump_vt` re-parse and the compose-time
`Snapshot::from_term`) — fits the same seam as a third codec negotiated by its
own capability id, without disturbing DumpDiff or MorphDelta. It is the
documented back-pocket alternate to MorphDelta and is not specified here.

Status is `experimental`: the codec is implemented and default-off. Promotion to
`testing`/`accepted` is gated on live-session validation — sustained use through
line editing, a full-screen application (`vim`), a terminal resize, and a
transport reconnect — with no observed display corruption.

## References

### Normative

- [RFC 2119] Key words for use in RFCs to Indicate Requirement Levels.
- [RFC 0001] posh: Target Grammar and Capability Table (`docs/rfcs/0001-target-grammar-and-capability-table.md`).
- [RFC 0002] posh: Scrollback Sync (`docs/rfcs/0002-scrollback-sync.md`).

### Informative

- [#81] Tracking issue: incremental frame sync (MorphDelta shipped, CellDelta next) — `amarbel-llc/posh#81`.
- mosh `Complete::diff_from` / `Display::new_frame` (`zz-mosh/src/statesync/completeterminal.cc`, `zz-mosh/src/frontend/terminaldisplay.cc`) — the incremental-sync architecture this codec adopts.
