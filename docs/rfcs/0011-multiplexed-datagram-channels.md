---
status: proposed
date: 2026-07-20
---

# posh Multiplexed Datagram Channels

## Abstract

This document specifies a channel envelope for the posh AEAD-UDP transport, so
that one connection between a client host and a remote host carries many
independent streams — every session's frame/input traffic and every forwarded
SSH-agent connection — instead of one connection per session plus an agent
side-channel welded to each. It defines the envelope's placement in the existing
layering, a self-describing `u64` channel identifier that both peers allocate
from without collision, the channel lifecycle, the agent channel payload that
replaces the `CAP_AGENT_*` capability entries, and the fragment reassembly rules
multiplexing requires. It amends RFC 0008 and retires three RFC 0001 capability
ids.

## Introduction

posh opens one AEAD-UDP connection per remote session, and carries SSH agent
forwarding as capability entries riding on that session's frame stream (FDR
0004). Two consequences motivate this specification.

**The agent endpoint has no stable owner.** Because the agent stream is welded
to a session's event loop, N concurrent connections to a host produce N agent
endpoints contesting one well-known path, `<base>/agent/sock`, through a symlink
election. The election is racy: posh#136 reports agent operations intermittently
failing while a healthy, active connection exists. The shipped
relinquish-on-inactive refinement removed the starvation but left a measured
9.9 s window per handoff — two independent 5 s maintenance ticks — during which
the path resolves to an endpoint that fast-fails, then vanishes entirely. FDR
0014 exists to close that window by construction rather than narrow it.

**Every session pays a full connection.** Each attach re-runs the ssh bootstrap
(the dominant connect latency) and adds a NAT binding, a heartbeat, and an
independent roaming state. github #54 scoped a per-destination mux daemon as the
ControlMaster analog; it was closed as a decision without an implementation or a
design document.

Both follow from the same missing primitive: the transport cannot express more
than one stream. This RFC supplies it. The ownership fix falls out — with one
connection per client-host pair, one endpoint owns the agent path because there
is only one endpoint — and the connection-sharing win becomes a migration rather
than a redesign.

