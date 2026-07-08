---
status: proposed
date: 2026-07-08
promotion-criteria: a working implementation of a connection-independent
  forwarded-agent endpoint (no per-connection symlink election) exists, with the
  posh#136 multi-connection starvation reproduced-then-fixed E2E — a `git push`
  from an idle-owner's sibling connection succeeds with zero handoff window. The
  interim relinquish-on-inactive refinement (already shipped) is NOT this bar; it
  narrows the race, this record closes it.
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
   election disappears entirely as a byproduct. This subsumes option 2. Its status
   is unresolved (see More Information): #54 is closed as completed, but no mux
   implementation or design doc exists in the tree, and the racy per-connection
   symlink election is still what runs.

Recommendation: option 2 is the smallest change that closes the race, and it does
not block on the much larger mux refactor. If the mux (option 3 / #54) is actually
resurrected and built, it makes option 2 redundant — so the sequencing question
(build the narrow broker now, or wait for the mux) is the open decision this
record exists to force.

## Limitations

- **The shipped interim fix (option 1) does not satisfy this record.** It narrows
  the window to the slow-tick cadence; it does not make the endpoint stable by
  construction. This FDR tracks closing the window entirely.
- **Broker blast radius (option 2/3).** A single long-lived endpoint (broker or
  mux) that owns `agent/sock` becomes a shared failure point: if it dies, agent
  forwarding for every connection to that host drops until it is respawned —
  versus today's per-connection endpoints, where one dying only loses its own
  election. Mitigated by the same respawn/liveness discipline the session
  daemons use, but it is a real trade of "many small independent owners" for "one
  stable shared owner."
- **Scope is the agent path only** (option 2). Session transport, roaming, and
  the ssh bootstrap are untouched; only the forwarded-agent endpoint is
  stabilized. The broader transport consolidation is #54's job.

## More Information

- **FDR 0004** (`0004-ssh-agent-forwarding.md`) — the agent-forwarding feature
  this stabilizes; its "Forwarded once" section documents the symlink election
  and now the shipped active-owner refinement (option 1).
- **posh#136** — the intermittent-drop bug this record's design closes; the
  landed relinquish-on-inactive fix (option 1) is `Refs #136`, not a close.
- **github #54** — the phase-2 connection mux (option 3), which would subsume
  this by construction. Flagged discrepancy: #54 is marked closed/completed, but
  no mux implementation or phase-2 design doc exists in the repo, and the
  per-connection symlink election it was meant to retire is still live. Its true
  status needs resolving before choosing between option 2 (build the narrow
  broker) and option 3 (revive the mux).
- **`crates/posh/src/remote/agent.rs`** — `AgentEndpoint` (the per-connection
  endpoint + symlink `claim`/`release`/`takeover`), the machinery a broker would
  sit above or the mux would collapse.
