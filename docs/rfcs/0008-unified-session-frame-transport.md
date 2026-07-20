---
status: proposed
date: 2026-06-30
---

# posh Unified Session Frame Transport

## Abstract

This document specifies how a posh session daemon serves the `posh-proto`
`ServerFrame` screen-sync protocol over its local Unix socket, so that the same
frame stream feeds both a local client (over the reliable socket) and a remote
client (over the AEAD-UDP roaming transport, through a disposable relay). It
defines the socket frame envelope and capability negotiation, the rule that the
reliable transport is the lossless degenerate of the datagram protocol, the
contract for the relay that replaces today's double-modeled `posh-server` +
inner `posh attach`, the re-homing of the RFC 0001 capability registry into
content vs. transport scopes, and the amendments to the RFC 0001 §1 target
grammar and §2 remote-command contract that the unified `attach`/picker
interface requires. Feature-level rationale is in FDR 0011; the architecture
trail in `docs/plans/2026-06-30-unified-session-transport-design.md`.

## Introduction

Before this specification, a session daemon spoke a `Tag::Output` raw-dump IPC
over its Unix socket, while the roaming transport spoke diffed `ServerFrame`s
over UDP; reaching a session remotely composed the two by running `posh attach`
inside a second `posh-server`-owned PTY and modeling the terminal twice (FDR
0001 "Architecture A"). This RFC realizes FDR 0001's "Architecture B": the
daemon becomes the single frame producer, the local client gains a client-side
display model, and `posh-server` is reduced to a transparent frame relay. It
specifies the wire and grammar contracts so that local and remote attach differ
only by transport, and so that mixed-version peers — including durable daemons
that predate an upgrade — degrade rather than break.

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Specification

### 1. Session frame transport over the socket

The session IPC envelope is unchanged: each frame is a 5-byte header (1-byte
`Tag` + 4-byte little-endian payload length) followed by the payload. A new tag
`Frame` (the next unused value, `12`) carries one encoded `posh-proto`
`ServerFrame` (`Full`/`Diff`/`Morph`/`Scrollback`/`Empty`) as its payload.

- When the attaching client has advertised frame support (§1.1), the daemon
  MUST deliver screen output as `Frame` records and MUST NOT send `Tag::Output`
  to that client.
- When the daemon has NOT observed frame support from a client, it MUST behave
  as the baseline daemon: deliver screen output as `Tag::Output` raw-dump
  records (the current behavior). `Tag::Output` is RETAINED for this purpose.
- Input, resize, detach, kill, info, and run/ack retain their existing `Tag`
  records and semantics unchanged.

#### 1.1 Capability negotiation on the socket

The client's `Tag::Init` payload, after the 4-byte resize prefix, MAY carry a
capability table encoded exactly as RFC 0001 §3 defines (`count: u8`, then
`count × (id: u8, len: u8, payload: len bytes)`). A frame-capable daemon MUST
decode the resize from the 4-byte prefix and parse the trailing table from the
remainder. Because a baseline (pre-this-RFC) daemon decodes the resize from the
*entire* Init payload and rejects any payload whose length is not exactly 4, a
client that appends a table MUST immediately re-assert its size with a
`Tag::Resize` frame carrying the same `(rows, cols)`; every daemon version
honors `Tag::Resize`, so the size lands even on a baseline daemon that dropped
the cap-extended Init's size. On a frame-capable daemon the re-assertion merely
re-sets the size the Init already conveyed. The cap table is therefore safe to
send unconditionally.

- A client that includes a `PROTOCOL_VERSION` (id 0) entry thereby advertises
  frame support and the post-table format version it implements.
- The daemon MUST answer with its own table (carried on its first `Frame`
  record, or an Init-ack frame) before or with the first screen output.
- The vocabulary is the SAME registry used on the datagram transport (§4). A
  receiver MUST skip unknown ids by `len`. A receiver that has never seen a
  table from its peer MUST treat the peer as baseline.
- A receiver seeing a `PROTOCOL_VERSION` higher than it implements MUST fall
  back to baseline (`Tag::Output`) interpretation, never guess.

