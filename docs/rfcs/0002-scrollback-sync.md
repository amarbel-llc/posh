---
status: proposed
date: 2026-06-12
---

# posh Scrollback Sync Protocol

## Abstract

This document specifies a server-to-client extension to the posh datagram
protocol that delivers a session's primary-screen scrollback history to a
roaming client, so the attached terminal can scroll back through
off-screen output locally. It defines a new capability-table entry that
negotiates the extension (baseline peers continue to receive
visible-screen-only frames unchanged), a new server frame body that
carries scrollback growth in an append-oriented, bottom-anchored form,
and the client state model that accumulates that history into a local
scrollback ring. The append-oriented framing is required because the
existing whole-screen byte-diff degrades to a near-full retransmit on
every scroll event.

## Introduction

A posh roaming client reconstructs the session's visible screen from
server frames and renders it onto a terminal that posh has pinned to its
own alternate screen for the duration of the connection (FDR 0002). The
session's scrollback — the primary-screen rows that have scrolled above
the visible grid — never reaches the client: the server's frame body
carries a serialization of the screen state, the client rebuilds a
terminal model from it with no scrollback ring, and that model is
discarded and rebuilt on every frame. The user therefore cannot scroll
back through session output that has left the visible region. FDR 0005
describes the resulting feature and its user-facing behavior; this RFC
specifies the wire contract that makes it possible.

The motivating constraint is an efficiency property of the current
format. The server's frame body is a prefix/suffix byte diff
(`make_diff`, RFC 0001 references the `ServerFrame` body kinds `Full` and
`Diff`) computed over `dump_vt`, a **top-anchored** serialization: the
oldest scrollback row is emitted first, the visible grid last, followed
by a cursor/mode trailer. When a single line scrolls off the top of the
primary screen, every subsequent row's byte offset shifts, the shared
prefix with the previous serialization collapses, and the diff degrades
toward a full retransmit. Naively extending the existing format to
include scrollback would therefore make every scroll event — i.e. all
normal terminal output — pay a whole-buffer retransmit that grows with
scrollback depth. This specification defines a body whose cost is bounded
by the *growth* between frames, not by the depth of accumulated history.

This RFC specifies (1) the **`SCROLLBACK` capability** that negotiates
the extension; (2) the **scrollback frame body** wire format; and (3) the
**client accumulation model** — how a conforming client maintains a
local, partial, monotonically-growing view of the server's primary-screen
row space. It does not change the visible-screen sync, the input stream,
the fragmentation layer, or the ssh bootstrap.

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Specification

### 1. Capability negotiation

This extension is negotiated through the RFC 0001 §3 capability table. A
new registry entry is allocated:

| id | Name | Direction | Payload | Meaning |
|---|---|---|---|---|
| 3 | `SCROLLBACK` | both | client: 1 byte; server: 0 bytes | Client entry advertises that the client maintains a scrollback ring and understands the scrollback frame body of section 2; its 1-byte payload is the client's requested ring depth in units of 256 rows (`0` means "server default"). Server entry (empty payload) acknowledges it will emit scrollback bodies. |

- A client that implements this specification MUST include the
  `SCROLLBACK` entry in its capability table to receive scrollback
  bodies.
- A server MUST NOT emit a scrollback frame body (section 2) unless the
  client has advertised `SCROLLBACK`. Against a client that has not
  advertised it, the server MUST emit only the baseline `Full`/`Diff`/
  `Empty` bodies, exactly as today.
- A server that does not implement this specification will never
  acknowledge `SCROLLBACK`; a client MUST treat the absence of the
  server's acknowledgement as "scrollback sync unavailable" and fall back
  to visible-screen-only behavior. There is no flag day (see
  Compatibility).
- Per RFC 0001 §3, capabilities do not persist across messages; both ends
  re-advertise in every message as today. A client MAY stop advertising
  `SCROLLBACK` (e.g. on resize, section 4) and the server MUST then cease
  emitting scrollback bodies until it is re-advertised.

### 2. Scrollback frame body

A new `ServerFrame` body kind is defined. The body-kind discriminator
byte (RFC 0001: `BODY_FULL = 0`, `BODY_DIFF = 1`, `BODY_EMPTY = 2`) gains:

