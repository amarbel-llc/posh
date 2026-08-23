---
status: stable
date: 2026-08-04
promotion-criteria: "MET in full. 2026-07-29, the original bar: the mux
  endpoint (M1 of docs/plans/2026-07-28-connection-mux-endpoint-design.md,
  `remote/mux.rs`) is a working connection-independent forwarded-agent
  endpoint — from one client host there is no per-connection symlink
  election, ownership is structural — and the posh#136
  reproduced-then-fixed E2E passes:
  `remote::mux::tests::agent_forward_mux_m1_two_sequential_invocations_one_owner_zero_handoff_window`
  (`just debug-agent-e2e`) proves a real `ssh-add -l` from the surviving
  invocation succeeds with zero handoff window after its sibling is killed,
  the symlink target never moving. The §8 two-client-host policy is ratified
  AND its mechanism landed (mux-named per-client-host sockets +
  most-recently-active election —
  `remote::agent::tests::two_mux_endpoints_elect_the_most_recently_active_client_host`).
  2026-08-04, the remaining bar: `POSH_MUX` promoted to DEFAULT-ON
  (`parse_mux_gate`, off only for 0/false/off/no — the posh#136 fix is now
  the default and the FDR 0004 election is the opt-out/mixed-version path),
  and the §5 consent-surface question discharged by decision: standing
  connections remain unoffered; the session-bound `MuxSessionRef` policy is
  the contract (see the 2026-08-04 decision below)."
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
construction. That holds while every local invocation for the destination
runs on the default: with mixed `POSH_MUX` on/off invocations, the per-connection
endpoints remain election siblings of the mux endpoint (M1 keeps
`agent/sock` a symlink for exactly this interop), so single ownership is
not yet structural. The M1 serviceability policy RFC 0011 §5 delegated here is in
force: agent channels are serviced iff a local session ref is held
(client-side enforcement — the side whose agent is exposed); unref-to-zero
FAILs new opens and closes open channels; the connection lingers
`POSH_MUX_PERSIST` seconds (default 60, 0 = none) with agent service off.
Exposure is identical to today's. The promotion-criteria E2E passes (see
frontmatter). `POSH_MUX` is DEFAULT-ON since the 2026-08-04 promotion:
`POSH_MUX=0` (or `false`/`off`/`no`) opts an invocation out, restoring
per-connection forwarding and the FDR 0004 election (with the posh#152
interim) byte-identically.

### Decision (2026-08-04): promotion to default-on; standing connections unoffered

`POSH_MUX` flipped from opt-in to default-on (`parse_mux_gate`,
`remote/mux.rs` — the `POSH_SESSION_FRAMES` off-switch shape). What made the
flip safe without new machinery:

- **Failure is fallback, not stranding.** Any ensure failure — endpoint
  unreachable, spawn failure, and specifically an older remote `posh-server`
  without the `agent` verb (its bootstrap fails, the daemon unlinks its
  socket and exits, the spawner sees the hello die) — warns once and
  proceeds with per-connection forwarding
  (`remote::mux::tests::spawned_daemon_that_dies_before_hello_falls_back_and_unlinks`).
- **The opt-out is byte-identical legacy**
  (`remote::mux::tests::mux_gate_off_keeps_the_bootstrap_byte_identical`),
  and mixed on/off usage stays safe because M1 keeps `agent/sock` a symlink
  and the mux endpoint elects as a full sibling.

Known accepted costs, recorded rather than engineered away: against an old
remote every invocation pays one failed ssh handshake before falling back
(no negative cache — add one only if field pain appears), and a cold start
serializes the mux bootstrap before the session bootstrap (the
`POSH_MUX_PERSIST` linger amortizes it across invocations).

The §5 consent-surface bar is discharged by decision, not by surface:
**standing connections remain unoffered.** The session-bound `MuxSessionRef`
policy is the only conforming behaviour — agent channels are serviced iff a
local session ref is held, so exposure stays the union of session lifetimes.
The `POSH_MUX_PERSIST` linger is a standing connection only in the transport
sense, not the consent sense: agent service is structurally off while
lingering (`MuxState::serviceable`), so no consent surface is required for
it. Should a serviceable standing connection ever be wanted, THIS record
must first specify its flag surface and consent semantics.

### Wire reconnect (2026-08-20, posh#162): the endpoint survives remote loss

