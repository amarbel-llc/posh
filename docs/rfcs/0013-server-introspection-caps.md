---
status: proposed
date: 2026-08-19
---

# Server Introspection Capabilities (`CAP_SERVER_IDENT`, `CAP_SERVER_STATE`, `CAP_SESSION_ACTIVITY`)

## Abstract

This document specifies three capability entries by which a posh client obtains
a server's identity, live state, and a session's activity label over the
existing connection: a static identity block (build version, git sha, pid, start
time) the client requests until held; an on-demand request that makes the server
attach its live transport/agent state to frames under a released capability id;
and an on-demand session **activity label** — the PTY's foreground-process
command, plus the terminal title when the app has set one — that the daemon
computes and exposes both on frames and in enumeration (`posh list`, the picker).
It also specifies the identity exchange on a multiplexed connection's heartbeat
channel, and a host-local status socket on the mux peer. Design record:
`docs/plans/2026-08-19-server-introspection-design.md`.

## Introduction

Nothing in posh reports what build a *running* far end is, and the one
existing state channel — the `CAP_DIAG` (224) transport-state piggyback — is
debug-posture-gated, rides an experimental-band id that RFC 0001 forbids in
released builds (posh#150), and is consumed only by diagnostic dumps. The
posh#161 triage demonstrated the cost: identifying a running server's build
required resolving `/proc/<pid>/exe` and grepping store binaries.

Scope: the three capability entries and their payloads, the request semantics,
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

## 5. `CAP_SESSION_ACTIVITY` (id 15)

A session's **activity label** identifies what the session is running, used to
make a session selectable without its (possibly auto-generated) name — the row
label in `posh list`, the FDR 0011 picker, the FDR 0016 switcher, and `ph`
completion (FDR 0015). The daemon reports two fields: the PTY's
**foreground-process command** (`tcgetpgrp` on the session PTY, resolved to
`comm`/cmdline — always present on a live session, at minimum the shell) and the
frontmost **terminal title** (the last OSC 0/2 string the shell or app set,
tracked by the daemon's `posh_term::Terminal`; may be empty). The rendered label
is **title + process** when a title is set, else the **process** alone; a
consumer MAY instead render the two fields distinctly (title primary, process a
secondary detail). Only the daemon can compute the foreground-process field; both
fields are display data, never trusted for behavior.

### 5.1 Payload

The server entry's payload is:

| offset | size | field |
|---|---|---|
| 0 | 1 | format version, `0x01` |
| 1 | 1 | foreground-process command length `pl` (≤ 128) |
| 2 | pl | foreground-process command, UTF-8 |
| 2+pl | 1 | terminal-title length `tl` (≤ 128; 0 = no title) |
| 3+pl | tl | terminal title, UTF-8 |

A decoder MUST reject an unknown format version, a truncated field, or trailing
bytes, and MUST keep its previously held label (or none) on rejection. An empty
foreground-process field (`pl` = 0) with an empty title renders as `unknown`. The
client entry is empty (a request).

### 5.2 Delivery — two surfaces

The same label reaches a client two ways, depending on whether it is attached to
the session or choosing among sessions:

- **On frames (attached).** A client requesting id 15 — e.g. for the palette
  heading or About view — MUST bound its request window as for `CAP_SERVER_STATE`
  (§2); the daemon attaches the label entry to outgoing frames while the peer's
  most recent message requested it, and refreshes it when the underlying title or
  foreground process changes. This is how an attached client shows the current
  session's label alongside the rtt/echo model already in the palette heading.
- **In enumeration (unattached).** The activity label is also carried in the
  session daemon's list reply and appended to the mux peer's per-session status
  line (§4), so `posh list`, `posh list box:`, the picker, and `ph` completion
  can label a session **without attaching to it**. This is the enabler for
  FDR 0011's auto-id durable sessions: an auto-named session is chosen by its
  activity label, not its id. A pre-RFC-0013 daemon omits the field; readers
  render `unknown`.

## Security Considerations

The identity block reveals build provenance and process facts, and the activity
label reveals what a session is running, to the authenticated peer only — all
three entries ride the AEAD-sealed connection, and the status socket inherits the
`agent/` directory's 0700 owner-only hardening (github #7). No entry is trusted
for behavior: identity, state, and the activity label are display/diagnostic
data, and a malformed payload is dropped without affecting the session.

## Registry Impact

RFC 0001's capability table gains ids 13 (`SERVER_IDENT`), 14 (`SERVER_STATE`),
and 15 (`SESSION_ACTIVITY`), maintained in place per its rules. Id 224's row is
unchanged; posh#150 tracks the experimental band's shipping contradiction,
which id 14 resolves for the state path.