```
BODY_SCROLLBACK = 3
```

The format chosen is **bottom-anchored append** (the alternative
considered is row-indexed structure; see section 2.3 for why this one is
normative). A scrollback body carries the change to the client's
accumulating row space since the frame the client last acknowledged:

```
appended:  u32 LE   -- count of new rows that entered scrollback at the
                       bottom (i.e. scrolled off the top of the visible
                       grid) since the acked frame
rows:      appended × Row
```

where each `Row` is:

```
len:   u16 LE       -- byte length of the row's dump_vt-style cell stream
bytes: len bytes    -- the row rendered as the same per-row escape stream
                       dump_vt emits for a grid row (style runs, wrap flag
                       implied by absence of a trailing newline)
```

- A frame carries exactly one body (RFC 0001), so `BODY_SCROLLBACK` and
  the visible-screen body (`Full`/`Diff`/`Empty`) ride in *separate*
  frames of the one ordered stream. The client MUST apply bodies in
  frame-number order, so a `BODY_SCROLLBACK` whose rows correspond to a
  scroll event is applied coherently relative to the visible `Diff` that
  reflects the same event. A server SHOULD emit the `BODY_SCROLLBACK` for
  a scroll event no later than the visible body that depends on those
  rows having been preserved.
- `BODY_SCROLLBACK` is **self-contained**: it carries the full byte
  rendering of each appended row, and the client appends those rows to
  its ring directly. The client MUST NOT attempt to recover scrolled-off
  rows by inspecting the visible `Diff` body (which conveys the new
  *visible* grid, not the rows that left it). This re-ships row content
  that a reader could in principle reconstruct from the diff stream; see
  section 2.3 for why the redundancy is accepted rather than designed
  out.
- `appended` MUST equal the number of rows that the server's primary
  screen pushed into scrollback in the covered interval. A server that
  has lost track (e.g. its own ring evicted rows faster than the client
  acked) MUST fall back to a `Full` body and reset the client's view
  (section 3).
- Each `Row`'s byte stream MUST reproduce the row's content and
  attributes when fed to a `posh_term` terminal positioned at the start
  of a fresh line, identically to how `dump_vt` emits a primary-screen
  row. The wrap flag MUST be conveyed exactly as `dump_vt` conveys it
  (a soft-wrapped row is one not terminated by the row-separating
  newline), so the client regenerates wrap seams by autowrapping on
  apply — the client MUST NOT compute wrapping independently.
- `appended = 0` is valid and is a no-op body (used to carry a frame
  whose only purpose is the visible-screen diff); a server SHOULD prefer
  `BODY_DIFF`/`BODY_EMPTY` in that case and SHOULD NOT emit an empty
  scrollback body gratuitously.

### 3. Client accumulation model

A conforming client maintains its local terminal model as a **persistent,
monotonically-growing** structure with a real scrollback ring, rather than
reconstructing it per frame:

- On applying a `BODY_SCROLLBACK` body, the client MUST append its
  `appended` rows to the bottom of its scrollback ring, in order. Because
  the body is self-contained (section 2), the client does not derive
  these rows from the visible-screen body; it appends the bytes the
  `BODY_SCROLLBACK` carried. The client's ring MAY have a smaller
  capacity than the server's; rows evicted from the *client* ring by its
  own capacity bound are gone and MUST NOT be re-requested by this
  protocol (no back-fill in this revision; see Compatibility).
- The client's view is explicitly **partial**: on a fresh attach the ring
  begins empty and grows forward. The client MUST NOT assume it holds the
  server's full history and MUST treat "scrolled past the top of what I
  hold" as the end of locally-available scrollback.
- A `Full` body (RFC 0001 `BODY_FULL`) resets the visible screen as
  today. Receipt of a `Full` body MUST NOT clear the client's
  accumulated scrollback ring: `Full` re-establishes the *visible* state
  (e.g. after packet loss or a base-mismatch), while the ring is the
  durable local accumulation. A server that must invalidate the client's
  ring (section 4) MUST do so by ceasing and re-advertising `SCROLLBACK`,
  not by relying on `Full`.
