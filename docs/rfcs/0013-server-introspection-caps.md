---
status: proposed
date: 2026-08-19
---

# Server Introspection Capabilities (`CAP_SERVER_IDENT`, `CAP_SERVER_STATE`)

## Abstract

This document specifies two capability entries by which a posh client obtains
a server's identity and live state over the existing connection: a static
identity block (build version, git sha, pid, start time) the client requests
until held, and an on-demand request that makes the server attach its live
transport/agent state to frames under a released capability id. It also
specifies the identity exchange on a multiplexed connection's heartbeat
channel, and a host-local status socket on the mux peer. Design record:
`docs/plans/2026-08-19-server-introspection-design.md`.

## Introduction

Nothing in posh reports what build a *running* far end is, and the one
existing state channel — the `CAP_DIAG` (224) transport-state piggyback — is
debug-posture-gated, rides an experimental-band id that RFC 0001 forbids in
released builds (posh#150), and is consumed only by diagnostic dumps. The
posh#161 triage demonstrated the cost: identifying a running server's build
required resolving `/proc/<pid>/exe` and grepping store binaries.

Scope: the two capability entries and their payloads, the request semantics,
the mux heartbeat-channel identity exchange, and the mux peer's local status
socket. Out of scope: any reconnect behavior (posh#162) and any change to
frame bodies or codecs.

## Conventions

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in
RFC 2119. "Requesting" a capability means including an entry with the given
id and an empty payload in a message's capability table.

## 1. `CAP_SERVER_IDENT` (id 13)

### 1.1 Payload

The server entry's payload is:

| offset | size | field |
|---|---|---|
| 0 | 1 | format version, `0x01` |
| 1 | 4 | pid, `u32` LE |
| 5 | 8 | process start time, unix ms, `u64` LE |
| 13 | 1 | version-string length `vl` (≤ 80) |
| 14 | vl | version string, UTF-8 (e.g. `0.3.2`) |
| 14+vl | 1 | git-sha string length `sl` (≤ 80) |
| 15+vl | sl | git sha string, UTF-8 (e.g. `e76fb8f`) |

A decoder MUST reject a payload with an unknown format version, a truncated
field, or trailing bytes, and MUST keep its previously held identity (or
none) on rejection. The client entry is empty (a request).

### 1.2 Semantics

- A client SHOULD request `SERVER_IDENT` on every message while it holds no
  identity for the connection, and MUST re-request after a resync (the
  server behind an address may have been replaced).
- A server MUST attach its identity entry to outgoing frames while the
  peer's most recent message requested it, and SHOULD NOT attach it
  otherwise. This yields reliable delivery with zero steady-state cost once
  the client holds the identity.
- The identity is per answering process: on a relayed connection (RFC 0008
  §3) the relay answers with its own identity, which on a single-binary
  deployment also identifies the daemon's build.

## 2. `CAP_SERVER_STATE` (id 14)

The released request id for the state payload `CAP_DIAG` (224) carries in a
debug posture. The client entry is empty (a request); the server entry's
payload is byte-identical to `CAP_DIAG`'s `ServerDiag` payload (all three
historical lengths remain valid).

- A server MUST answer a state request under the id the client requested
  with: a request via id 14 is answered under id 14; a legacy debug-posture
  request via id 224 is answered under id 224. It MAY answer both if both
  were requested.
- A client requesting on-demand state (e.g. for a palette view) MUST bound
  its request window; requesting indefinitely reintroduces the per-frame
  overhead the split exists to avoid.
- New consumers MUST request via id 14. Id 224 remains valid only for the
  legacy debug posture (posh#150 governs its future).

## 3. Identity on a multiplexed connection (RFC 0011)

On an enveloped connection, the local mux daemon MAY include a
`SERVER_IDENT` request in the `ClientMessage` heartbeats it sends on the
session channel (ordinal 1). A remote that understands this document MUST
answer with a single session-channel instruction carrying an Empty
`ServerFrame` whose capability table holds its identity entry, and SHOULD
answer once per request sighting rather than per heartbeat. A pre-RFC-0013
remote ignores the request harmlessly (it already discards session-channel
payloads on ordinal 1).

This exchange is the first client-observable round trip on an otherwise
idle agent-only connection; liveness machinery (posh#162) MAY build on it.

## 4. The mux peer's local status socket

A mux peer (`posh-server agent`/`mux`) SHOULD bind
`<base>/agent/mux-<client-id>.status.sock` beside its agent socket, with a
`mux-<client-id>.status.pid` liveness record written before the bind (the
same write-before-bind ordering as its agent socket, so the existing GC
rules reap crash leftovers). On each accepted connection it MUST write one
UTF-8 status line — identity plus peer address, heard age, channel counts,
and `agent/sock` ownership — and close; it MUST NOT read. A bind failure
MUST be non-fatal. Readers treat connect-timeout or empty output as
`stale`.

## Security Considerations

The identity block reveals build provenance and process facts to the
authenticated peer only — both entries ride the AEAD-sealed connection, and
the status socket inherits the `agent/` directory's 0700 owner-only
hardening (github #7). No entry is trusted for behavior: identity and state
are display/diagnostic data, and a malformed payload is dropped without
affecting the session.

## Registry Impact

RFC 0001's capability table gains ids 13 (`SERVER_IDENT`) and 14
(`SERVER_STATE`), maintained in place per its rules. Id 224's row is
unchanged; posh#150 tracks the experimental band's shipping contradiction,
which id 14 resolves for the state path.