The M1 daemon originally established its wire exactly once; a remote that
timed out and exited (its 60 s peer silence — e.g. the client host
suspending) left a zombie: a daemon serving refs forever, forwarding
nothing, absorbing every new invocation, reporting `state=connected` and a
stale `remote=` ident. The 2026-08-20 incident (documented on posh#162)
proved the failure live for ten hours — and proved that **socket errors
cannot be the death signal**: ~1500 heartbeats into a closed port produced
zero recv errors (posh#163 tracks where they go).

The fix, landed 2026-08-20: liveness is a **positive probe** — after 15 s
of wire silence each heartbeat re-requests the RFC 0013 §3 ident (a held
ident proves nothing about the wire staying alive), and 10 s unanswered is
the dead verdict. A **resume fast path** detects suspend directly (wall
clock racing the frozen CLOCK_MONOTONIC between loop iterations) and
condemns the wire without a probe when the gap exceeds the remote's 60 s
peer timeout — the endpoint has provably exited by then. On the verdict the
daemon tears down channel state once (agent channels failed, M2 session
conns get their SessionClose fallback cue, the held ident cleared), reports
`reconnecting`, and re-runs the ssh establish on a capped backoff
(0/2/5/15/60 s) for as long as refs are held — refs-to-zero still hands
over to the normal linger/exit. The IPC surface keeps serving throughout,
so invocations ride through remote loss, network outages, and
suspend/resume with at most a forwarding blip.
`remote::mux::tests::dead_wire_verdict_reconnects_and_relearns_the_ident`
pins the whole cycle in-process.

### Sessions reach the endpoint (2026-08-23, posh#161): the stable-path export

The reconnect above kept `agent/sock` alive — and a live-host triage
(`just debug-posh-agent-resolve`) then showed that **no session shell was
pointed at it**. Under M1 the session bootstrap sends no `-A` to
`posh-server` (`apply_mux_gate` moves ownership to the endpoint), and the
`-A` arm was the ONLY place `server::run`/the relay exported
`SSH_AUTH_SOCK=<base>/agent/sock` — so a mux-mode session was born with
whatever the bootstrap ssh left in the server's environment. That was an
sshd-forwarded agent socket: the bootstrap ssh carried the workstation's
`ForwardAgent` config (the bare-host path even defaults the real ssh to
`-A`), and the mux daemon's own bootstrap passed neither `-a` nor `-A`.
The host's login-shell rendezvous (the eng fish hook's first-wins
`ssh_client-agent.sock` symlink, posh#103's interim) then latched onto that
socket for every shell on the host. Its lifetime is an ssh TCP connection
(the workstation's ControlMaster) — the exact dependency posh exists to
remove — so a link drop took every shell's agent with it (`ENOENT` on the
dangling rendezvous, SIGPIPE mid-request), the endpoint's reconnect
restored nothing anyone used, and the next login shell re-latched onto the
NEW bootstrap's sshd socket ("works again after a new session"). Before
the 2026-08-04 promotion, sessions exported `agent/sock` through the `-A`
arm, which is how the rendezvous used to chain to posh's path.

The fix, landed 2026-08-23: with the endpoint owning forwarding the client
(1) asks the remote to export the stable path anyway — the
`POSH_AGENT_EXPORT=1` env prefix on the bootstrap command, honored by
`server::run` and the relay through `agent::session_auth_sock` (an env
prefix, not a server flag, so an older remote stays bootstrappable by
ignoring it) — and (2) runs the bootstrap ssh with the real `-a` (the
mux daemon's bootstrap too), so no sshd-forwarded competitor exists in
the session's environment at all; an explicit `posh ssh -A` still wins.
`remote::agent::tests::session_auth_sock_prefers_own_endpoint_then_export_then_nothing`,
`remote::sshwrap::tests::remote_command_carries_agent_export_prefix_only_when_set`,
and `main::tests::resolve_real_ssh_agent_forward_defaults_on` pin the
three halves. Residual, deliberately NOT posh's: a plain `ssh host` login
still forwards the workstation's real agent and can latch the first-wins
rendezvous onto that connection-bound socket ahead of posh's path — the
rendezvous policy is eng's (tracked there), and posh#103 remains the
end-state. The detached spawn (`posh host:session --detach`, FDR 0010)
still inherits the spawning ssh's environment (posh#103's second case).

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
  The mechanism was labeled throwaway once the mux endpoint makes ownership
  structural — which it now does by default (M1 landed 2026-07-29, promoted
  default-on 2026-08-04) — but it is NOT deletable: the `POSH_MUX=0`
  opt-out, mixed-version peers, and the ratified two-client-host election
  (which elects on this very marker machinery) all still need it. What the
  promotion changed is which endpoint shape is the common case, not the
  interop contract. Binding `agent/sock` directly (the RFC 0011 §7
  end-state) is a separate future decision, gated on retiring the opt-out.
- **Broker blast radius (option 2/3).** A single long-lived endpoint (broker or
  mux) that owns `agent/sock` becomes a shared failure point: if it dies, agent
  forwarding for every connection to that host drops until it is respawned —
  versus today's per-connection endpoints, where one dying only loses its own
  election. Mitigated by the same respawn/liveness discipline the session
  daemons use, but it is a real trade of "many small independent owners" for "one
  stable shared owner." The worst realization of this — the remote half dying
  and the local daemon zombifying forever (posh#161/#162) — is closed by the
  2026-08-20 wire reconnect above; the local daemon dying outright remains
  covered by the spawner's respawn (a dead socket is reclaimed by the next
  invocation).
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
  where the agent stream dies with its session. The standing-connection
  question is now DECIDED (2026-08-04, above): unoffered; the session-bound
  behaviour is the only conforming one, and any future serviceable standing
  connection must first be specified here (flag surface and consent
  semantics).
- **Scope is still the agent path only.** Session transport, roaming, and the ssh
  bootstrap are untouched by *this record*; RFC 0011's envelope is shared
  machinery, and the broader transport consolidation remains #54's job.

## More Information

- **FDR 0004** (`0004-ssh-agent-forwarding.md`) — the agent-forwarding feature
  this stabilizes; its "Forwarded once" section documents the symlink election
  and now the shipped active-owner refinement (option 1).
- **posh#136** — the intermittent-drop bug this record's design closes; the
  landed relinquish-on-inactive fix (option 1) was `Refs #136`, not a close.
  Closed by the M1 mux endpoint (2026-07-29); the fix is the DEFAULT since
  the 2026-08-04 `POSH_MUX` promotion.
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
