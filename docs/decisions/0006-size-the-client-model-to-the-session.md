---
status: proposed
date: 2026-07-20
---

# Size the client model to the session, not to the client's own tty

## Context and Problem Statement

A posh client maintains a `Terminal` mirror of the session and paints its real
tty by rendering that mirror — it never forwards server-authored bytes. But
nothing on the wire tells the client how big the session is, so the `DumpDiff`
applier sizes the mirror with the only number it has: the client's own tty
dimensions. Under the daemon's smallest-wins sizing every client but the
smallest is therefore modelling the session at the wrong size, permanently.

## Decision Drivers

* The mismatch is the steady state, not an edge case: the session daemon sizes
  the pty to the **smallest** attached client (`min_client_size` /
  `apply_client_size`, `crates/posh/src/session/daemon.rs:751-772`, tmux
  `window-size smallest`), so every larger client renders a smaller session for
  as long as it stays attached.
* It has already produced two user-visible bugs, both landing the cursor above
  its own content on the larger client, both fixed serializer-side after the
  fact (`da5930b`, the scrollback path's absolute CUP; `2f5883c`/`6e997fd`, the
  alt path's unhomed grid).
* The current mitigation is an invariant every serializer must independently
  uphold — "a dump replayed into a taller target must still be correct" — which
  is unenforceable by construction and was violated twice.
* The client already renders as a viewport (`display::new_frame_opt` from a
  `Snapshot` of its own model, in both `remote/client.rs` and
  `session/client.rs`), so the machinery to display a smaller grid in a larger
  tty exists and works.

## Considered Options

1. **Carry the session's geometry on the frame** and have appliers build the
   client mirror at the *session's* size.
2. **Migrate everything to MorphDelta**, whose applier already ignores the
   client's dimensions.
3. **Keep the status quo**: no geometry on the wire, and hold every serializer
   to the height-independence invariant with tests.
4. **Send a `Snapshot`-carrying frame body** — structured cells instead of an
   escape stream — which would carry geometry inherently.

## Decision Outcome

Chosen option: **1, carry the session's geometry on the frame**, because it
removes the client's need to guess a size it is never told, accepting a wire
change and its version-skew story.

The mirror is a model of the *session's* grid; only the tty belongs to the
client. Once the applier can size the mirror correctly, the existing viewport
render handles the rest, and no serializer needs to reason about the target's
height at all.

RFC 0012 specifies the wire change.

### Consequences

* Good: the failure mode becomes unrepresentable rather than merely tested. The
  client stops conflating "the session's grid" with "my window".
* Good: the invariant in posh#139 weakens from "every path that serializes
  terminal state must be height-independent" — an open-ended obligation on all
  future code — to "the applier sizes the mirror once, correctly".
* Good: likely retires the DECCOLM clamp in
  `crates/posh-proto/src/framesync/dumpdiff.rs`, which exists to repair a model
  built at the wrong size.
* Good: makes a genuine viewport policy expressible later (top-anchor, centre,
  letterbox), because the client would know both geometries. Today it knows
  only one and cannot tell them apart.
* Bad: a wire change, with the skew matrix of RFC 0008 §6 to honour. A client
  that does not understand the new field must keep working.
* Bad: a client whose tty is *smaller* than the session now models rows it
  cannot display, so clipping becomes an explicit render decision instead of an
  accident of the model being tty-sized.
* Neutral: does not by itself remove the escape-stream body. `Full` remains
  `dump_vt` bytes; this decision fixes what they are replayed *into*.

### Confirmation

The `framereplay` harness can already express the condition
(`FrameHarness::add_client` at a differing geometry, posh#140). Confirmation is
that `mirrors_content` holds for a mismatched client while the applier is fed
only the frame — no out-of-band knowledge of the session size — and that the
`dump.rs` cursor-mismatch tests still pass unchanged.

## Pros and Cons of the Options

### 1. Carry the session's geometry on the frame

* Good, because it supplies the one fact the client is missing rather than
  working around its absence.
* Good, because it is codec-independent: it fixes `DumpDiff` and any future
  body kind at once.
* Bad, because it changes the wire format and needs negotiation.

### 2. Migrate everything to MorphDelta

* Good, because `MorphDelta`'s applier already discards the client's dimensions
  (`let _ = (rows, cols);`) and applies escapes to the long-lived mirror.
* Bad, and disqualifying: its applier delegates every non-`Morph` body to its
  `DumpDiff` fallback, and its encoder refuses to emit a `Morph` when
  dimensions change — so **every keyframe rebuilds the mirror at the client's
  size**, reintroducing the defect precisely when recovering from loss.
* Bad, because the local socket path cannot negotiate a codec and is
  `DumpDiff` by construction.

### 3. Keep the status quo

* Good, because it costs nothing now and the two known bugs are fixed.
* Bad, because the obligation is open-ended and unenforceable: it binds code
  not yet written, and two competent changes already violated it.
* Bad, because the invariant is subtle enough that `dump_vt`'s own
  documentation stated the opposite ("a fresh terminal of the same size") until
  this session.

### 4. Snapshot-carrying frame body

* Good, because geometry travels inherently and the body stops being a script
  whose meaning depends on the machine executing it.
* Good, because it would also close the mode-state variant of the same defect
  (posh#141: a replay's meaning depended on the target's leftover DECOM,
  margins, autowrap).
* Bad, because it is a much larger change — a new body kind, new encoders, and
  a diffing story to replace prefix/suffix byte diffs — and it is not needed to
  fix the sizing bug.
* Neutral: not mutually exclusive with option 1. Option 1 is the smaller step
  and does not foreclose this.

## More Information

* RFC 0012 — the wire contract for the geometry field.
* posh#139 — the height-independence invariant this decision demotes.
* posh#140 — the multi-client harness that can verify it.
* posh#141 — the mode-state variant of the same script-vs-state defect.
* RFC 0008 §6 — the version-skew matrix any wire change must satisfy.
* Evidence gathered 2026-07-20: `ServerFrame` carries `flags`, `caps`,
  `frame_num`, `input_ack`, `echo_ack`, `body` and no dimensions;
  `posh_proto::display::Snapshot` carries `rows`/`cols` but is encode-side only
  and never serialized; every `Tag::Resize` / `encode_resize` in the tree flows
  client → daemon, and `daemon.rs:1249` lists the daemon → client tags as
  `Output | Ack | Exit | Frame`.