Scope. This document specifies the wire contract only: the envelope, the
identifier, the lifecycle, and the agent payload. The mux daemon's process
model, lifetime, and local IPC (github #54) and the feature-level agent
behaviour (FDR 0014) are specified in their own records.

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Specification

### 1. Envelope placement

The transport layering before this document is:

    UDP datagram
      └─ Connection: AEAD seal (24 B) + timestamps (4 B), sequence, roam pinning
        └─ Fragment: 10-byte header (u64 instruction id, u16 final-bit|number)
          └─ ClientMessage | ServerFrame

This RFC inserts the channel envelope **between fragment reassembly and the
message**, so a fragmented instruction belongs wholly to one channel:

    UDP datagram
      └─ Connection
        └─ Fragment
          └─ Channel envelope  ← this document
            └─ ClientMessage | ServerFrame | agent-channel payload

- A sender MUST prepend the envelope to the message and fragment the result; the
  envelope is therefore carried once per instruction, not once per datagram.
- A receiver MUST reassemble the instruction before interpreting the envelope.
- Implementations MUST NOT place channel information in the fragment header.

Rationale (informative): placing the envelope above fragmentation costs 9 bytes
per instruction rather than per datagram, and no participant needs to route
before reassembly — the RFC 0008 §3 relay terminates and re-seals rather than
forwarding raw fragments, so pre-reassembly routability buys nothing here. The
cost is that fragments of one instruction cannot interleave with another
channel's *within that instruction*; §4 bounds the resulting head-of-line
exposure.

### 2. The envelope

    +---------+--------------------+--------------------------+
    | ver: u8 | channel: u64 (LE)  | payload (message bytes)  |
    +---------+--------------------+--------------------------+
    0         1                    9

- `ver` MUST be `0x01` for this specification. A receiver seeing any other value
  MUST discard the instruction and MUST NOT attempt to interpret the payload. It
  SHOULD record the event; it MUST NOT tear down the connection on a single
  occurrence.
- `channel` is the channel identifier defined in §3, little-endian to match the
  existing message field encoding.
- `payload` is the encoded message, whose interpretation is determined by the
  channel's kind (§3.2).

The envelope is symmetric: both peers send and receive it on every instruction
once negotiated (§6).

### 3. Channel identifiers

A channel identifier is a `u64` partitioned so that it is **self-describing**: a
receiver learns the initiator and the kind from the identifier alone, without
consulting connection state. This is required because a channel's OPEN may be
lost or reordered on the datagram transport (§3.3).

    bit  0      : initiator  — 0 = client-initiated, 1 = server-initiated
    bits 1..7   : kind       — 7-bit channel kind (§3.2)
    bits 8..63  : ordinal    — 56-bit sequence within (initiator, kind)

#### 3.1 Allocation

- Each peer MUST allocate identifiers only from the space whose initiator bit
  matches its own role, and MUST NOT allocate from the peer's space.
- Ordinals MUST start at `1` and increase monotonically within each
  `(initiator, kind)` pair. Ordinal `0` is RESERVED.
- Identifier `0` (client, kind `0`, ordinal `0`) is RESERVED for
  connection-level control and MUST NOT carry channel data. This document
  defines no connection-level control messages; the identifier is reserved so a
  future revision may add them without a flag day.
- A peer MUST NOT reuse an ordinal within the lifetime of a connection, even
  after the channel closes. 2^56 ordinals per kind make exhaustion unreachable
  in practice; an implementation that somehow exhausts a space MUST fail the
  connection rather than wrap.

Rationale (informative): a single partitioned space is preferred over SSH's
paired sender/recipient channel numbers. Both solve the same problem — two peers
allocating without collision — but posh genuinely needs bidirectional initiation
(the client opens session channels; the *server* opens agent channels, since a
forwarded agent connection is accepted on the remote host and announced toward
the client), and the partitioned space achieves that without a per-channel
translation table or an open/confirm round trip. This is the QUIC
stream-identifier approach.

#### 3.2 Channel kinds

| Kind | Name | Payload | Initiator |
|---|---|---|---|
| 0 | `session` | `ClientMessage` / `ServerFrame` | client |
| 1 | `agent` | agent-channel payload (§5) | server |
| 2..127 | — | RESERVED | — |

- A receiver encountering a RESERVED kind MUST discard the instruction and
  SHOULD respond with a CLOSE (§3.3) for that identifier. It MUST NOT treat an
  unknown kind as a connection error.
- A peer MUST NOT open a channel of a kind whose defined initiator is not its
  own role.

#### 3.3 Lifecycle

Channels are opened implicitly and closed explicitly.

- The first instruction a peer sends on a not-yet-seen identifier from its own
  space OPENS that channel. There is no separate open handshake and no
  confirmation.
- An OPEN-bearing instruction MUST carry the channel's binding parameters in its
  payload (for `session`, the RFC 0001 target the channel attaches to; for
  `agent`, the OPEN flag of §5). Subsequent instructions MUST NOT repeat them.
- Because a datagram may be lost, a sender MUST retransmit the OPEN-bearing
  instruction until the channel's own reliability mechanism confirms delivery.
  Each kind supplies that mechanism: `session` uses `frame_num`/`acked_frame`,
  `agent` uses the cumulative offsets of §5.
- A receiver MUST treat a duplicate OPEN for an already-open identifier as a
  retransmission, not an error.
- A peer closes a channel it no longer needs by sending a CLOSE for that
  identifier, expressed per kind (§5 for `agent`; `CLIENT_FLAG_SHUTDOWN` and the
  existing exit-status path for `session`).
- A receiver MUST discard instructions on a closed identifier.

Carrying the RFC 0001 target identity in the OPEN — rather than in every
envelope — is deliberate. It keeps full addressing expressible in the existing
grammar while keeping the steady-state envelope compact. A future revision that
needs relay-routable datagrams can extend §2 without changing §3.

#### 3.4 Limits

- An implementation MUST bound the number of concurrent channels per kind and
  MUST refuse (CLOSE) rather than allocate past the bound. The current agent
  bound (`MAX_AGENT_CHANNELS`, 8 per connection) becomes the `agent` kind's
  per-connection bound.
- An implementation MUST bound total buffered bytes across all channels.

### 4. Fragment reassembly under multiplexing

Multiplexing invalidates the single-instruction-in-flight assumption of the
existing reassembly buffer, which discards a partial assembly whenever a
fragment bearing a different instruction id arrives. Under multiplexing, a
session frame and an agent chunk in flight together would each destroy the
other's reassembly.

- A receiver MUST maintain reassembly state for multiple concurrent instruction
  ids, keyed by the fragment header's `id`.
- A receiver MUST NOT discard a partial assembly solely because a fragment
  bearing a different `id` arrived.
- A receiver MUST support at least 4 concurrent assemblies, MUST bound the total
  bytes buffered across them, and MUST evict least-recently-updated assemblies
  when a bound would be exceeded.
- The existing per-instruction fragment cap (`MAX_FRAGMENTS`) and the
  malformed-fragment rejection rules are unchanged and apply per assembly.

This requirement is independent of the envelope: instruction ids are already
unique per instruction, so the fix is in the receiver's bookkeeping alone. It is
nonetheless NORMATIVE here, because an implementation that adopts the envelope
without it will corrupt reassembly rather than degrade.

### 5. Agent channels

Agent forwarding before this document nests two multiplexers: individual
forwarded agent connections are addressed by a `u32` inside an `AgentRecord`,
those records are framed into one cumulative byte stream per connection, and
that stream is carried in `CAP_AGENT_DATA` entries on the session's messages —
each entry limited to 247 bytes by the capability table's `len: u8` budget.

This document collapses the two into one. **Each forwarded agent connection is
its own `agent` channel.** The record framing and its `channel` field are
removed; the mux channel identifier replaces them.

The payload of an `agent` channel instruction is:

    +----------+------------------+------------------+---------------+
    | flags:u8 | send_base:u64 LE | recv_ack: u64 LE | data: bytes   |
    +----------+------------------+------------------+---------------+
    0          1                  9                  17

- `flags` bits: `0x01` OPEN, `0x02` CLOSE, `0x04` FAIL. Remaining bits are
  RESERVED and MUST be zero; a receiver MUST ignore instructions with unknown
  bits set rather than guess.
- `send_base` is the offset of the first byte of `data` within this channel's
  cumulative outbound stream.
- `recv_ack` cumulatively acknowledges the peer's stream on this channel.
- Reliability is per channel: a sender MUST retransmit unacknowledged bytes, and
  a receiver MUST deliver bytes to the agent socket in offset order.
- OPEN MUST be set on the first instruction of a channel and MUST NOT be set
  thereafter. CLOSE and FAIL MUST be terminal; a receiver MUST close the
  underlying socket on either and MUST discard subsequent instructions.
- FAIL signals that the far end could not service the connection (no reachable
  agent, refused, or a limit from §3.4). A receiver MUST surface it to the local
  agent client as a closed socket, so the request fails rather than hangs.

Consequences (informative): agent data is now fragmented like any other payload
rather than chunked into 247-byte capability entries (`AGENT_DATA_MAX`), which
removes the per-message ceiling those entries impose — the table's `count: u8`
caps one message at `MAX_AGENT_DATA_CAPS` entries, about 59 KB of agent bytes,
with the remainder deferred to a later message. An agent channel also no longer
depends on a session existing, so forwarding is not bound to any particular
session's lifetime.

### 6. Negotiation

This envelope is not backward compatible: a baseline peer would parse `ver` as a
flags byte and the identifier as a capability table. It MUST therefore be
selected before any datagram is sent, and MUST NOT be negotiated in band.

- The client MUST select the protocol when it invokes the remote server through
  the ssh bootstrap, by explicit command-line argument.
- A remote server invoked WITHOUT that argument MUST speak the baseline
  (unenveloped) protocol.
- A peer MUST NOT change protocol for the lifetime of a connection.
- A local mux endpoint's IPC socket MUST carry a version stamp; a client
  encountering a mismatch MUST start a fresh endpoint and let the old one drain,
  rather than negotiate down.

Because the client controls the remote server's invocation, and a fresh
invocation accompanies every bootstrap, version skew across the datagram
transport is not reachable by construction; the stamp above covers only the
long-lived local endpoint.

### 7. Amendments to RFC 0008 and RFC 0001

**RFC 0008 §3** states that agent forwarding "remains the relay's
`srv-<pid>.sock` + `agent/sock` symlink mechanism (FDR 0004), unchanged by this
document," and its §4 capability-scope table lists ids 6/7/8 as transport
capabilities terminated by the relay. Both are amended:

- Agent forwarding MUST use `agent` channels (§5). The symlink election is
  REMOVED.
- With one connection per client-host pair, exactly one endpoint on the remote
  host serves the forwarded agent. `<base>/agent/sock` MUST therefore be a bound
  socket owned by that endpoint, NOT a symlink to a per-process socket. Neither
  peer performs takeover, liveness probing of a sibling, or election.
- The relay MUST still terminate agent traffic rather than pass it to the
  session daemon; the RFC 0008 security boundary (the daemon never brokers key
  material) is unchanged.

**RFC 0001 §3** capability ids `6` (`AGENT_FORWARD`), `7` (`AGENT_DATA`) and `8`
(`AGENT_ACK`) are RETIRED by this document. Implementations of this
specification MUST NOT send them. The ids MUST NOT be reassigned, so that a
baseline peer's entries remain unambiguous.

### 8. Known limitation: two client hosts, one remote host

Single ownership of `<base>/agent/sock` (§7) holds for connections originating
from one client host, which is the posh#136 case. It does NOT hold when a user
reaches the same remote account from two *different* client hosts: each has its
own mux connection, so two endpoints again contest one path, and which agent
should answer becomes a policy question this document does not settle.

Implementations MUST NOT claim stable ownership in that configuration. An
implementation encountering a bound `<base>/agent/sock` owned by a live peer
endpoint MUST NOT unlink or take it over; it SHOULD report the condition and
proceed without forwarding. Resolving the multi-client-host case — by
per-client-host sub-paths, an explicit policy, or an election among long-lived
mux endpoints — is deferred to FDR 0014.

### 9. Deferred transport mechanisms

This document specifies multiplexing. It deliberately does not specify four
transport mechanisms a multi-stream connection would ordinarily carry. Each is
recorded here because this document changes how much they matter, and because
retrofitting any of them MUST NOT require a second flag day.

#### 9.1 Selective acknowledgement

Acknowledgement in posh is cumulative throughout — `acked_frame`, `input_base`,
and the `recv_ack` of §5. That is correct for the `session` kind, whose payload
is a state synchronization rather than a stream: loss is repaired by sending a
newer diff against the last acked base, never by retransmitting stale content.

It is not obviously correct for the `agent` kind, which IS a byte stream: a
single lost datagram costs retransmission from the cumulative base. QUIC
addresses this with acknowledgement ranges.

A future revision MAY define range-based acknowledgement for stream-shaped
kinds. It MUST do so behind a `ver` bump or a new kind rather than redefining
§5's payload in place. Until then, implementations MUST NOT assume `recv_ack`
carries anything other than cumulative semantics.

Tracked as posh#142.

#### 9.2 Congestion control

posh paces sending off its RTT estimate and applies an RTO; it has no congestion
window, no slow start, and no loss response beyond retransmission. That is a
deliberate inheritance from mosh, and sound on mosh's premise: a screen diff is
bounded-rate, so there is little to control.

This document weakens that premise. One connection now carries agent traffic,
scrollback, and an arbitrary number of sessions — materially more bulk-shaped
than one terminal.

This document does not specify congestion control. Implementations MUST NOT read
that silence as a determination that none is required; the question is open, and
"deliberately none, because ..." is a valid resolution that has not yet been
made.

Tracked as posh#143.

#### 9.3 Flow control

§3.4 and §4 REQUIRE hard bounds on concurrent channels, buffered bytes, and
concurrent reassemblies, enforced by refusal. Those bounds exist for the
allocation-exhaustion reason given under Security Considerations and are
REQUIRED for it.

They are not backpressure. There is no credit or window mechanism at either the
channel or the connection level, so a sender that outpaces a receiver has its
channel refused rather than slowed. Whether the multiplexed connection needs
real flow control, or whether hard bounds plus the existing coalescing
capability suffice, is an open decision coupled to §9.2.

Tracked as posh#144.

#### 9.4 Key update

Not introduced by this document, and recorded only because this document changes
its significance. posh holds one AEAD key for a connection's lifetime, with a
deterministic counter nonce; there is no nonce-reuse exposure. There is also no
rekey and no forward secrecy, which mattered less when a connection died with
its session and matters more for a connection intended to outlive every session
on it.

A future revision adding key update SHOULD carry the key phase either in a `ver`
value or on reserved channel identifier 0 (§3.1). Both are reserved by this
document partly for that purpose, so the mechanism can be added without a wire
break.

## Security Considerations

- **Trust boundary is unchanged.** Channels ride inside the existing AEAD seal;
  a peer that can inject a channel identifier has already broken the seal.
  Nothing here weakens the datagram authentication or replay protections of RFC
  0001, and §7 preserves RFC 0008's rule that the session daemon never brokers
  SSH-agent key material.
- **Multiplexing widens what one authenticated peer can consume.** Concurrent
  channels and concurrent reassemblies are both attacker-influenceable by an
  authenticated peer. The bounds in §3.4 and §4 are REQUIRED for that reason,
  not merely for hygiene: without them a peer could force unbounded allocation
  by opening channels or by interleaving fragments across many instruction ids.
- **`agent/sock` becomes a bound socket rather than a symlink** (§7). The
  directory hardening FDR 0004 applies to `<base>/agent/` — 0700, self-owned,
  symlink-rejecting — MUST continue to apply, and an implementation MUST NOT
  adopt a pre-existing socket at that path it did not create.
- **Agent reachability outlives any one session** (§5). Because an agent channel
  no longer depends on a session, a connection can expose the user's agent to
  the remote host while no session is attached. An implementation MUST bound
  that exposure by a defined connection lifetime; the policy belongs to FDR
  0014, but the absence of one is a security defect, not a missing convenience.
- **Malformed payloads are corruption, not attack surface**, since the peer is
  authenticated — but they MUST cause the instruction to be discarded, never a
  panic or an over-read. This restates the RFC 0001 §3 and RFC 0008 rule for the
  new payloads of §2 and §5.

## Conformance Testing

- Envelope encode/decode roundtrip, including a rejected `ver` and a truncated
  envelope.
- Identifier partitioning: allocation stays within a peer's own space, ordinals
  start at 1, identifier 0 is refused as a data channel, and a RESERVED kind is
  closed rather than treated as a connection error.
- Concurrent reassembly (§4): interleaved fragments of two instructions both
  complete — the direct regression test for the discard-on-different-id
  behaviour this document forbids. Plus eviction under the byte bound. The
  inverse of `remote::sync::tests::interleaved_instructions_destroy_each_other_today`,
  which pins the pre-implementation behaviour and MUST be inverted, not deleted,
  when §4 lands.
- Agent channel (§5): OPEN/data/CLOSE lifecycle over the envelope, cumulative
  retransmission across a simulated loss, FAIL surfacing as a closed socket, and
  a payload larger than the retired 247-byte capability budget completing in one
  instruction.
- Ownership (§7): a second endpoint MUST NOT take over a live peer's bound
  `agent/sock` (the §8 configuration), and a single endpoint binds it directly
  with no symlink present.
- The posh#136 regression: with one connection per client host, the handoff
  outage measured by
  `remote::agent::tests::handoff_between_two_endpoints_leaves_a_multi_tick_outage`
  MUST NOT be reachable — no interval exists in which `agent/sock` is absent or
  resolves to an endpoint that cannot serve.
- Cross-host flows (real sshd, real `ssh-agent`, roam) remain covered by `just
  debug-agent-e2e` and `docs/manual-testing.md`.

## Compatibility

- This is a **flag-day wire change** for the datagram transport, selected out of
  band per §6. Baseline peers are unaffected because a baseline server is never
  invoked with the selector; there is no mixed-mode connection.
- The `ServerFrame` and `ClientMessage` encodings are unchanged — they become the
  `session` channel's payload verbatim, so the frame codecs, the capability
  table, and the posh-proto contracts are untouched. Verified, not assumed:
  `remote::sync::tests::a_nine_byte_envelope_prefix_leaves_both_codecs_verbatim`
  decodes both message types unaltered from behind a §2 envelope. Both decoders
  take a `&[u8]`, so decoding from an offset needs no codec change.
- RFC 0001 capability ids 6/7/8 are retired, not reused (§7). A baseline peer
  that still sends them remains unambiguous.
- Migration is incremental: the `agent` kind and a single `session` channel are
  sufficient to close posh#136, and additional `session` channels (the github #54
  connection sharing) require no further wire change. That sequencing is why the
  envelope is specified before the mux daemon exists.
