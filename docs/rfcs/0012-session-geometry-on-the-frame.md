---
status: proposed
date: 2026-07-20
---

# Session Geometry on the Frame (`CAP_SESSION_SIZE`)

## Abstract

This document specifies how a posh server conveys the *session's* terminal
dimensions to a client alongside each visible frame. Today a frame carries no
geometry, so a client applying a frame must size its mirror terminal with the
only dimensions it has — its own tty — which is wrong whenever the session is
smaller, the normal condition when several differently-sized clients are
attached. The geometry travels as a capability entry on the frame's capability
table, so a client that does not understand it is unaffected.

## Introduction

A posh client keeps a `Terminal` mirror of the session and paints its real tty
by rendering that mirror; it does not forward server-authored bytes. The mirror
must therefore model the *session's* grid.

The session daemon sizes the pty to the **smallest** attached client
(`min_client_size` / `apply_client_size`, tmux `window-size smallest`). A client
larger than that minimum is thus permanently displaying a session smaller than
its own window. But size information travels only client → server (`Tag::Init`,
`Tag::Resize`); nothing informs the client of the resulting session size. The
`DumpDiff` applier consequently constructs the mirror at the client's
dimensions and replays a server-authored dump into it.

That mismatch has produced two user-visible defects, each placing the cursor
above its own content on the larger client. Both were repaired in the
serializer. This specification removes the cause instead: it gives the client
the one fact it is missing.

Scope: the capability entry, when a server emits it, and how a client uses it.
It does not change any frame body, nor the codecs. ADR 0006 records the
decision and the alternatives weighed.

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Specification

### 1. Capability entry

`CAP_SESSION_SIZE` is a server-to-client capability carried in the `caps` table
of a `ServerFrame` (RFC 0001). Its identifier MUST be allocated in RFC 0001's
capability registry, which is maintained in place; this document does not fix a
numeric value.

Payload — exactly 4 octets:

```
rows  u16 LE   the session terminal's row count
cols  u16 LE   the session terminal's column count
```

A payload whose length is not exactly 4 MUST be treated as absent (§4). `rows`
and `cols` MUST both be non-zero; a zero in either field MUST be treated as
absent.

### 2. Client advertisement

A client that understands this capability SHOULD advertise `CAP_SESSION_SIZE`
with an empty payload in its `Tag::Init` capability table. A server MUST NOT
require the advertisement before emitting the entry — the entry is inert to a
client that ignores it, and requiring negotiation would add a round trip to
every attach for no benefit.

### 3. Server emission

A server MUST include `CAP_SESSION_SIZE` on every frame whose body is
`FrameBody::Full`, because that body causes the client to construct a fresh
mirror and is precisely where the wrong size takes hold.

A server SHOULD include it on every frame carrying visible state
(`Full`, `Diff`, `Morph`), so a client that attaches or resynchronises
mid-stream learns the geometry without waiting for a keyframe.

A server MUST emit a `Full` body on the first visible frame after the session's
dimensions change, and that frame MUST carry the new geometry. (`MorphDelta`
already refuses to emit a `Morph` across a dimension change, so this constrains
only the choice of body, not the codecs.)

The values MUST be the dimensions of the terminal the frame's body was encoded
from — not any client's dimensions, and not the pty's dimensions if those have
been changed but the model has not yet been resized.

### 4. Client behaviour

A client that receives a well-formed `CAP_SESSION_SIZE` entry MUST use its
`rows`/`cols` as the dimensions of the mirror terminal it constructs or resizes
when applying that frame's body, in place of its own tty dimensions.

A client MUST NOT resize its real tty in response to this entry. The entry
describes the session, not the client's window.

A client that receives no entry, or a malformed one, MUST fall back to its
current behaviour — sizing the mirror to its own tty — so that a server which
does not emit the entry keeps working exactly as it does today.

A client whose tty is smaller than the session will hold a mirror larger than
it can display. Clipping the mirror to the tty on render is a presentation
concern outside this specification; a client MUST NOT clip by shrinking the
mirror itself, as that would discard session state the server believes the
client holds and desynchronise the diff base.

### 5. Interaction with the diff base

The geometry describes the frame's own body. A client MUST NOT retroactively
apply a newly-received geometry to state it has already applied; it takes
effect for the body it accompanies and thereafter.

Because a dimension change forces a `Full` (§3), a client never applies an
incremental body across a geometry change, and the mirror's size and the diff
base always agree.

### 6. Example

A 50-row client attached to a session whose smallest client is 24 rows. The
server's mirror is 24x80; the frame carries:

```
caps:  [ { id: CAP_SESSION_SIZE, payload: 18 00 50 00 } ]   // rows=24, cols=80
body:  Full(<dump_vt bytes of the 24x80 session>)
```

The client constructs a **24x80** mirror, applies the body, and renders that
24-row grid into its 50-row tty. Before this specification it constructed a
50x80 mirror and replayed the same bytes into it.

## Security Considerations

The payload is two unsigned 16-bit integers from an already-authenticated peer;
it conveys no user content and discloses nothing beyond the session's window
size, which the peer necessarily influences.

An implementation MUST bound the allocation implied by the values. `rows` and
`cols` are attacker-controlled by an authenticated peer, and a client that
constructs a terminal of `rows * cols` cells from them without a bound could be
driven into an unbounded allocation by a hostile or malfunctioning server — the
same consideration RFC 0002 records for `MAX_SCROLLBACK_ROWS`. A client MUST
reject values above the bound it already applies to its own terminal
dimensions and fall back to §4's absent-entry behaviour.

Rejecting an out-of-range value degrades to today's behaviour rather than
failing the session: the mismatch it would have fixed is a rendering defect,
not a safety property.

## Compatibility

The entry rides the RFC 0001 capability table, which is already an extension
point: an unknown capability id is carried through and ignored, and the
extension bit is a wire-format detail of the encoder. No frame header field
changes and no body changes, so the encoding remains byte-compatible for every
existing body kind.

The four-way skew matrix of RFC 0008 §6 resolves as:

| server | client | outcome |
|---|---|---|
| new | new | client sizes the mirror to the session (§4) |
| new | old | entry ignored; client sizes to its own tty (today's behaviour) |
| old | new | no entry; client falls back to its own tty (§4) |
| old | old | unchanged |

Only the new×new cell changes behaviour, and only for a client whose tty
differs from the session — a same-sized client observes no difference, because
the two sizes it might use are equal.

Because the fallback is exactly the current behaviour, this specification can
ship without a flag day and without a kill switch.

## References

### Normative

* [RFC 2119] Key words for use in RFCs to Indicate Requirement Levels.
* [RFC 0001] posh Target Grammar and Capability Table — the capability table
  this entry is carried in, and the registry its id MUST be allocated from.
* [RFC 0008] posh Unified Session Frame Transport — the frame this entry rides,
  and the version-skew matrix in §6.

### Informative

* [ADR 0006] Size the client model to the session, not to the client's own tty
  — the decision this specification implements, and the alternatives rejected.
* [RFC 0004] Incremental Frame Sync (MorphDelta) — the codecs whose appliers
  consume the geometry.
* [RFC 0002] posh Scrollback Sync Protocol — precedent for bounding an
  attacker-controlled count from an authenticated peer.
* posh#139 — the height-independence invariant this specification demotes from
  an open-ended obligation on every serializer to a single correct sizing.
* posh#140 — the `framereplay` multi-client harness able to verify §4 by
  driving a client at a geometry differing from the server's.
