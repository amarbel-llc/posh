---
status: experimental
date: 2026-07-08
promotion-criteria: "MET 2026-07-29 for the original bar: the mux endpoint
  (M1 of docs/plans/2026-07-28-connection-mux-endpoint-design.md,
  `remote/mux.rs`, behind `POSH_MUX`) is a working connection-independent
  forwarded-agent endpoint — from one client host there is no per-connection
  symlink election, ownership is structural — and the posh#136
  reproduced-then-fixed E2E passes:
  `remote::mux::tests::agent_forward_mux_m1_two_invocations_one_owner_zero_handoff_window`
  (`just debug-agent-e2e`) proves a real `ssh-add -l` from the surviving
  invocation succeeds with zero handoff window after its sibling is killed,
  the symlink target never moving (`ssh-add -l` stands in for the `git push`,
  as throughout the agent E2E suite). Advanced to `experimental` on that
  basis. The §8 two-client-host policy is ratified AND its mechanism landed
  (mux-named per-client-host sockets + most-recently-active election —
  `remote::agent::tests::two_mux_endpoints_elect_the_most_recently_active_client_host`).
  The remaining bar for `stable`: promote `POSH_MUX` to default-on (until
  then the per-connection election of FDR 0004 remains the default path and
  posh#136's fix is opt-in), plus the §5 lifetime-bound consent surface for
  any standing connection beyond the M1 session-ref policy."
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
process model remains #54's job; nothing in RFC 0011 waits on it — though
closing posh#136 waits on BOTH (RFC 0011 §7's conditional-adoption rule, added
by the 2026-07-28 review). The wire increment itself — envelope, session
channel, agent channels, behind `POSH_CHANNELS`/`--channels` — landed
2026-07-28; the mux endpoint (M1 of
`docs/plans/2026-07-28-connection-mux-endpoint-design.md`, approved M1-first
2026-07-28) landed 2026-07-29 — the §8 policy below is ratified and its
mechanism shipped with it.

### M1 landed (2026-07-29), behind `POSH_MUX`

The agent-only mux endpoint is implemented (`remote/mux.rs`; the remote half
is the `posh-server agent` verb, `remote/server.rs::run_agent_only`): one
double-forked local daemon per destination key owns a single enveloped
agent-only connection, per-invocation posh processes hold `MuxSessionRef`s
over its IPC socket, and sessions bootstrap with their own forwarding off —
so from one client host the remote `agent/sock` has exactly one owner by
construction. The M1 serviceability policy RFC 0011 §5 delegated here is in
force: agent channels are serviced iff a local session ref is held
(client-side enforcement — the side whose agent is exposed); unref-to-zero
FAILs new opens and closes open channels; the connection lingers
`POSH_MUX_PERSIST` seconds (default 60, 0 = none) with agent service off.
Exposure is identical to today's. The promotion-criteria E2E passes (see
frontmatter). `POSH_MUX` is opt-in and default-off until promotion: without
it, invocations forward per-connection and the FDR 0004 election (with the
posh#152 interim) remains exactly today's behavior.

### Decision (2026-07-28, RATIFIED): the two-client-host policy

RFC 0011 §8 defers to this record the case single ownership does not cover: the
same remote account reached from two different client hosts, two mux endpoints,
one `agent/sock`. This is not an edge case in the motivating deployment — the
forwarded agent is the *only* agent there (the #103 login-set rendezvous chains
`SSH_AUTH_SOCK` straight to posh's path), and hosts are reached from more than
one client machine — and §8's safe default (the second endpoint proceeds
without forwarding) would be a regression against even today's racy election.

**Proposed: a most-recently-active election among mux endpoints, on stable
per-client-host sockets.**

- Each mux endpoint binds its own **deterministically named** socket,
  `agent/mux-<client-id>.sock` — per client host, not per process, so it
  survives endpoint respawn. Anyone wanting a *specific* client host's agent
  connects to it directly; this subsumes the per-client-host-sub-paths option
  without giving up the one stable path.
- `agent/sock` stays an atomically repointed symlink electing among those
  sockets, owned by the endpoint whose **peer was most recently active**. The
  agent that answers is the one nearest the user's attention, which is the only
  ranking that predicts where the matching key lives.
- Handoff is **event-driven, not tick-driven**: a mux endpoint knows its own
  peer's activity transitions the moment they happen (they are its connection's
  heartbeat), so the owner releases on its peer going inactive and an active
  sibling claims on its next activity edge — no multi-tick window. What makes
  this election tractable where today's is not (posh#136): participants are one
  per client host, long-lived, and their liveness signal is the authoritative
  connection state rather than a bound-socket probe.