- Future channel kinds MUST use a RESERVED kind value; future envelope fields
  MUST use a `ver` bump. Neither may be added ad hoc.

## References

Normative:

- RFC 0001: posh target grammar and capability table — §3 registry amended by §7.
- RFC 0008: posh unified session frame transport — §3 and §4 amended by §7.
- [RFC 2119] Key words for use in RFCs to Indicate Requirement Levels.

Informative:

- FDR 0004: SSH agent forwarding over the posh transport — the mechanism §5
  replaces.
- FDR 0014: Stable forwarded-agent endpoint — the feature record this contract
  serves; owner of the §8 multi-client-host policy and the §5 lifetime bound.
- FDR 0012: Session layer collapse — the relay-retarget extension; its v1
  explicitly scopes cross-host chaining out, which is why §3.3 binds identity at
  OPEN rather than per datagram.
- posh#136: the intermittent agent-forwarding failure, and the 9.9 s handoff
  outage measured in `crates/posh/src/remote/agent.rs`.
- github #54: the phase-2 connection mux — the process model this wire contract
  enables, closed as a decision without an implementation.
- github #103: host-global agent rendezvous — out of scope here; §8 is the
  nearest boundary.
- posh#142, posh#143, posh#144: the §9.1 / §9.2 / §9.3 open decisions.
