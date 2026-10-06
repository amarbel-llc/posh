---
status: experimental
date: 2026-07-03
---

# posh Scrollback Stream Separation (Scrollback Sync v2)

## Abstract

This document revises the posh scrollback sync protocol (RFC 0002) so that
scrollback delivery no longer participates in the visible-frame sequence.
Scrollback becomes a cumulative, row-offset-addressed append stream with its
own acknowledgement — mirroring the input channel's byte-offset design — and
the frame acknowledgement returns to meaning exactly "the newest visible state
the client holds". This removes by construction the wedge class in which a
scrollback frame's acknowledgement advances the shared frame counter past an
undelivered visible frame, silently staling the client's visible baseline and
leaving both ends quiescent while content sits undelivered.

## Introduction

RFC 0002 delivers scrollback growth in `BODY_SCROLLBACK` frames that occupy
slots in the same `frame_num` sequence as the visible-state bodies
(`Full`/`Diff`/`Morph`), and the client acknowledges a single cumulative
number (`acked_frame = applied_num`) covering both kinds. The two kinds have
incompatible sequencing semantics:

- **Visible bodies are state synchronization**: idempotent, latest-wins;
  skipping ahead over a lost frame is desirable.
- **Scrollback bodies are a reliable append stream**: cumulative and
  order-sensitive; rows must land exactly once, in order.