### 2. The reliable transport as the degenerate datagram

The daemon's frame production MUST be transport-agnostic: it produces a `Full`
keyframe, then `Diff`/`Morph` records against the last *acked* base, identically
for both transports. Over a reliable, ordered transport (the Unix socket):

- The daemon MUST treat each delivered `Frame` as immediately acked; the base
  for the next `Diff`/`Morph` is always the last frame sent.
- The daemon MUST NOT apply datagram-only machinery: no fragmentation, no AEAD
  seal/replay header, no retransmission or RTO.
- The `flags`, `frame_num`, and `input_ack` fields retain their meaning, but the
  loss-recovery paths they drive are inert.
- A client on a reliable transport MUST honor a negotiated `BASE_SUM` but SHOULD
  NOT need to request resyncs, since the base cannot diverge.

No separate "local Full-only" mode is defined; reliability is expressed solely
as instant acks and a never-lost base.

### 3. The relay contract

A remote client reaches the daemon through a disposable relay (`posh-server` in
its reduced role). The relay:

- MUST connect to the session's Unix socket as an ordinary frame-consuming
  client, advertising frame support per §1.1.
- MUST relay each daemon `Frame` into a datagram `ServerFrame` — sealing,
  fragmenting, and applying roaming/RTO — WITHOUT re-modeling the terminal (no
  second PTY, no second `posh_term::Terminal`).
- MUST bridge the client's reliable cumulative UDP input stream into socket
  `Tag::Input` writes, and propagate resize/detach as the corresponding `Tag`
  records (the lossy→reliable conversion is the relay's responsibility; the
  daemon never observes lossy input).
- MUST own the transport capabilities `AGENT_FORWARD`/`AGENT_DATA`/`AGENT_ACK`
  (§4): handle those entries itself and forward ALL other capability entries
  transparently between client and daemon.
- MUST NOT require parsing content-capability frame bodies; unknown-id-skip
  applies, and the `ServerFrame` body is opaque to the relay except for the
  `flags` it must honor (e.g. shutdown).

Because the relay holds no terminal model and its only per-session state is
which Unix socket it is connected to, *retargeting* the relay at a different
daemon socket mid-transport is a natural extension: the relay drops its current
connection, opens a new one, and the new daemon's `Full` keyframe (§2)
re-establishes the base — the same reset as any fresh attach. This RFC does not
yet specify the retarget trigger or whether the relay tracks a single target or
a target stack; that is the subject of **FDR 0012** (session layer collapse),
which hangs off this contract. Nothing in this section presumes a single fixed
target for the transport's lifetime.

### 4. Capability re-homing

No new registry ids are allocated; the socket reuses the RFC 0001 §3 registry.
Each id is classified by scope:

| id | Name | Scope | Negotiated | On Unix transport |
|---|---|---|---|---|
| 0 | `PROTOCOL_VERSION` | content (meta) | end-to-end | yes |
| 1 | `EXIT_STATUS` | content (daemon owns the shell) | end-to-end | yes |
| 2 | `TERM_FEATURES` (reserved) | content (daemon model) | client→daemon | yes |
| 3 | `SCROLLBACK` | content (daemon owns the ring) | end-to-end | yes |
| 4 | `MORPH` | content (frame producer) | end-to-end | yes |
| 5 | `BASE_SUM` | content (checksummed bodies) | end-to-end | dormant (no divergence) |
| 6 | `AGENT_FORWARD` | transport (relay) | relay↔client | absent — RETIRED, see below |
| 7 | `AGENT_DATA` | transport (relay) | relay↔client | absent — RETIRED, see below |
| 8 | `AGENT_ACK` | transport (relay) | relay↔client | absent — RETIRED, see below |

- **Content** capabilities MUST be negotiated between the consuming client and
  the producing daemon, end-to-end; the relay forwards their table entries and
  the opaque frame body unchanged.
- **Transport** capabilities are terminated by the relay and MUST NOT appear on
  the Unix transport.
