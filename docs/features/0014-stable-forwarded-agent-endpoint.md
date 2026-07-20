---
status: proposed
date: 2026-07-08
promotion-criteria: a working implementation of a connection-independent
  forwarded-agent endpoint (no per-connection symlink election) exists, with the
  posh#136 multi-connection starvation reproduced-then-fixed E2E — a `git push`
  from an idle-owner's sibling connection succeeds with zero handoff window. The
  interim relinquish-on-inactive refinement (already shipped) is NOT this bar; it
  narrows the race to a measured 9.9 s, this record closes it. The mechanism is
  now settled: RFC 0011 (multiplexed datagram channels), under which one
  connection per client-host pair makes single ownership structural. What remains
  for this record is the RFC 0011 §8 policy question (two client hosts reaching
  one remote account) and the §5 agent-channel lifetime bound.
---

# Stable forwarded-agent endpoint

## Problem Statement

SSH agent forwarding (FDR 0004) exposes one stable path on the remote host,
`SSH_AUTH_SOCK = <base>/agent/sock`, but that path is a **symlink whose target is
elected among connections** — the newest forwarding-active connection repoints it
at its own `agent/srv-<pid>.sock`. With multiple concurrent posh connections to
the same host, that election is racy and can leave the stable path resolving to a
connection that cannot serve it, so agent operations intermittently fail even
though a healthy, active connection exists (posh#136). The endpoint should be
**stable by construction** — the `SSH_AUTH_SOCK` path should always resolve to a
server that can actually reach a live local agent, with no election, handoff
window, or dependency on which connection last won.

## Interface

No user-facing surface change is intended. `SSH_AUTH_SOCK` stays
`<base>/agent/sock` and keeps working across detach/reattach/roam, exactly as FDR
0004 promises. The change is entirely in *what backs that path* and how ownership
is resolved: the goal is that a `git push` / `ssh` / signing op from inside any
forwarding-active session to that host succeeds whenever *any* attached client
with a reachable local agent exists — never gated on which connection's process
happens to own the symlink, and never subject to a takeover-latency window.

Observable improvement: the intermittent `SSH_AGENT_FAILURE` / "no key" that
appears and clears on its own (posh#136) stops happening.

## Design space

Three shapes, cheapest to most-thorough:

1. **Relinquish-on-inactive (shipped, interim).** The symlink election stays, but
   an endpoint gives up `agent/sock` when *its own* client goes inactive and
   reclaims it when active, so ownership tracks "newest connection **with an
   active client**" rather than "newest process." This removes the starvation
   (a roamed-away owner no longer pins the link) but **only narrows the window**:
   the handoff waits on the ~5 s slow tick, and the release/reclaim is still an
   election among per-connection processes. posh#136's landed fix.

2. **A stable agent-only endpoint (this record's proposal).** Replace the
   per-connection `srv-<pid>.sock` + symlink election with a single, long-lived
   **agent broker** under `<base>/agent/` that owns `agent/sock` for the host's
   lifetime and is fed by whichever connections are currently active. Connections
   register/deregister with the broker as their clients come and go; the broker
   routes an incoming agent request to any registered connection with an active
   client (preferring the most-recently-active). No symlink repointing, no
   takeover latency, no dead-owner window. Smaller than the full transport mux:
   it stabilizes *only* the agent path, reusing the existing per-connection
   `AgentEndpoint` channel machinery underneath the broker.

3. **The phase-2 connection mux (github #54).** A per-destination mux daemon owns
   the whole transport (ssh bootstrap, UDP, roaming, RTT, *and* the agent
   channel). Under it, "forwarded once" is true **by construction** — a single
   endpoint owns the socket because there is a single connection — and the agent
   election disappears entirely as a byproduct. This subsumes option 2. #54 is
   closed, but as a *decision* only — its own last note says "next step before
   code: write the phase-2 design doc," and no such doc or implementation exists.
   So this was greenfield, not a workstream waiting to land.

### Decision (2026-07-20): option 3's end-state, reached wire-first

The sequencing question this record existed to force is **settled**, and neither
option 2 nor option 3 as written is what was chosen.

Option 2's premise turned out to be false. A broker cannot own the agent path on
its own, because the agent stream is not separable from a session: `AgentEndpoint`
is a local of `server_loop`, its fds are in that loop's poll set, and its bytes
leave the host only as `CAP_AGENT_*` extras on a `ServerFrame`
(`remote/server.rs`). A separate broker process has no route to a client's local
agent except back through the owning `posh-server`'s event loop — so "a broker
above the existing endpoints" is a relay hop, not an owner.

What makes the ownership problem dissolve is a connection that carries more than
one stream. That is option 3's end-state, and the transport layer is already
separable enough to get there: `datagram.rs`'s `Connection` is a self-contained
AEAD-UDP pipe that knows nothing about frames, PTYs, or codecs.

So the chosen path is **the wire contract first, the daemon after**: RFC 0011
specifies the channel envelope, and single ownership of `agent/sock` falls out of
it — one connection per client-host pair means one endpoint, so the path becomes
a bound socket rather than an elected symlink (RFC 0011 §7). The mux daemon's
process model remains #54's job; nothing in RFC 0011 waits on it.

## Limitations

- **The shipped interim fix (option 1) does not satisfy this record.** It narrows
  the window to the slow-tick cadence; it does not make the endpoint stable by
  construction. This FDR tracks closing the window entirely. The residual is now
  measured rather than estimated: **9.9 s of unusable `agent/sock` per handoff**
  — 4.9 s resolving to the inactive owner (which fast-fails, `SSH_AGENT_FAILURE`)
  then 5.0 s absent entirely (`ENOENT` on connect), being two independent
  `AGENT_SLOW_TICK_MS` periods, since the owner releases on its own next tick and
  the active sibling claims on its next tick after that. See
  `remote::agent::tests::handoff_between_two_endpoints_leaves_a_multi_tick_outage`.
- **Broker blast radius (option 2/3).** A single long-lived endpoint (broker or
  mux) that owns `agent/sock` becomes a shared failure point: if it dies, agent
  forwarding for every connection to that host drops until it is respawned —
  versus today's per-connection endpoints, where one dying only loses its own
  election. Mitigated by the same respawn/liveness discipline the session
  daemons use, but it is a real trade of "many small independent owners" for "one
  stable shared owner."
- **Single ownership holds per client host, not across two.** This is the
  limitation the chosen path does NOT remove, and it is now the substance of this
  record. One connection per client-host pair means one endpoint — but a user
  reaching the same remote account from two *different* client hosts has two mux
  connections and two endpoints contesting one path again, and which agent should
  answer is a policy question. RFC 0011 §8 specifies the safe behaviour (an
  endpoint MUST NOT take over a live peer's bound socket) and explicitly defers
  the policy here. Options: per-client-host sub-paths, an explicit preference, or
  an election among long-lived mux endpoints — the last being far more tractable
  than today's, since a mux endpoint's liveness is meaningful where a
  per-connection process's is not.
- **The agent-channel lifetime bound is unspecified and is a security item.**
  RFC 0011 §5 decouples agent channels from sessions, so a connection can expose
  the user's agent to a remote host with no session attached; the RFC requires a
  bound but assigns the policy here. The decision taken this session: tie the
  connection to at least one live frame-shaped session to start (exposure no
  worse than today), with an opt-in standing connection later.
- **Scope is still the agent path only.** Session transport, roaming, and the ssh
  bootstrap are untouched by *this record*; RFC 0011's envelope is shared
  machinery, and the broader transport consolidation remains #54's job.

## More Information

- **FDR 0004** (`0004-ssh-agent-forwarding.md`) — the agent-forwarding feature
  this stabilizes; its "Forwarded once" section documents the symlink election
  and now the shipped active-owner refinement (option 1).
- **posh#136** — the intermittent-drop bug this record's design closes; the
  landed relinquish-on-inactive fix (option 1) is `Refs #136`, not a close.
- **RFC 0011** (`docs/rfcs/0011-multiplexed-datagram-channels.md`) — the wire
  contract this record's mechanism now rests on. §7 removes the symlink election
  and makes `agent/sock` a bound socket; §5 collapses agent forwarding onto mux
  channels; §8 defers the two-client-host policy back here.
- **github #54** — the phase-2 connection mux. Status RESOLVED (2026-07-20): it
  is closed as a *decision*, never implemented — its own closing note says "next
  step before code: write the phase-2 design doc," and no design doc or mux
  module exists in the tree. It was therefore not a workstream to wait for, which
  is what unblocked the sequencing decision above. #54 remains the owner of the
  mux *daemon* (process model, lifetime, local IPC); RFC 0011 owns the wire and
  does not wait on it.
- **`crates/posh/src/remote/agent.rs`** — `AgentEndpoint` (the per-connection
  endpoint + symlink `claim`/`release`/`takeover`), the machinery a broker would
  sit above or the mux would collapse.