Sharing one sequence number between them makes the single ack conflate three
distinct client facts: the highest frame consumed, the visible state held
(the diff-base identity), and the scrollback coverage held. Under
interleaving and loss the conflation is exploitable by ordinary packet
timing (posh#95, posh#117): a scrollback frame whose `base` matches the
client's `applied_num` applies and advances `applied_num` past a lost
visible frame; the retransmitted visible frame is then discarded as stale;
the client's ack covers a visible frame it never applied; and the server —
seeing everything acked and its terminal idle — goes quiescent with the
final visible content (typically a shell prompt after a process exit)
permanently undelivered. Live captures of this failure are recorded in
posh#83 and posh#117.

Two transitional defenses shipped ahead of this specification (posh#117
stage C, informative): the client's frozen-model watchdog forces a resync by
default when the visible model freezes while frames keep arriving, and the
server forces one fresh visible frame when, at quiescence, its newest
visible frame was only ever covered by a leaping scrollback ack. These
recover the wedge after the fact. This RFC removes the cause.

This RFC specifies (1) the **`SCROLLBACK2` capability** and its negotiation
against RFC 0002 peers; (2) the **v2 scrollback body**, addressed by
absolute row offset instead of frame-number base; (3) the **scrollback
acknowledgement** carried in the client's capability entry; and (4) the
**sequencing invariants** that visible-frame sync and scrollback sync must
each uphold once separated. The visible-state bodies, input stream,
fragmentation layer, and ssh bootstrap are unchanged.

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Specification

### 1. Capability negotiation

A new RFC 0001 §3 registry entry is allocated:

| id | Name | Direction | Payload | Meaning |
|---|---|---|---|---|
| 10 | `SCROLLBACK2` | both | client: 10 bytes; server: 2 bytes | Client entry advertises v2 scrollback support and carries the client's scrollback acknowledgement (section 3): ring depth u8 (256-row units, `0` = server default, as RFC 0002), epoch u8 (the epoch the client is accumulating in, section 1.1), then `acked_sb_rows: u64 LE`. Server entry acknowledges v2 with `{0x02, epoch: u8}`, naming the current epoch. Both entries ride every message (capabilities do not persist, RFC 0001 §3). |

- A client implementing this specification MUST advertise `SCROLLBACK2` in
  every message (capabilities do not persist across messages, RFC 0001 §3).
  It SHOULD also advertise RFC 0002's `SCROLLBACK` until the server has
  acknowledged `SCROLLBACK2`, so a v1-only server still provides v1
  scrollback; once `SCROLLBACK2` is acknowledged the client MUST drop the
  v1 `SCROLLBACK` entry.
- A server that acknowledges `SCROLLBACK2` MUST emit only v2 scrollback
  bodies (section 2) and MUST NOT emit RFC 0002 `BODY_SCROLLBACK` bodies to
  that client. A server that does not implement v2 ignores the unknown
  entry (RFC 0001 §3) and the session proceeds under RFC 0002 or baseline
  behavior. All version-skew combinations degrade, never corrupt (see
  Compatibility).
- When `SCROLLBACK2` is active, both peers SHOULD negotiate `CAP_BASE_SUM`
  (RFC 0006), and a server that has acknowledged both MUST stamp the base
  checksum on every `Diff` body. The separated design removes the
  frame-number aliasing that produced divergent bases, but content
  divergence (#94) remains detectable only by checksum.

#### 1.1 Epochs

The row space is scoped to an **epoch**, identified by a u8 the server owns.
Epochs exist because a client resize invalidates accumulated rows (reflow;
RFC 0002 §4) while stale v2 bodies from before the reset may still be in
flight — without an epoch tag, a cumulative-offset receiver would append
old-width rows into a freshly-cleared ring.

- The server MUST bump the epoch (wrapping) and re-anchor the row space
  (row 0 = the monotonic scrollback total at the bump) whenever the client's
  reported terminal size changes, and MAY bump it for any other
  reset-requiring event. The server MUST ignore an `acked_sb_rows` whose
  entry names a different epoch.
- The client learns the current epoch from the server's `SCROLLBACK2` ack.
  On a change (including the first ack) it MUST clear its ring and zero its
  cumulative count. On its own resize it MUST treat the epoch as unknown —
  discarding every v2 body — until the server's post-resize ack names the
  fresh epoch.
- A client MUST discard a v2 body whose `epoch` differs from the one it is
  accumulating in.

### 2. v2 scrollback body

A new body-kind discriminator is allocated in the RFC 0001 registry
(`BODY_FULL = 0` … `BODY_MORPH_SUM = 6`):

```
BODY_SCROLLBACK2 = 7
```

```
epoch:      u8      -- the epoch this body's row space belongs to (§1.1); a
                       receiver in a different epoch discards the body
row_offset: u64 LE  -- absolute index, within the epoch's monotonically
                       growing scrollback row space, of the first row this
                       body carries. Row 0 is the first row pushed to
                       scrollback after the epoch opened.
appended:   u32 LE  -- count of rows in this body
rows:       appended × Row
```

A server SHOULD cap the rows per body (the reference implementation uses
256) so a long-disconnect resend chunks into fragmentation-friendly frames;
the cumulative repeat loop carries the remainder as acks advance.

`Row` is unchanged from RFC 0002 §2 (`len: u16 LE` + `bytes`), including the
`dump_vt` rendering contract and the wrap-flag convention. RFC 0002's bounds
requirements on `appended` and `len` apply unchanged.

The carrying `ServerFrame`'s `frame_num` is **not load-bearing** for v2
bodies: a server SHOULD set it to its newest visible frame number (an "as
of" annotation for diagnostics), and a client MUST NOT derive any state —
in particular its acknowledgements (section 4) — from a v2 body's
`frame_num`.

### 3. Client accumulation and the scrollback acknowledgement

The client maintains a cumulative row total `T`: the number of scrollback
rows it has accepted in the current epoch, or given up as not received
(including rows its ring has since evicted for capacity; `T` never decreases
within an epoch and resets to zero on an epoch change, §1.1). On receiving a
v2 body of its current epoch:

- `row_offset + appended <= T`: a retransmission already covered — the
  client MUST discard it (idempotency).
- `row_offset == T`: the client MUST append the rows to its ring in order
  and set `T = row_offset + appended`.
- `row_offset > T` (a **forward jump**): with no `SCROLLBACK2_EXTENT` (§3.1)
  seen in the epoch, the client MUST accept the jump: it treats the skipped
  rows as permanently lost to it, SHOULD label them as one not-received span
  where they fall in its history, appends the body, and sets `T = row_offset
  + appended`. It MUST NOT stall waiting for the gap to be filled (the
  partial view is first-class, FDR 0005). With an extent seen in the epoch,
  §3.1 applies instead. (Amended 2026-10-06, posh#225 Stage 4; posh#243:
  this bullet read every jump as an eviction, and a body lost on a lossy
  link was taken for one.)
- `row_offset < T < row_offset + appended` (partial overlap): the rows
  below `T` are ones the client holds and the rest are new. A sender that
  resends from a lagging acknowledgement (one the client had already
  overtaken when the resend was cut, section 5) MAY produce this. The
  client MUST append only the tail — the rows at and beyond `T` — and set
  `T = row_offset + appended`; it MUST NOT append the overlapping rows
  again. (Amended 2026-10-06, posh#225 Stage 3: this bullet said a
  conforming server never produces a partial overlap and the client
  discards it. Discarding it would stall the stream until a resend happened
  to start exactly at `T`.)

The client reports `T` as `acked_sb_rows` in its `SCROLLBACK2` capability
entry on every message; the acknowledgement stays cumulative, so it never
passes a gap the client is holding open (§3.1). RFC 0002 §3's remaining
accumulation rules (the ring is partial and monotonic; a `Full` body MUST
NOT clear it; local scroll-view behavior is out of scope) carry over
unchanged.

#### 3.1 The extent (posh#243)

Added 2026-10-06 (posh#225 Stage 4). A forward jump (§3) means either that
the server's ring evicted rows, or — on a lossy link — that a body was lost
while a later one was in flight. The **extent** lets a client tell them
apart. A new RFC 0001 §3 registry entry is allocated:

| id | Name | Direction | Payload | Meaning |
|---|---|---|---|---|
| 24 | `SCROLLBACK2_EXTENT` | both | client: ≥ 1 byte; server: ≥ 18 bytes | Client entry: a version byte (`1`) asking the server to report its extent. Server entry: `{version: u8 = 1, epoch: u8, avail_rows: u64 LE, evicted_upto: u64 LE}`. |

- `avail_rows` is the number of rows the epoch's row space holds: the next
  row scrolled is row `avail_rows`. `evicted_upto` is the row below which
  the server will send no body of this epoch again — its ring's eviction
  floor, or the point the row space was re-anchored at (§5), whichever is
  higher. Rows below it that the client lacks are permanently lost to it.
- A server that received the request MUST carry its extent beside every
  server `SCROLLBACK2` entry it sends to that client, naming the same epoch.
  A session daemon MAY repeat the last extent it computed while history is
  paused (for instance under an escape overlay, when it has no session
  terminal to compute one from). A server MUST NOT send the extent to a
  client that did not ask.
- `evicted_upto` MUST be the row below which no body of this epoch will
  start: the server MUST NOT afterwards send a v2 body of the epoch that
  starts below it. Neither
  field decreases within an epoch, so a client MAY keep the maximum of each
  it has seen, and a reordered older extent does no harm.
- Readers MUST ignore bytes past the fields they know (later versions
  append). A payload shorter than its version's fields, or one naming
  version `0`, is malformed and MUST be ignored.
- The request is a separate id, not a longer `SCROLLBACK2` entry, because
  that entry's server half is read exact-length by clients that predate
  this section: a grown entry would stop them adopting epochs.
- On the session socket (§5) the request is Init-persistent: a client sends
  it on its `Tag::Init`; an M2 bridge carries its viewport's request into
  the daemon Init (and a re-homed daemon's re-Init) and does not forward it
  per message; a relay does not carry it (ADR 0007). A roaming server
  latches the request for the connection once any message carries it.

The client half (added 2026-10-06, posh#225 Stage 4) — how a client that
asked uses the extent:

- **The request.** A client that implements this section SHOULD request the
  extent. The reference client sends the request on every message, beside
  `CAP_PACED` and whether or not it is paced, so it is a standing request:
  an M2 bridge forms the daemon Init from the first message it sees, which
  may be the one sent across the client's own resize that omits
  `SCROLLBACK2`.
- **Adoption.** A client MUST adopt only an extent naming the epoch it is
  accumulating in, after applying the same frame's `SCROLLBACK2` entry
  (§1.1). It keeps the maximum of each field within the epoch. On an epoch
  change, and on its own resize (§1.1), it MUST discard the extent and every
  held body (below).
- **Not received.** Rows from `T` up to `evicted_upto` will never be sent. When
  `evicted_upto > T`, the client MUST advance `T` to the lower of
  `evicted_upto` and the first held body's offset, and SHOULD label the
  rows it passed as one not-received span. It MAY do so on the extent alone,
  with no body in hand; the reference client does, so its acknowledgement
  stops naming rows that cannot arrive. An extent can overtake a body still
  in flight (datagram reordering, a bridge retransmit); the rows that body
  carried below the floor then arrive after the span was marked and are
  skipped as already covered (§3). That is a bounded cost of reordering —
  rows the server also evicted — never a stall, and the ring stays
  append-only.
- **The hold rule.** A body of the epoch that, after the step above, still
  starts past `T` lies past a gap the server can still fill. The client
  MUST NOT advance `T`, or its acknowledgement, past the gap on account of
  it. It MAY hold the body; a client that holds bodies MUST bound them, MUST
  keep the longer of two held bodies at the same offset, and MUST discard
  whole (never truncate) a body that would exceed the bound — the resend
  sends it again. The reference bound is 1,024 rows (four bodies of 256,
  §2).
- **Draining.** After each append and each adoption the client repeats,
  until neither applies: append every held body whose offset is at or below
  `T`, under §3's rules (its rows below `T` are skipped); then apply the
  not-received step. So a held gap that a later extent's floor passes
  becomes not received, and the bodies held behind it drain in order.
- **Sender obligation.** The only repair of a held gap is the sender's
  resend from the acknowledgement. A server that sends the extent MUST
  therefore resend from `acked_sb_rows` (§4) under §4's timing rule. Both
  reference senders do.
- **A reset under the same epoch.** A client that clears its count on its
  own resize (§1.1) acknowledges `0` again; if the server never saw a size
  change (the message carrying the intermediate size was lost, and the final
  size equals the one it holds), it does not bump the epoch, and its next
  bodies — at its own send cursor, far past the client's `T = 0` and above
  the floor — would be held forever under the hold rule. A server MUST
  therefore treat an acknowledgement of `0` rows in its current epoch,
  received after that epoch's acknowledgement has advanced, as a client
  reset and open a fresh epoch, exactly as the lost resize would have. A
  fresh adoption always acknowledges `0` first, and a client never
  acknowledges backwards otherwise, so the only false signal is a stale
  `0` from that first round trip, reordered past a later acknowledgement
  (the roaming transport delivers late datagrams inside its reorder
  window; the session socket is ordered). That costs the client the ring
  of an epoch it adopted one reorder window ago — a few seconds of rows at
  most — against a stall that would otherwise last until the server's ring
  evicted past its cursor. Both reference senders do.

Rows the server holds and the client does not — `avail_rows − T`, which
includes a held gap and the bodies behind it — are rows still arriving, not
lost. How a client draws not-received spans and the arriving count, and how
a scrolled view stays on the text being read while they change, is a
rendering concern (FDR 0005, FDR 0021).

### 4. Sequencing invariants (the class-killer)

Once separated, each stream's acknowledgement attests exactly one thing,
and implementations MUST keep them independent:

- `acked_frame` MUST equal the number of the newest **visible** body
  (`Full`/`Diff`/`Morph`) the client has applied. A client MUST NOT advance
  it — and a server MUST NOT interpret it as advanced — on account of any
  scrollback body. Under v2 the client-side `applied_num` is therefore the
  visible-state identity again, and every diff-base comparison
  (`base == applied_num`, RFC 0006 checksums) refers to state the client
  actually holds.
- A server MUST compute visible diff bases only from `acked_frame`, and
  MUST size scrollback retransmission only from `acked_sb_rows`: each v2
  body it emits MUST be anchored at the latest `acked_sb_rows` received
  (or at its own later send cursor / post-eviction floor — never below
  `acked_sb_rows`). Anchoring at the acknowledgement does not exclude the
  partial overlap of section 3: the client may have accepted rows past
  the `acked_sb_rows` the server last received, so a resend from it can
  overlap rows the client holds, which the client resolves by appending
  only the tail.
- Scrollback bodies MUST NOT occupy visible frame-sequence slots: a v2
  server's frame producer advances its frame number only for visible
  bodies, and its retransmission window (`outstanding`, RTO) covers only
  visible frames. Scrollback delivery repeats on the server's send pacing
  until covered by `acked_sb_rows` — the same repeat-until-acked loop as
  the input channel's `input_base`.
- A sender MUST NOT let fresh bodies postpone the resend of unacknowledged
  rows indefinitely: the resend MUST come due a bounded time after the
  acknowledgement last advanced or the last resend, whatever fresh bodies
  went out since. The reference senders time it from the start of the
  current in-flight run, restarted by each resend and each advancing
  acknowledgement.

  *Note (2026-10-06, posh#225 Stage 4).* A resend clocked from the last body
  sent never comes due for a sender whose window never fills (the roaming
  `server_loop`, which sends a fresh body every paced interval through a
  flood), so a lost body stayed lost for the length of the flood. That was
  harmless while a client accepted every jump; under §3.1's hold rule it
  would freeze the acknowledgement at the gap.

These invariants eliminate the posh#95/#117 mechanism by construction: no
scrollback acknowledgement can assert visible delivery, so no visible frame
can be leapt, staled, or laundered by scrollback traffic.

### 5. The session-socket path

Added 2026-10-06 (posh#225 Stage 3; FDR 0021). Between a session daemon and
its client on the session socket (RFC 0008), v2 runs as sections 1–4
specify, with these differences:

- **Advertisement and acknowledgement.** Capabilities on the socket are
  Init-persistent (RFC 0008 §1.1), not per-message. A client advertises
  `SCROLLBACK2` on its `Tag::Init`; the entry names the epoch and count it
  holds. It sends each changed cumulative acknowledgement as a
  `SCROLLBACK2` entry in a `Tag::ClientCaps` record. A daemon reads the
  acknowledgement from either. An M2 bridge carries its viewport's entry
  into the daemon Init, forwards the viewport's entry as `Tag::ClientCaps`
  when its payload changed (the socket is reliable, so an unchanged one is
  not re-sent), and re-Inits a re-homed daemon with the viewport's latest
  entry. A relay does not carry the entry (ADR 0007), so a relayed viewport
  stays on RFC 0002.
- **Gating.** A daemon MUST emit v2 bodies only to a client that advertised
  both `SCROLLBACK2` and `CAP_PACED` (RFC 0008 §3.2), and then carries its
  server `SCROLLBACK2` entry on every frame to that client. Every other
  client — unpaced, or paced without `SCROLLBACK2` — keeps RFC 0002
  scrollback unchanged. When a v2 body is sent is RFC 0008 §3.2's rule.
- **A new attachment continues the client's row space.** The daemon opens
  the row space from the Init entry: epoch `0` (the client holds none) opens
  a fresh epoch whose row 0 is the first row scrolled after the attachment;
  epoch `e` with count `T` continues epoch `e`, the next row scrolled being
  row `T`. So a client keeps its ring across a reconnect or a re-home, and
  the row space continues forward-only across the seam: rows scrolled
  before the attachment (while detached, say) are not delivered. A later
  amendment gives the attachment a starting position from the resume
  cursor (RFC 0015; posh#225 Stage 5).
- **Epoch values.** The server never uses epoch `0`, which a client
  advertises to mean "holds none": a bump skips it on wrap (255 → 1). The
  reference roaming server shares this cursor and the rule.
- **Bump versus re-anchor.** The daemon bumps a client's epoch when that
  client's OWN reported size changes (§1.1: it cleared its ring). A session
  width change — a reflow (RFC 0002 §4), for instance another client
  attaching narrower or leaving under smallest-wins sizing — or a session
  height GROW (the terminal pops ring rows back onto the grid without
  lowering its total, so the ring index of a relative row moves)
  RE-ANCHORS every other v2 client in its SAME epoch: the daemon maps the
  terminal's total at the resize to that client's row COUNT as of the
  resize (every row scrolled before it, sent or not). The client's ring and
  acknowledgement stay valid. Rows scrolled before the resize but not yet
  sent, and rows sent but lost before it, are therefore never delivered
  and are not resent (no resend reaches below the re-anchor point: a
  resend from a lagging ack is an empty body at the anchor); the next body
  reaches the client as ONE forward jump over them
  (§3). This is §1.1's "MAY bump" declined: a bump would clear the
  client's ring whenever another client resized the session.
- **Resend and overlap.** The daemon resends from the latest acknowledgement
  it has received after an implementation-defined floor (FDR 0021). Because
  that acknowledgement crosses a bridge and a link, it can lag rows the
  client already accepted, so a resend MAY partially overlap; the client
  appends only the tail (§3).
- **Annotation.** A v2 body's carrying `frame_num` is the newest visible
  frame number (§2), and the body takes no visible frame-sequence slot
  (§4).
- **The extent.** The request is Init-persistent and the M2 bridge carries
  it into the daemon Init (§3.1). A daemon carries the extent beside the
  server `SCROLLBACK2` entry on every frame to a client that asked, and
  repeats the last one it computed while history is paused (the escape
  overlay). Its `evicted_upto` covers both eviction and re-anchoring, so the
  rows a re-anchor or a stalled reader's eviction skips reach the client as
  not received, while a body lost above the floor is held for the resend
  (§3.1).

**posh#243 — resolved by §3.1 (posh#225 Stage 4, 2026-10-06).** A body lost
on the wire while a later body was in flight reached the client as a
forward jump it could not tell from an eviction. The extent's floor now
tells them apart: a jump at or below it is not received, and one above it
is held until the resend from the acknowledgement fills the gap.

## Security Considerations

- v2 bodies ride the same AEAD-sealed datagram payload as all posh protocol
  data (RFC 0001); RFC 0002's Security Considerations apply unchanged,
  including the mandatory bounds checks on `appended`, row `len`, and the
  cumulative ring size.
- `row_offset` and `acked_sb_rows` are attacker-controlled by an
  authenticated peer. A malicious server can already fabricate scrollback
  content under v1; v2 adds the ability to fast-forward the client's `T`
  (a fabricated forward jump), which discards nothing the client holds and
  is bounded by the same ring-size checks. A malicious client understating
  `acked_sb_rows` induces bounded retransmission (the server resends at
  most its retained ring); a client overstating it merely denies itself
  history. Neither moves the trust boundary.
- Both extent fields (§3.1) come from an authenticated peer. A fabricated
  `evicted_upto` advances `T` over rows labelled not received — the same
  power as a fabricated forward jump. A fabricated `avail_rows` only
  inflates the count of rows shown as arriving. A fabricated jump above the
  floor can make a client hold bodies, which the bound caps (the reference
  client holds at most 1,024 rows); a client whose resend never comes keeps
  acknowledging the gap's start and loses nothing it holds. The extent
  request carries no data beyond its version byte.
- The separation narrows the blast radius of the shared-sequence design:
  scrollback traffic can no longer influence visible-state recovery paths
  (resync, base selection), removing a lever an anomalous peer could pull
  to force full-keyframe storms.

## Conformance Testing

Conformance tests for this specification live in `crates/posh/` and
`crates/posh-proto/` (cargo suite; the normative home until a
cross-implementation CLI suite exists, per RFC 0001 Conformance Testing).
Tests MUST use `bats-emo` binary injection (`require_bin POSH posh`) once a
`zz-tests_bats/` conformance suite exists.

### Covered Requirements

| Requirement | Test | Description |
|---|---|---|
| §2, v2 body encode/decode roundtrip + bounds | `posh-proto frame::tests::scrollback2_body_roundtrips_and_bounds` | `epoch`/`row_offset`/`appended`/rows survive roundtrip; truncated and oversized bodies are rejected. |
| §1, cap payload roundtrips | `posh remote::client::tests::outgoing_caps_advertises_v2_and_drops_v1_once_acked` | The 10-byte client entry carries epoch + `acked_sb_rows`; the v1 entry rides only until v2 is acked. |
| §1.1/§3, epoch adoption + offset-gated append | `remote::client::tests::scrollback2_epoch_adoption_resets_ring_and_count`, `scrollback2_apply_rules_never_touch_applied_num` | Epoch change clears the ring and zeroes `T`; dup discard / in-order append / forward-jump accept / partial-overlap tail append behave per §3; `applied_num` is inert throughout (§4). |
| §3, partial overlap appends only the tail | `remote::client::tests::scrollback2_partial_overlap_appends_only_the_tail` | A body overlapping rows the client holds appends each new row once, in order. |
| §5, the shared send cursor | `remote::history::tests::a_continued_cursor_resumes_the_viewports_count`, `a_continued_cursor_never_offers_rows_from_before_its_anchor`, `a_size_change_bumps_the_epoch_and_reanchors`, `reanchor_keeps_the_epoch_and_jumps_to_the_count_at_the_resize`, `bump_epoch_reanchors_unconditionally_when_active`, `an_ack_moves_only_forward_and_only_in_its_epoch`, `a_resend_starts_at_the_ack_and_waits_for_the_rto`, `a_late_ack_past_a_rewound_resend_is_not_sent_again`, `evicted_rows_become_one_forward_jump`, `a_fresh_body_takes_at_most_the_room_in_the_window`, `a_body_carries_at_most_sb2_rows_per_body` | Continue-on-attach at the Init's count; the epoch skips 0 on wrap; a re-anchor keeps the epoch and ack and jumps to the count at the resize; a late ack past a rewound resend is honoured; the window caps a fresh body; ack, resend and eviction rules shared with the roaming server. |
| §5, the M2 bridge carries the entry and the ack | `remote::server::tests::bridge_init_carries_the_viewports_scrollback2_entry`, `the_relay_never_carries_scrollback2`, `the_bridge_forwards_a_changed_scrollback2_ack_once`, `the_bridge_keeps_its_init_content_at_the_viewports_latest_entry`, `rehome_bridge_reinits_with_the_latest_scrollback2_entry_and_forwards_afresh`, `a_message_without_scrollback2_forwards_nothing_for_it` | Init carries the viewport's entry (never the relay, ADR 0007); a changed ack is forwarded once as `Tag::ClientCaps`; a re-home re-Inits at the latest entry. |
| §5, daemon gating, row space and epochs | `session::daemon::tests::a_paced_scrollback2_init_opens_a_history_cursor_and_nothing_else_does`, `a_viewport_holding_an_epoch_continues_it_at_its_count`, `a_bare_reinit_keeps_the_history_cursor`, `every_frame_to_a_v2_viewport_carries_the_scrollback2_ack`, `a_v2_viewport_gets_scrollback2_bodies_and_never_v1`, `a_stale_epoch_or_backward_ack_is_ignored`, `a_viewports_own_resize_bumps_its_epoch_and_a_width_change_reanchors_the_others`, `a_session_height_grow_reanchors_the_other_viewports` | v2 only for a paced client that advertised `SCROLLBACK2`; continue-on-attach; the server entry on every frame; bodies annotated with the newest visible number and never v1; own-resize bump; width-change and height-grow re-anchor (asserted by row content). |
| §5 with RFC 0008 §3.2, delivery | `session::daemon::tests::the_screen_takes_the_first_tie_then_screen_and_history_alternate`, `a_history_body_carries_at_most_sb2_rows_per_body`, `history_waits_for_room_in_the_window`, `a_withheld_ack_is_resent_from_the_ack_only_after_the_floor`, `resends_back_off_while_acks_stay_withheld`, `the_resend_floor_follows_the_measured_ack_latency`, `a_stalled_v2_viewport_gets_one_forward_jump_of_the_evicted_span`, `no_history_body_while_the_overlay_is_up`, `the_poll_wakes_for_pending_history`, `the_exit_flush_sends_only_the_screen` | One body per opportunity; the in-flight window; resend from the ack with backoff; eviction as one forward jump. |
| §3/§5 under a flood | `session::daemon::tests::posh225_v2_flood_ships_every_scrolled_row_exactly_once`, `posh225_v2_flood_backlog_is_one_body_for_every_cadence`, `posh225_v2_flood_at_a_1500_ms_rtt_delivers_its_history`, `posh225_v2_flood_without_acks_resends_one_window_per_backed_off_floor`, `posh225_v2_slow_reader_loses_only_rows_evicted_before_their_turn`, `non_paced_and_v1_paced_streams_are_identical_beside_a_v2_client` | Every row exactly once at 0/50/300 ms RTT and all delivered at 1,500 ms; one unsent body at a time; a slow reader loses only evicted rows, each inside a forward jump; RFC 0002 clients' streams unchanged beside a v2 client. |
| §3.1, the extent's payloads | `posh-proto caps::tests::scrollback2_extent_roundtrips`, `scrollback2_extent_ignores_trailing_bytes_and_rejects_short_or_version_0`, `the_extent_request_is_a_version_byte`, `the_scrollback2_ack_decoder_is_exact_length` | 18-byte v1 server entry; trailing bytes ignored; short or version-0 payloads ignored; why id 10 cannot grow. |
| §3.1, the floor is the send floor | `remote::history::tests::the_extent_is_none_until_activated`, `the_extent_counts_the_rows_of_the_epoch`, `the_extent_floor_rises_as_the_ring_evicts`, `the_extent_floor_is_the_count_at_a_reanchor`, `a_continued_cursors_floor_is_the_viewports_count`, `the_extent_floor_never_falls_within_an_epoch`, `a_bump_resets_the_extent` | `evicted_upto` is where the next body starts (eviction, re-anchor, continued attach); non-decreasing within an epoch. |
| §3.1/§5, the daemon reports it | `session::daemon::tests::a_v2_viewport_that_asks_gets_the_extent_on_every_frame`, `a_v2_viewport_that_does_not_ask_gets_no_extent`, `a_request_without_scrollback2_gets_no_extent`, `the_extent_floor_is_the_daemons_eviction_floor`, `the_extent_floor_is_the_count_at_a_session_resize`, `the_extent_freezes_while_the_overlay_is_up`, `a_resize_under_the_overlay_keeps_the_extent_in_the_frames_epoch`, `the_first_visible_frame_after_open_history_carries_the_extent`, `posh225_v2_extent_marks_every_flood_jump_as_evicted`, `posh225_v2_extent_counts_nothing_arriving_once_caught_up`, `posh225_v2_extent_without_acks_reports_rows_still_arriving` | Every frame carrying the server `SCROLLBACK2` entry, only to a client that asked; a body starts at its frame's floor; every flood jump is marked. |
| §3.1/§5, the M2 bridge carries the request | `remote::server::tests::bridge_init_carries_the_viewports_extent_request`, `the_relay_never_carries_the_extent_request`, `the_bridge_does_not_forward_the_extent_request_per_message` | Init-only through the bridge; never the relay (ADR 0007). |
| §3/§3.1, not-received spans in the ring | `remote::sync::tests::a_hole_sits_between_the_rows_it_separates`, `marks_at_the_same_tail_position_merge`, `a_hole_is_evicted_with_the_row_after_it`, `clear_drops_every_hole`, `view_total_counts_every_row_and_hole_ever_added` | A gap is labelled once, where it falls, without occupying a ring row; adjacent marks merge; a label leaves with the row after it; an epoch clear drops every label. |
| §3.1, held bodies | `remote::sync::tests::held_bodies_drain_in_row_order_from_the_count`, `a_body_at_the_same_offset_keeps_the_longer`, `held_rows_are_bounded` | Held bodies drain in row order from `T`; the longer body at an offset is kept; a body past the 1,024-row bound is held not at all. |
| §3.1, the client's request | `remote::client::tests::outgoing_caps_requests_the_extent_with_scrollback2` | The request rides every message, including the resize message that omits id 10. |
| §3, a jump with no extent | `remote::client::tests::without_an_extent_a_jump_is_accepted_and_labelled_not_received` | Accepted as before, `T` and the acknowledgement at the body's end; the gap labelled. |
| §3.1, the floor and the hold rule | `remote::client::tests::a_jump_at_or_below_the_floor_is_not_received`, `a_jump_above_the_floor_is_held_and_not_acked_past`, `the_resend_fills_the_gap_and_drains_what_was_held`, `a_floor_that_passes_a_held_gap_makes_it_not_received`, `the_floor_alone_settles_a_gap_with_nothing_held`, `held_bodies_past_the_bound_are_discarded_for_the_resend` | A jump at or below `evicted_upto` is not received; one above it is held and the acknowledgement stays at the gap; the resend fills it and drains the held bodies; a later floor turns a held gap, or a gap with nothing held, into not received; a body past the bound is discarded for the resend. |
| §3.1, adoption | `remote::client::tests::an_extent_of_another_epoch_is_ignored_and_a_new_epoch_clears_it`, `the_extent_only_moves_forward`, `an_own_resize_clears_the_extent_and_what_was_held` | Only the current epoch's extent is adopted; each field keeps its maximum; an epoch change or the client's own resize discards the extent and the held bodies. |
| §3.1/§4, a lost body is repaired, not labelled | `remote::client::tests::a_cursor_and_a_viewport_repair_a_lost_body_without_a_hole` | The real send cursor and viewport: a dropped first body is held past, resent from the acknowledgement, and drained, leaving every row in order and no label; with eviction, exactly one not-received span of the evicted rows. |
| §4, fresh bodies never postpone the resend | `remote::history::tests::a_fresh_body_does_not_postpone_the_resend`, `an_advancing_ack_restarts_the_resend_clock`, `a_resend_restarts_the_resend_clock` | The resend is timed from the start of the in-flight run, restarted only by a resend or an advancing acknowledgement. |
| §3.1, a reset under the same epoch | `remote::history::tests::an_ack_back_to_zero_in_the_same_epoch_is_a_reset_and_bumps_the_epoch`, `session::daemon::tests::an_ack_back_to_zero_is_a_viewport_reset_answered_with_a_fresh_epoch`, `remote::client::tests::a_viewport_reset_under_the_same_epoch_gets_a_fresh_epoch_not_a_held_stall` | A fresh cursor's first `0` is no reset; a `0` after the acknowledgement advanced opens a fresh epoch at once (the daemon's next body and extent carry it); end to end, a viewport that cleared its count under an epoch the sender never bumped holds the stall signature (rows held, acknowledgement at 0) for one frame and then accumulates in the new epoch. |
| §3.1, view rows past many holes | `remote::sync::tests::view_row_locates_a_row_past_many_holes` | The hole-aware row walk agrees with a linear walk across 50 holes on an evicting ring (O(log holes) per row, for the scroll view). |
| §4, the #95 leap is impossible end-to-end; §3.1 under loss | `remote::client::tests::wedge_repro_server_loop_with_loss_and_titles` | The real `server_loop` under 35% induced loss with v2 negotiated: `reack=0`, `base_sum_mismatch=0`, and the harness asserts v2 engaged (epoch adopted, rows accumulated), so it cannot vacuously pass. Since posh#225 Stage 4 it also asserts the extent engaged and that no jump was accepted without one, so every lost body is held and repaired by the resend. |

## Compatibility

- **No flag day.** `SCROLLBACK2` is capability-gated. A v2 client against a
  v1 server falls back to RFC 0002 semantics (it keeps advertising the v1
  entry until v2 is acknowledged); a v1 client against a v2 server gets
  RFC 0002 semantics; baseline peers get visible-only frames. Every skew
  combination degrades to a working protocol, never corruption.
- **v1 remains exposed to the wedge class.** RFC 0002 sessions retain the
  posh#95/#117 failure mode; the stage-C recovery mechanisms (client
  watchdog resync, server anti-quiescence nudge — posh#117) remain in
  force for them and stay harmless under v2.
- **Supersession plan.** When the v2 implementation is validated (the
  extended loss harness above) and this RFC is accepted, RFC 0002's status
  becomes `superseded by RFC-0009`; until then RFC 0002 remains the
  operative scrollback specification and the two documents cross-reference.
- **Registry allocations.** Capability ids `10` and `24` (§3.1, added
  2026-10-06) and body kind `7` are allocated within RFC 0001's registries
  and follow its rules for unknown entries.
- **The extent (§3.1, 2026-10-06).** A client that asks, against a server
  that predates §3.1, sees no extent and keeps §3's jump rule (a jump is
  lost, now labelled). A client that does not ask is sent no extent and its
  stream is unchanged. The acknowledgement keeps its meaning (cumulative
  `T`), and a client holds bodies only after an extent arrives, so only a
  server that sends the extent is relied on to repair a held gap (§3.1,
  §4).

## References

Normative:

- RFC 0001: posh Target Grammar and Datagram Capability Table
  (`docs/rfcs/0001-target-grammar-and-capability-table.md`) — capability and
  body-kind registries, negotiation, bounds-checking, compatibility rules.
- RFC 0002: posh Scrollback Sync Protocol
  (`docs/rfcs/0002-scrollback-sync.md`) — the v1 protocol this document
  revises; the `Row` format, rendering contract, and accumulation model are
  incorporated by reference.
- RFC 0006: posh Diff Base-Integrity Checksum
  (`docs/rfcs/0006-diff-base-integrity.md`) — the checksum this document
  makes mandatory for `Diff` bodies under v2.
- [RFC 2119] Key words for use in RFCs to Indicate Requirement Levels.

Informative:

- posh#95, posh#117, posh#90, posh#83 — the wedge class, its live captures,
  and the transitional (stage C) recovery mechanisms.
- FDR 0005: Client-side scrollback (`docs/features/`) — the user-facing
  feature; the partial-view principle §3 leans on.
- The posh input channel (`ClientMessage.input_base` + pending bytes) — the
  cumulative-offset stream design v2 adopts for scrollback.