- An **explicit preference** (config/env naming a client id to pin) MAY sit on
  top as an override; it is not the base mechanism, because a static preference
  goes stale exactly when the user walks to the other machine.

Rejected: bound-socket-only with no election (the §8 safe default; starves the
second host), per-client-host sub-paths as the primary interface (relocates the
question — something must still choose what the one rendezvous path resolves
to, per #103), and a broker process owning `agent/sock` (reintroduces the
blast-radius and respawn machinery this record's option 2 died of, for no
routing power the election lacks).

Ratified 2026-07-28, closing the §8 question; the mechanism landed with the
M1 mux endpoint (2026-07-29): each `posh-server agent` binds its
deterministic `agent/mux-<client-id>.sock` (+ a pid liveness record) and
elects on the #152 marker machinery as a full sibling —
`remote::agent::tests::two_mux_endpoints_elect_the_most_recently_active_client_host`
pins the two-host election at the unit level. The explicit-preference
override remains unimplemented (MAY).

## Limitations

- **The shipped interim fix (option 1) does not satisfy this record.** It does
  not make the endpoint stable by construction; this FDR tracks removing the
  election entirely. The interim's own window has since been closed in place:
  the original relinquish-on-inactive left a measured **9.9 s of unusable
  `agent/sock` per handoff** — 4.9 s resolving to the inactive owner
  (fast-failing, `SSH_AGENT_FAILURE`) then 5.0 s absent (`ENOENT` on connect),
  two independent `AGENT_SLOW_TICK_MS` periods. The posh#152 interim
  (repoint-on-release: per-endpoint `srv-<pid>.active` activity markers,
  edge-driven release/reclaim on every `tick` call, and an atomic repoint at
  the freshest active sibling instead of an unlink) scales the ratified
  election philosophy above down to today's per-connection endpoints, and the
  measured handoff is now zero stale/absent time at the releasing endpoint's
  edge (residual: one `server_loop` poll wake). See
  `remote::agent::tests::handoff_repoints_to_the_active_sibling_on_the_inactivity_edge`.
  The mechanism is explicitly throwaway once the mux endpoint makes ownership
  structural — which it now does (M1, landed 2026-07-29), but only behind the
  opt-in `POSH_MUX`; the interim election remains the DEFAULT path until the
  gate is promoted, so it cannot be removed yet.
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
  the policy here. The policy is DECIDED (the ratified 2026-07-28 decision
  above) and its mechanism LANDED with M1: a most-recently-active election
  among mux endpoints on stable per-client-host sockets — far more tractable
  than today's election, since a mux endpoint's liveness is meaningful where
  a per-connection process's is not
  (`remote::agent::tests::two_mux_endpoints_elect_the_most_recently_active_client_host`).
- **The agent-channel lifetime bound is normative in RFC 0011 §5.** A
  connection with no open `session` channel does not service agent channels, so
  exposure is held to the union of session lifetimes — no worse than today,
  where the agent stream dies with its session. What remains owned here is the
  opt-in *standing* connection (its flag surface and consent semantics), should
  one ever be wanted; until this record specifies it, the session-bound
  behaviour is the only conforming one.
- **Scope is still the agent path only.** Session transport, roaming, and the ssh
  bootstrap are untouched by *this record*; RFC 0011's envelope is shared
  machinery, and the broader transport consolidation remains #54's job.

## More Information

- **FDR 0004** (`0004-ssh-agent-forwarding.md`) — the agent-forwarding feature
  this stabilizes; its "Forwarded once" section documents the symlink election
  and now the shipped active-owner refinement (option 1).
- **posh#136** — the intermittent-drop bug this record's design closes; the
  landed relinquish-on-inactive fix (option 1) was `Refs #136`, not a close.
  Closed by the M1 mux endpoint (2026-07-29) — opt-in via `POSH_MUX` until
  promotion.
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
