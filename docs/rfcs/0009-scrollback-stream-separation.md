---
status: proposed
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
| 10 | `SCROLLBACK2` | both | client: 9 bytes; server: 1 byte | Client entry advertises v2 scrollback support and carries the client's scrollback acknowledgement (section 3): 1 byte requested ring depth in units of 256 rows (`0` = server default, as RFC 0002) followed by `acked_sb_rows: u64 LE`. Server entry acknowledges v2 with a 1-byte payload of `0x02`. |

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

### 2. v2 scrollback body

A new body-kind discriminator is allocated in the RFC 0001 registry
(`BODY_FULL = 0` … `BODY_MORPH_SUM = 6`):

```
BODY_SCROLLBACK2 = 7
```

```
row_offset: u64 LE  -- absolute index, within the session's monotonically
                       growing scrollback row space, of the first row this
                       body carries. Row 0 is the first row pushed to
                       scrollback after the server acknowledged SCROLLBACK2.
appended:   u32 LE  -- count of rows in this body
rows:       appended × Row
```

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
rows it has accepted since the server acknowledged `SCROLLBACK2` (including
rows its ring has since evicted for capacity; `T` never decreases). On
receiving a v2 body:

- `row_offset + appended <= T`: a retransmission already covered — the
  client MUST discard it (idempotency).
- `row_offset >= T`: the client MUST append the rows to its ring in order
  and set `T = row_offset + appended`. A **forward jump** (`row_offset >
  T`) means the server's ring evicted rows the client never received; the
  skipped rows are permanently lost to this client. The client MUST accept
  the jump (the partial view is first-class, FDR 0005) and MAY render a
  local gap indicator; it MUST NOT stall waiting for the gap to be filled.
- `row_offset < T < row_offset + appended` (partial overlap): a conforming
  server never produces this (section 4); the client MUST discard the body.

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
  `acked_sb_rows`, which is what excludes the partial overlap of
  section 3).
- Scrollback bodies MUST NOT occupy visible frame-sequence slots: a v2
  server's frame producer advances its frame number only for visible
  bodies, and its retransmission window (`outstanding`, RTO) covers only
  visible frames. Scrollback delivery repeats on the server's send pacing
  until covered by `acked_sb_rows` — the same repeat-until-acked loop as
  the input channel's `input_base`.

These invariants eliminate the posh#95/#117 mechanism by construction: no
scrollback acknowledgement can assert visible delivery, so no visible frame
can be leapt, staled, or laundered by scrollback traffic.

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
| §1, server MUST NOT emit v1 bodies once v2 acknowledged | to be added with the implementation | A v2 session's frame stream contains no `BODY_SCROLLBACK` (3). |
| §2, v2 body encode/decode roundtrip + bounds | to be added | `row_offset`/`appended`/rows survive roundtrip; truncated and oversized bodies are rejected. |
| §3, offset-gated append: dup discard, in-order append, forward-jump accept | to be added | `T` advances exactly per §3's three cases. |
| §4, scrollback MUST NOT advance `acked_frame` | to be added | Under interleaved loss, `acked_frame` names only applied visible frames. |
| §4, the #95 leap is impossible | extend `remote::client::wedge_repro_server_loop_with_loss_and_titles` | The loss harness under v2 completes with `reack=0`, no stale visible drops, and the final visible content delivered without watchdog/nudge intervention. |

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