- Local scrollback rendering (entering a scroll view, moving the
  viewport, freezing the live view) is entirely client-side and is NOT
  part of this wire contract; it operates on the accumulated ring. FDR
  0005 specifies that behavior.

### 4. Resize and reflow

A change in terminal width rewraps the server's scrollback (reflow
renumbers the logical row space), so absolute row continuity holds only
between resizes. On a resize:

- The client MUST stop advertising `SCROLLBACK` for the resize message,
  discard any active scroll view, and return to the live view. It MAY
  re-advertise `SCROLLBACK` on a subsequent message to resume
  accumulation at the new width.
- The server, on observing the client cease advertising `SCROLLBACK`,
  MUST cease emitting `BODY_SCROLLBACK` until it is re-advertised, and on
  resumption MUST begin appended-row counting afresh from the resized
  ring (the prior `appended` sequence does not carry across a resize).
- The client's already-accumulated ring from before the resize MAY be
  retained for display but MUST NOT be assumed byte-compatible with
  post-resize appended rows; a conforming client MAY simply drop it and
  re-accumulate. Reflow of retained pre-resize history is explicitly out
  of scope and MUST NOT be attempted client-side.

### 2.3 Why bottom-anchored append is normative (informative)

Two wire shapes were considered:

1. **Bottom-anchored append** (specified above): the server names how
   many rows newly entered scrollback and ships those rows. Cost per
   frame is bounded by inter-frame growth, independent of total
   scrollback depth. It reuses the existing per-row `dump_vt` cell
   encoding and the existing `Full`/`Diff` machinery for the visible
   screen unchanged.
2. **Row-indexed structured body**: every scrollback row carries an
   absolute logical index (`{ base: u64, rows: [...] }`), moving the row
   dimension to a mosh-style logical-framebuffer model. This is more
   robust to reordering and partial delivery but introduces an absolute
   index space that must survive resize/reflow (it cannot — section 4),
   and a larger new wire surface.

Shape 1 is normative because the resize boundary (section 4) already
forces a "renumber and resync" event, which removes the principal
advantage of absolute indices (cross-resize stability) — leaving shape 2
with more wire surface and no retained benefit. Should a future need
arise for back-fill of pre-attach history or out-of-order window
requests, a row-indexed request/response MAY be added as a separate
capability; this RFC does not specify it.

On the self-contained-body redundancy (section 2): a row that scrolls off
the top is, in principle, recoverable from the visible `Diff` stream that
rendered it before it scrolled, so shipping its bytes again in
`BODY_SCROLLBACK` re-sends content the wire already carried once. The
redundancy is accepted deliberately: reconstructing scrolled-off rows
from the diff stream would require the client to track a rolling
pre-scroll grid and reverse the diff, reintroducing exactly the
diff-coupled fragility this design avoids — and the byte cost is bounded
by `appended` (inter-frame growth), which is small in steady state. A
row's full rendering is the simplest unit that lets the client append
without depending on the visible body's content, which is the property
that keeps the two bodies independently orderable.

## Security Considerations

- The scrollback body rides inside the same AEAD-sealed datagram payload
  as all other posh protocol data (RFC 0001 Security Considerations); it
  is authenticated and confidential to the same degree. A forged body
  cannot be injected without the session key.
- `appended` and each row `len` are attacker-controlled by an
  authenticated peer. A receiver MUST bounds-check both: `appended` MUST
  be capped (a single frame's payload is already bounded by the
  fragmentation layer's `MAX_FRAGMENTS`, but the client MUST additionally
  reject an `appended` count or cumulative row total that would exceed
  its configured ring capacity by a sane factor rather than allocate
  unboundedly), and a row `len` extending past the body MUST cause the
  frame to be discarded, not an over-read or panic. This matches the
  existing table- and fragment-parsing requirements of RFC 0001.
- The scrollback content is session output the client already renders to
  its own screen; accumulating it in a client-side ring discloses nothing
  to the client that the live session did not already show it. The ring
  is in-memory for the life of the attach and is not persisted by this
  specification.
