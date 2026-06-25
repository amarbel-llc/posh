---
status: proposed
date: 2026-06-25
---

# posh Diff Base-Integrity Checksum

## Abstract

This document specifies a capability-gated extension to the posh datagram
protocol that adds a checksum of a visible `Diff` body's *diff base* to the
server frame, so a roaming client can confirm it holds the same base the server
diffed against before applying a content-blind prefix/suffix diff. It detects a
base divergence between client and server that the existing format cannot,
turning a silent screen corruption or a permanent apply-stall into a clean
resync.

## Introduction

The posh visible-screen `Diff` body (RFC 0001) is a **content-blind**
prefix/suffix byte diff: the client reconstructs the new screen dump by splicing
the diff's middle bytes between the prefix and suffix bytes of its own held base
(`apply_diff`), validating only that `prefix + suffix <= base.len()`. It never
confirms that the client's base is the *same bytes* the server diffed against.

When the two diverge — e.g. a transport-layer desync (RFC 0002 scrollback
interleaving, github #95) leaves the client on a stale or empty visible base —
applying the diff either **fails** (the encoded `prefix + suffix` exceeds the
short base → `apply_diff` returns nothing → the client re-acks forever → a
permanent apply-stall, github #90) or, worse, **succeeds against the wrong
bytes** and silently corrupts the rendered screen with no error signal (github
#94). The content-blind diff has no way to tell a matching base from a
divergent one of the same length.

This RFC adds an optional integrity tag so the client *detects* the divergence
before applying and recovers via a resync, rather than wedging or corrupting.

## Requirements Language

The key words "MUST", "MUST NOT", "SHOULD", "MAY", etc. are to be interpreted as
described in RFC 2119.

## Specification

### 1. Capability

A new RFC 0001 §3 capability-table entry is allocated:

| id | Name | Direction | Payload | Meaning |
|---|---|---|---|---|
| 5 | `BASE_SUM` | client | client: 0 bytes | The client verifies the base checksum of a `BODY_DIFF_SUM` body against its own held dump before applying, and on a mismatch re-acks its current frame and requests a resync (`CLIENT_FLAG_RESYNC`) instead of applying against a divergent base. |

- A client that implements this specification SHOULD advertise `BASE_SUM`
  unconditionally: the check is a few bytes and one pass over the base, and is
  strictly safety-improving. (There is no behaviour-changing apply path as with
  `CAP_MORPH`, so no opt-in gate is warranted.)
- A server MUST emit the checksummed body variant (section 2) only when the peer
  has advertised `BASE_SUM`; against a non-advertising peer it MUST emit the
  plain `BODY_DIFF` exactly as today.
- Per RFC 0001 §3, capabilities do not persist across messages; both ends
  re-advertise every message. There is no flag day (see Compatibility).

### 2. Body kinds

Two new body-kind discriminators are allocated in the RFC 0001 registry
(`BODY_FULL = 0`, `BODY_DIFF = 1`, `BODY_EMPTY = 2`, `BODY_SCROLLBACK = 3`,
`BODY_MORPH = 4`):

```
BODY_DIFF_SUM  = 5
BODY_MORPH_SUM = 6   -- reserved (see below)
```

A `BODY_DIFF_SUM` body is a `BODY_DIFF` whose `base` is immediately followed by
the checksum, then the diff bytes:

```
base:     u64 LE   -- the diff base frame number (== BODY_DIFF semantics)
base_sum: u32 LE   -- base_checksum (section 3) of the server's diff base,
                      i.e. the acked dump the diff was computed against
diff:     prefix/suffix diff bytes (RFC 0001, unchanged)
```

- A server MUST set `base_sum = base_checksum(diff_base)` where `diff_base` is
  the exact byte buffer the prefix/suffix diff was computed against (the server's
  last-acked dump).
- `BODY_MORPH_SUM` is **reserved** with the analogous shape for a future Morph
  base-integrity check. A Morph base is a *snapshot* (display state), not the
  client's held dump bytes, so a byte checksum of the client's dump does not
  apply to it; a server implementing this revision MUST NOT emit
  `BODY_MORPH_SUM`, and the `Morph` body's `base_sum` is always absent.
- `base_sum` and `base` are peer-controlled lengths; a body too short to contain
  them MUST be discarded, never an over-read or panic (RFC 0001 Security
  Considerations).

### 3. `base_checksum`

`base_checksum(bytes)` is **FNV-1a, 32-bit** (offset basis `0x811c9dc5`, prime
`0x01000193`) over the base bytes. It is **not** cryptographic — the datagram is
already AEAD-sealed and authenticated (RFC 0001 Security Considerations); the tag
only needs to catch an *accidental* divergence between the client's held base
and the server's. A 32-bit tag's ~`2^-32` collision probability is adequate for
that and keeps the per-`Diff` overhead at 4 bytes.

### 4. Client behaviour

On receiving a `BODY_DIFF_SUM` whose `base` equals the frame the client has
applied through (`base == applied_num`, the existing base-number gate), the
client MUST compute `base_checksum(applied_data)` over its own held dump and
compare it to the body's `base_sum`:

- **Match:** apply exactly as `BODY_DIFF`.
- **Mismatch:** the client's base content has diverged from the server's. The
  client MUST NOT apply the diff. It re-acks its current frame (idempotent under
  loss, like a base-number mismatch) and sets `CLIENT_FLAG_RESYNC`, so the server
  drops its acked baseline and re-establishes the visible state with a `Full`
  keyframe.

This converts both failure modes — the short-base apply-stall and the
equal-length silent corruption — into a single detectable, recoverable resync.

## Security Considerations

- The checksum is integrity-only and non-cryptographic. Confidentiality and
  authenticity are provided by the AEAD datagram seal (RFC 0001). An
  authenticated peer could forge a `base_sum`, but the only effect is to trigger
  a (cheap) resync — never corruption or disclosure.
- `base_sum` and `base` are bounds-checked exactly like the other RFC 0001 frame
  fields; a truncated `BODY_DIFF_SUM` is discarded.

## Conformance Testing

Conformance tests live in `crates/posh` (cargo suite, the normative home per
RFC 0001):

| Requirement | Test |
|---|---|
| §2 `BODY_DIFF_SUM`/`BODY_MORPH_SUM` encode/decode round-trip | `remote::sync::server_frame_roundtrip` |
| §3 checksum determinism + sensitivity | `remote::sync::base_checksum_is_deterministic_and_sensitive` |
| §4 mismatch re-acks + resyncs, match applies | `remote::client::apply_frame_base_sum_mismatch_resyncs_instead_of_applying` |
| end-to-end over a real `server_loop` with the cap negotiated | `remote::client::wedge_repro_server_loop_with_loss_and_titles` (`#[ignore]`) |

## Compatibility

- **No flag day.** The extension is gated by `BASE_SUM` (section 1). A client
  that does not advertise it, or a server that does not implement it,
  interoperates exactly as today: plain `BODY_DIFF`/`BODY_MORPH`. All four
  version-skew combinations degrade to baseline behaviour, never corruption —
  the guarantee RFC 0001 §3 makes for the capability table generally.
- **Body discriminators.** `BODY_DIFF_SUM = 5` / `BODY_MORPH_SUM = 6` are
  allocated alongside RFC 0001's `0`–`4`. A baseline receiver never receives them
  (the server gates on the capability).

## References

- RFC 0001: posh Target Grammar and Datagram Capability Table — normative; this
  RFC allocates capability id 5 and body kinds 5/6 within its registries and
  inherits its negotiation, bounds-checking, and compatibility rules.
- RFC 0002: Scrollback sync — the visible/scrollback interleaving whose desync
  (github #95) is the divergence this check guards against.
- RFC 0004: Incremental frame sync (`CAP_MORPH`) — the Morph body whose
  snapshot-based base motivates the reserved `BODY_MORPH_SUM`.
- github #94 (content-blind `apply_diff` / silent corruption); #90, #95
  (apply-stall wedge and its root cause).
- [RFC 2119] Key words for use in RFCs to Indicate Requirement Levels.
