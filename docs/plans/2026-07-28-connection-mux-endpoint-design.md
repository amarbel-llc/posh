# The per-destination mux endpoint (github #54 phase 2) — design

Status: DRAFT for review, 2026-07-28. This is the design doc #54's closing note
demanded ("next step before code: write the phase-2 design doc") and which the
2026-07-28 architecture review found missing: RFC 0011 specifies the wire
(channel envelope) and §6 already presupposes a local mux endpoint's IPC
socket, but nothing specified the endpoint itself. FDR 0014's promotion
criteria are unreachable without it.

## What it is

A local, per-destination daemon that owns THE connection to a remote host:
the ssh bootstrap, the AEAD-UDP association, roaming state, RTT/RTO, and the
forwarded-agent channel. Per-invocation `posh` processes become IPC clients of
it. One connection per client-host pair is what makes `agent/sock` ownership
structural (RFC 0011 §7) and is ControlMaster's amortization win.

Non-goals: the wire contract (RFC 0011 owns it); the two-client-host election
policy (FDR 0014's proposed decision owns it; this doc supplies its mechanism
hooks); any change to session daemons, the relay contract (RFC 0008 §3), or
local (non-remote) attaches, which never touch the mux.

## Keying and placement

- Destination key: canonicalized `user@host` + address family + port range
  (#54), rendered as a filesystem-safe slug.
- Endpoint socket: `<base>/mux/<destkey>.sock`, where `<base>` is the existing
  session-dir resolution. The `mux/` directory gets the same hardening as
  `agent/` (0700, self-owned, symlink-rejecting).
- One daemon per key; started on demand by the first remote-target invocation,
  double-forked and process-grouped exactly like session daemons
  (`session/daemon.rs` pattern). A losing race to bind the socket exits and
  connects to the winner.

## Milestones — and which one closes posh#136

**M1 (recommended first): the agent-only mux connection.** The mux endpoint
owns ONE enveloped connection carrying the `agent` kind and zero `session`
channels; sessions keep their existing per-invocation connections, invoked
with forwarding OFF (`-a`), so the mux connection is the only agent-capable
connection to the destination. Ownership of `agent/sock` is then structural —
exactly one endpoint per client host — and posh#136 closes without waiting on
the §9.2/§9.3 congestion and flow-control decisions (posh#143/#144), because
the shared connection carries only agent-sized traffic. The cost: no
bootstrap amortization yet, one extra standing connection per destination,
and a refinement to RFC 0011 §5's lifetime bound (below).

**M2: session sharing.** Session channels move onto the mux connection;
attach latency drops to a unix-socket hop; the per-session bootstrap
disappears. GATED on resolving posh#143/#144 (even a measured "deliberately
none, because ..." resolution) — N sessions plus agent bulk on one connection
is the load shape that voids the no-congestion-control inheritance. M2 also
generalizes the remote `posh-server` from one PTY to a channel table and is
where `FragmentAssembly`'s concurrency (already landed per RFC 0011 §4)
starts carrying real interleaving.

The review's sequencing conclusion, restated: the wire increment (envelope +
agent kind + single session channel) is necessary for both milestones but
closes nothing by itself; M1 is the shortest path to closing posh#136.

### The §5 refinement M1 needs

RFC 0011 §5 binds agent serviceability to "an open `session` channel on the
connection" — which an agent-only connection never has. The bound's security
intent (agent exposure never exceeds the union of session lifetimes) is
enforced meaningfully on the CLIENT side — the client answers server-opened
agent channels, and it is the client's agent being exposed; remote-side
enforcement is advisory against a compromised remote. Amend §5 so the
serviceability condition is: an open `session` channel on the connection, OR
a live session to the same destination through the same local endpoint (the
M1 shape, asserted by the client). The client MUST FAIL agent opens and close
open agent channels when its endpoint's last local session to the destination
ends; the linger window (below) keeps the connection but not agent service.
Exposure is identical to today's. This amendment should land with M1, not
silently.

## IPC

zmx-style framing reused from `session/ipc.rs` (1-byte tag + u32 LE length),
new tags in the mux socket's own tag space:

- `MuxHello` (client→mux): protocol/version stamp (RFC 0011 §6), client pid.
  `MuxHelloAck` (mux→client): version stamp, connection state (bootstrapping /
  connected / draining), destination key. A client seeing a stamp mismatch
  MUST start a fresh endpoint (bind a new socket name variant, e.g.
  `<destkey>.sock.<ver>`) and let the old one drain — never negotiate down.
- M1 needs nothing else session-shaped: agent traffic terminates inside the
  mux (local `$SSH_AUTH_SOCK` dial on channel open), so no per-request IPC
  exists at all. A `MuxStatus` tag (counts, peer address, last-heard age —
  the FDR 0007 dump surface) rides along for diagnostics.
- M1 session accounting: `MuxSessionRef` / `MuxSessionUnref` (client→mux) —
  each local posh invocation targeting the destination registers while alive;
  the count gates agent serviceability (§5 refinement) and the linger clock.
  Registration is by open IPC connection, so a crashed client auto-unrefs on
  socket close — no pid probing.
- M2 adds the session-channel tags (open-with-target, input, resize, frame
  delivery, detach), lifted from #54's sketch: prediction and rendering stay
  in the foreground process; the mux relays whole assembled messages, cost of
  one unix hop. Their shapes are deferred to the M2 revision of this doc.

## Lifecycle

- Spawn: first invocation for a destination forks the daemon, which performs
  the ssh bootstrap and holds the connection. Invocations proceed against
  their own connections (M1) as today, minus agent forwarding.
- Linger: after the last `MuxSessionRef` drops, the endpoint keeps the
  connection `POSH_MUX_PERSIST` (default 60 s) for fast re-attach, with agent
  service OFF during the window (§5 refinement). Then it closes the
  connection and exits. `POSH_MUX_PERSIST=0` disables lingering.
- Crash/blast radius: an M1 mux crash loses agent forwarding to that
  destination until the next invocation respawns it (sessions are untouched —
  they own their connections). An M2 mux crash detaches every session on the
  destination; sessions survive in their daemons, and clients respawn the mux
  and re-attach — the same failure surface as a killed terminal. This is the
  FDR 0014 "broker blast radius" trade, accepted there.
- Shutdown: SIGTERM drains (FAIL new agent opens, close channels, close
  connection); the socket unlinks on exit. A stale socket (crash) is detected
  by failed connect and unlinked by the next spawner — the session-daemon
  pattern.

## Remote side

- The client invokes the remote server with the RFC 0011 §6 selector plus its
  client id; the remote `posh-server` binds `agent/mux-<client-id>.sock`
  (deterministic, respawn-surviving) and participates in the FDR 0014
  most-recently-active election for `agent/sock`. With a single client host,
  the election is trivially stable (one participant) — the posh#136 property.
- Client id: the client host's sanitized hostname (the value the FDR 0014
  election names). Two client hosts sharing a hostname collapse to one id —
  acceptable, the election then treats them as one participant; a config
  override (`POSH_CLIENT_ID`) covers the pathological case.