- A malicious or buggy server cannot use the scrollback body to write to
  the client's *visible* screen or input stream; the body's only effect
  is appending rows to the client's scrollback ring, which is rendered
  only when the user scrolls.

## Conformance Testing

Conformance tests for this specification live in
`crates/posh/` (cargo suite; the normative home until a cross-
implementation CLI suite exists, per RFC 0001 Conformance Testing) and,
for end-to-end behavior, the pty integration tests in
`crates/posh/tests/`.

### Covered Requirements

| Requirement | Test | Description |
|---|---|---|
| §1, server MUST NOT emit scrollback body unless client advertised `SCROLLBACK` | `remote::sync` / `remote::server` | A frame stream to a non-advertising client contains only `Full`/`Diff`/`Empty` bodies. |
| §2, `BODY_SCROLLBACK` encode/decode roundtrip | `remote::sync` | `appended` count and each row's `len`/bytes survive encode→decode; `appended = 0` roundtrips. |
| §2, row `len` past body MUST be rejected | `remote::sync` | A truncated scrollback body fails to decode rather than over-reading. |
| §3, `Full` body MUST NOT clear the accumulated ring | `remote::client` (`apply_frame`) | After accumulating scrollback, a `Full` visible reset leaves the client ring intact. |
| §3, appended rows land in ring order | `remote::client` | Rows scrolled off the grid plus `appended` rows accumulate in the correct sequence. |
| §4, resize ceases scrollback and resets counting | `remote::client` / `remote::server` | A resize message drops `SCROLLBACK`; re-advertisement restarts appended-row counting. |
| §2, scrollback growth diff is bounded by growth, not depth | `remote::sync` | A scroll event over a deep ring produces a body sized by `appended`, not total scrollback. |

Tests MUST use `bats-emo` binary injection (`require_bin POSH posh`) once
a `zz-tests_bats/` conformance suite exists; until then the cargo suite is
normative, consistent with RFC 0001.

## Compatibility

- **No flag day.** The extension is gated by the `SCROLLBACK` capability
  (section 1). A client that does not advertise it, or a server that does
  not acknowledge it, interoperates exactly as today: visible-screen-only
  `Full`/`Diff`/`Empty` frames. All four version-skew combinations
  (baseline/extended × client/server) degrade to baseline behavior,
  never corruption — the same guarantee RFC 0001 §3 makes for the
  capability table generally.
- **Body discriminator.** `BODY_SCROLLBACK = 3` is allocated in the
  body-kind space alongside RFC 0001's `0`/`1`/`2`. A baseline receiver
  never receives it (the server gates on the capability), and a receiver
  implementing this RFC continues to accept `0`/`1`/`2` unchanged.
- **No back-fill.** This revision delivers scrollback that accrues while
  the client is attached; it does not stream the server's pre-attach
  history. Interactive access to pre-attach lines remains available via
  the `posh history` command. A future revision MAY add a row-indexed
  "request older window" capability (section 2.3) without superseding
  this one.
- **Future datagram-protocol changes** continue to be governed by RFC
  0001 Compatibility: new capability entries or a `PROTOCOL_VERSION`
  bump, never ad-hoc format changes.

## References

- RFC 0001: posh Target Grammar and Datagram Capability Table
  (`docs/rfcs/0001-target-grammar-and-capability-table.md`) — normative;
  this RFC allocates capability id 3 and body kind 3 within its registries
  and inherits its negotiation, bounds-checking, and compatibility rules.
- FDR 0005: Client-side scrollback (`docs/features/`) — the user-facing
  feature this protocol enables, including the local scroll-view behavior
  that is out of scope here.
- FDR 0002: Terminal takeover and restore (`docs/features/`) — why the
  client's outer terminal cannot itself expose the session's scrollback.
- github #43 (session scrollback access); #3/#28 (wheel/alt-screen
  root cause).
- [RFC 2119] Key words for use in RFCs to Indicate Requirement Levels.
- mosh: Winstein & Balakrishnan, "Mosh: An Interactive Remote Shell for
  Mobile Clients" (USENIX ATC 2012) — the logical-framebuffer lineage
  referenced in section 2.3.