- The daemon MUST NOT broker SSH-agent key material; agent forwarding remains
  the relay's `srv-<pid>.sock` + `agent/sock` symlink mechanism (FDR 0004),
  unchanged by this document. Agent forwarding for sessions whose shell was
  spawned without a forwarding connection is out of scope (FDR 0011
  Limitations; #103).

  **AMENDED by RFC 0011 §7.** The two sentences above are superseded for any
  connection speaking the RFC 0011 envelope. Ids 6/7/8 are RETIRED (permanently
  reserved, never reassigned); agent forwarding rides `agent` channels (RFC 0011
  §5); and the symlink election is REMOVED — `<base>/agent/sock` becomes a bound
  socket owned by the single endpoint that a one-connection-per-client-host mux
  implies, with no takeover or liveness probing (RFC 0011 §7, and §8 for the
  residual two-client-host case). What is NOT amended: the relay still terminates
  agent traffic rather than passing it to the daemon, so this section's security
  boundary — the daemon never brokers key material — stands unchanged.
- Because a frame-consuming client repaints the visible screen in place on
  BOTH transports, it does NOT stream the session's scrolled-off lines into the
  outer terminal's native scrollback. The daemon is therefore the authoritative
  scrollback owner, and a client (local or remote) that wants history MUST sync
  it via `SCROLLBACK` (RFC 0002) and present it itself — the reliable Unix
  transport does NOT grant outer-terminal-native scrollback for free. This is a
  deliberate convergence of local onto the remote model (FDR 0011 Limitations);
  outer-terminal-native scrollback integration on capable terminals, negotiated
  via the reserved `TERM_FEATURES` capability, is a future direction (#104).

### 5. Amendments to RFC 0001

#### 5.1 Target grammar (§1)

The following resolutions are amended. Parsing remains total.

- A host token with an explicit trailing colon and empty session part (RFC 0001
  rule 3, e.g. `box:`, `user@box:`) resolves to a **host scope** rather than
  `Host`.
- A bare `:` resolves to a **local scope** rather than `LocalSession{":"}`.
- A `Host` target with no session part (a bare host such as `box`, `user@box`,
  `box.example`) resolves, under the `attach` command, to a **host scope**
  rather than a plain roaming shell.

A *scope* target, under `attach` (and bare `posh <target>`), MUST open the
TTY-gated session picker scoped accordingly (host scope → that host's sessions;
local scope → local sessions; no target → all). When stdin is NOT a TTY, the
picker MUST NOT launch; the implementation MUST instead error and print the
candidate session list, so non-interactive use is deterministic.

The legacy plain roaming shell (a non-durable, daemon-less shell that dies with
its transport) is reachable only via an explicit `--ephemeral` modifier on
`attach` applied to a host target. All other sessions are durable.

`RemoteSession`, `Local` (`:session`, `:group/session`), and bare
`LocalSession` (a non-`:` word, e.g. `dev`) resolutions are UNCHANGED. The
explicit `posh attach host:name` path remains the non-interactive way to
attach-or-create a named session without a picker.

#### 5.2 Remote command contract (§2)

A conforming client reaching a `RemoteSession` MAY use either:

1. the **frame-relay bootstrap** (this RFC): `posh-server` connects to the
   session socket and relays frames per §3; or
2. the **legacy composition** (RFC 0001 §2): `posh-server new -- posh attach
   SESSION`, which models the terminal twice.

The two MUST interoperate via §1.1 capability negotiation, and an
implementation MUST be able to select (2) as the rollback path (§6). The `POSH
CONNECT` line and the ssh bootstrap are unchanged.

### 6. Version skew and rollback

The socket negotiation MUST handle all four cases without a flag day:

| daemon | client | behavior |
|---|---|---|
| new | new | frames over the socket (§1) |
| new | old | old client sends no table → daemon falls back to `Tag::Output` |
| old | new | old daemon ignores the Init table, sends `Tag::Output` → new client renders it |
| old | old | unchanged baseline |

Because durable daemons may predate an upgrade, the "old daemon / new client"
case is normal, not exceptional, and MUST be supported indefinitely until the
promotion criterion (FDR 0011) retires the fallback. An implementation MUST
provide a single switch that forces the daemon to emit `Tag::Output` only and
the bootstrap to use §5.2(2) — restoring Architecture A in one step. The
reference implementation's daemon-side half of this switch is the
`POSH_SESSION_FRAMES` environment gate, an **opt-out**: default **on**, and
`0`/`false`/`off`/`no` disables it. With it off the daemon never constructs a
per-client frame producer and every client receives raw `Tag::Output` (the
legacy path, byte-for-byte). It is deliberately distinct from `POSH_FRAMESYNC`
(the *remote* MorphDelta codec opt-in), which selects a codec rather than
gating frame emission.

## Security Considerations

- The socket capability table is same-host and bounded by Unix-socket
  permissions, like all session IPC. Parsing MUST be bounds-checked: `count` and
  each `len` are attacker-influenceable by a same-uid peer; a malformed table
  MUST cause the record to be discarded, not a panic or over-read.
- The relay constructs datagrams from opaque daemon frame bodies; the existing
  datagram bounds-checking and AEAD requirements (RFC 0001) are unchanged. The
  relay MUST NOT trust frame lengths beyond the datagram size.
- The daemon MUST NOT gain an SSH-agent key-brokering surface: agent forwarding
  stays a transport (relay) concern. This is a deliberate security boundary of
  the A3 architecture.
- Grammar fallbacks are strictly safer than RFC 0001's: a mistyped host scope
  opens a local picker, and no ssh connection is made until the user selects a
  target — versus RFC 0001's note that a typo could open an ssh connection.

## Conformance Testing

- A `Tag::Frame` encode/decode roundtrip, and the §1.1 Init capability
  negotiation (advertise / fall back / unknown-id skip).
- The four-way socket version-skew matrix of §6.
- A reliable-as-degenerate property test: identical input fed through the Unix
  transport and a lossless UDP transport MUST yield identical client
  `Snapshot`s (the `framereplay` deterministic harness, #75).
- A relay test: agent-capability termination plus content-capability
  pass-through, and the lossy→reliable input bridge.
- A table-driven `Target::parse` test covering the §5.1 amendments (`box:`,
  `user@box:`, `:`, bare `box`), and a test that the picker errors with
  candidates on a non-TTY rather than launching.
- End-to-end (sandbox-safe, loopback UDP): local diff/morph/scrollback/
  exit-status over the socket, and the command palette composited over a local
  session. Cross-host flows (real sshd, agent forwarding, roam) are covered by
  `docs/manual-testing.md`.

## Compatibility

- The datagram wire (`ServerFrame`, the RFC 0001 capability table) is
  unchanged; the socket adopts the same vocabulary, so no new wire format is
  introduced for remote peers.
- `Tag::Output` is retained as the negotiated socket fallback. `Tag::History`
  (pull-based scrollback) is retained as a fallback and is slated for retirement
  once `SCROLLBACK` frames cover it.
- The CLI meaning of `box:` and `:` changes (§5.1) — an intentional,
  documented interface revision, not a wire flag day. `posh attach host:name`
  and the explicit `attach`/`ssh` subcommands remain the stable escape hatches.
- Future socket or datagram protocol changes MUST use the capability table or a
  `PROTOCOL_VERSION` bump, never an ad-hoc format change.

## References

- FDR 0011: Unified durable sessions (`docs/features/`).
- FDR 0012: Session layer collapse (`docs/features/`) — the relay-retarget
  extension anticipated by §3, exploring layer collapse for posh-in-posh.
- RFC 0001: Target grammar and capability table — amended by §5.
- RFC 0011: Multiplexed datagram channels — amends §3 and §4 of this document
  (agent forwarding moves to `agent` channels; capability ids 6/7/8 retired).
- RFC 0004: Incremental frame sync (`MORPH`); RFC 0002: scrollback sync;
  RFC 0006: base-integrity checksums — the content-capability bodies re-homed
  here.
- FDR 0004 and #103: SSH agent forwarding today and its host-global future.
- Design trail: `docs/plans/2026-06-30-unified-session-transport-design.md`.
- #75: the posh-proto extraction (shared codecs, `framereplay`).
- [RFC 2119] Key words for use in RFCs to Indicate Requirement Levels.