- The M1 remote server for the mux connection is a `posh-server` with no PTY:
  it serves agent channels and the election only. M2 folds session relaying
  back in via the RFC 0008 relay contract.

## Security

- The mux socket is same-uid IPC under a hardened directory; version stamps
  are parsed bounds-checked like all session IPC (RFC 0008 security rules).
- Agent exposure: bounded by the §5 refinement above, client-enforced;
  default-on forwarding policy, notices, and opt-outs are FDR 0004's and are
  unchanged — the mux endpoint inherits the resolved agent source
  (`--forward-agent=PATH` etc.) from the invocation that spawns it. A later
  invocation with a DIFFERENT agent source than the running endpoint's warns
  and keeps the endpoint's (restart the endpoint to change it); silently
  switching sources under existing sessions would be surprising in both
  directions.
- Key lifetime: the mux connection lives longer than any session connection
  today, which is what elevates the rekey/forward-secrecy gap (posh#145,
  gated on posh#146). M1 does not wait on it, but the linger default stays
  conservative (minutes, not days) until rekey exists.

## Open questions (for review, not blockers to M1 build-out)

1. Should M1's agent-only remote process be `posh-server agent` (a new
   subcommand) or a flag on `new`? Leaning subcommand: no PTY, no relay, no
   session — different enough to name.
2. `POSH_MUX_PERSIST` default: 60 s (ControlMaster-ish) vs 0 (no linger)
   until rekey lands. Leaning 60 s; the agent gate already covers the
   sensitive surface during linger.
3. Does `posh list`/diagnostics enumerate mux endpoints? (FDR 0007's dump
   covers the transport; a `posh mux ls` is cheap once the socket dir
   exists.) Deferred to implementation.

## References

- github #54 — the decision this doc implements; its sketch is the M2 shape.
- RFC 0011 — the wire contract; §6 (selector, version stamp), §7 (ownership,
  conditional-adoption rule), §5 (lifetime bound; refinement above).
- FDR 0014 — the feature record; the 2026-07-28 proposed two-client-host
  election this doc's remote side mechanizes.
- FDR 0004 — agent-forwarding policy surface, unchanged.
- RFC 0008 §3 — the relay contract M2 composes with.
- posh#136, posh#142–#146, posh#152 — the bug, the deferred transport
  decisions, and the interim-mitigation revisit filed alongside this doc.
