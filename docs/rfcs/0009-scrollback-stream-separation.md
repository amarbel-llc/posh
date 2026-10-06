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
rows it has accepted in the current epoch (including rows its ring has since
evicted for capacity; `T` never decreases within an epoch and resets to zero
on an epoch change, §1.1). On receiving a v2 body of its current epoch:

- `row_offset + appended <= T`: a retransmission already covered — the
  client MUST discard it (idempotency).
- `row_offset >= T`: the client MUST append the rows to its ring in order
  and set `T = row_offset + appended`. A **forward jump** (`row_offset >
  T`) means the server's ring evicted rows the client never received; the
  skipped rows are permanently lost to this client. The client MUST accept
  the jump (the partial view is first-class, FDR 0005) and MAY render a
  local gap indicator; it MUST NOT stall waiting for the gap to be filled.
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
entry on every message. RFC 0002 §3's remaining accumulation rules (the
ring is partial and monotonic; a `Full` body MUST NOT clear it; local
scroll-view behavior is out of scope) carry over unchanged.

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
  attaching narrower or leaving under smallest-wins sizing — RE-ANCHORS
  every other v2 client in its SAME epoch: the daemon maps its reflowed
  total to that client's send cursor, so the next row scrolled after the
  reflow is numbered right after the last row sent. The client's ring and
  acknowledgement stay valid. Rows the reflow renumbered before they were
  sent are not delivered, and because the numbering continues at the send
  cursor the client sees no offset gap for them — a silent seam, as a v1
  client's history resumes "afresh from the resized ring" (RFC 0002 §4).
  Rows sent before the
  reflow but lost are not resent (no resend reaches below the re-anchor
  point); the next body reaches the client as a forward jump over them
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

**Open issue (posh#243).** A body lost on the wire while a later body is in
flight reaches the client as a forward jump that the resend from the
acknowledgement cannot repair and that the client cannot tell from an
eviction; distinguishing them needs an eviction marker in this protocol.

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
| §5, the shared send cursor | `remote::history::tests::a_continued_cursor_resumes_the_viewports_count`, `a_continued_cursor_never_offers_rows_from_before_its_anchor`, `a_size_change_bumps_the_epoch_and_reanchors`, `reanchor_keeps_the_epoch_and_continues_from_the_send_cursor`, `bump_epoch_reanchors_unconditionally_when_active`, `an_ack_moves_only_forward_and_only_in_its_epoch`, `a_resend_starts_at_the_ack_and_waits_for_the_rto`, `evicted_rows_become_one_forward_jump` | Continue-on-attach at the Init's count; the epoch skips 0 on wrap; a re-anchor keeps the epoch and ack; ack, resend and eviction rules shared with the roaming server. |
| §5, the M2 bridge carries the entry and the ack | `remote::server::tests::bridge_init_carries_the_viewports_scrollback2_entry`, `the_relay_never_carries_scrollback2`, `the_bridge_forwards_a_changed_scrollback2_ack_once`, `the_bridge_keeps_its_init_content_at_the_viewports_latest_entry`, `rehome_bridge_reinits_with_the_latest_scrollback2_entry_and_forwards_afresh`, `a_message_without_scrollback2_forwards_nothing_for_it` | Init carries the viewport's entry (never the relay, ADR 0007); a changed ack is forwarded once as `Tag::ClientCaps`; a re-home re-Inits at the latest entry. |
| §5, daemon gating, row space and epochs | `session::daemon::tests::a_paced_scrollback2_init_opens_a_history_cursor_and_nothing_else_does`, `a_viewport_holding_an_epoch_continues_it_at_its_count`, `a_bare_reinit_keeps_the_history_cursor`, `every_frame_to_a_v2_viewport_carries_the_scrollback2_ack`, `a_v2_viewport_gets_scrollback2_bodies_and_never_v1`, `a_stale_epoch_or_backward_ack_is_ignored`, `a_viewports_own_resize_bumps_its_epoch_and_a_width_change_reanchors_the_others` | v2 only for a paced client that advertised `SCROLLBACK2`; continue-on-attach; the server entry on every frame; bodies annotated with the newest visible number and never v1; own-resize bump, width-change re-anchor. |
| §5 with RFC 0008 §3.2, delivery | `session::daemon::tests::screen_and_history_take_turns_when_both_are_due`, `a_history_body_carries_at_most_sb2_rows_per_body`, `history_waits_for_room_in_the_window`, `a_withheld_ack_is_resent_from_the_ack_only_after_the_floor`, `resends_back_off_while_acks_stay_withheld`, `the_resend_floor_follows_the_measured_ack_latency`, `a_stalled_v2_viewport_gets_one_forward_jump_of_the_evicted_span`, `no_history_body_while_the_overlay_is_up`, `the_poll_wakes_for_pending_history`, `the_exit_flush_sends_only_the_screen` | One body per opportunity; the in-flight window; resend from the ack with backoff; eviction as one forward jump. |
| §3/§5 under a flood | `session::daemon::tests::posh225_v2_flood_ships_every_scrolled_row_exactly_once`, `posh225_v2_flood_backlog_is_one_body_for_every_cadence`, `posh225_v2_flood_at_a_1500_ms_rtt_delivers_its_history`, `posh225_v2_flood_without_acks_resends_one_window_per_backed_off_floor`, `posh225_v2_slow_reader_loses_only_rows_evicted_before_their_turn`, `non_paced_and_v1_paced_streams_are_identical_beside_a_v2_client` | Every row exactly once at 0/50/300 ms RTT and all delivered at 1,500 ms; one unsent body at a time; a slow reader loses only evicted rows, each inside a forward jump; RFC 0002 clients' streams unchanged beside a v2 client. |
| §4, the #95 leap is impossible end-to-end | `remote::client::tests::wedge_repro_server_loop_with_loss_and_titles` | The real `server_loop` under 35% induced loss with v2 negotiated: `reack=0`, `base_sum_mismatch=0`, and the harness asserts v2 engaged (epoch adopted, rows accumulated), so it cannot vacuously pass. |

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
- **Registry allocations.** Capability id `10` and body kind `7` are
  allocated within RFC 0001's registries and follow its rules for unknown
  entries.

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
