# The per-destination mux endpoint (github #54 phase 2) — design

Status: M1 IMPLEMENTED 2026-07-29 (`remote/mux.rs` + the `posh-server agent`
verb; FDR 0014 promotion-criteria E2E green —
`remote::mux::tests::agent_forward_mux_m1_two_sequential_invocations_one_owner_zero_handoff_window`),
PROMOTED 2026-08-04: `POSH_MUX` is default-on (`=0` opts out; FDR 0014
`stable`). posh#143/#144 RESOLVED 2026-08-05 by measurement (RFC 0011
§9.2/§9.3, the `remote/loadprobe.rs` harness, `just debug-mux-load`): §9.3
deliberately none, §9.2 requires a sender-side congestion response
(RTO backoff + AIMD aggregate bound). That mechanism is IMPLEMENTED
2026-08-05 (posh#155, `remote/agent.rs`, behind the default-on
`POSH_CONGESTION`) with the §9.3 re-measurement green — the M2 gate
specified below is fully satisfied. M2 DESIGN REVISED 2026-08-05 (this
revision, approved in review): scope, the local IPC session tags, the
remote channel-table peer, gating (`POSH_MUX_SESSIONS`), and the palette
info surface are specified below. M2 IMPLEMENTED 2026-08-05 per that
revision, behind the OPT-IN `POSH_MUX_SESSIONS`: the IPC session tags,
the mux daemon's channel routing, the `posh-server mux` channel-table
peer (`agent` kept as the M1 alias), the client `Wire` seam, and the
palette About surface — the in-process end-to-end
(`remote::mux::tests::m2_two_transports_share_one_connection_and_survive_peer_loss`)
pins two sessions + agent on one connection with per-channel failure
isolation. Promotion to default-on remains a later dated decision per
the gating section (its real-binary/real-agent E2E staging runs then).
Previously: ACCEPTED 2026-07-28 (M1-first sequencing approved; open
questions 1 and 2 resolved — `posh-server agent` subcommand, 60 s linger
default). Originally: DRAFT for review, 2026-07-28. This is the design doc #54's closing note
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

### M2 scope (revised 2026-08-05)

NAMED-SESSION attaches only (`posh host:session` — the relay-shaped path,
RFC 0008 §3): each session channel is one DaemonLink in the remote peer's
channel table, the §3 contract applied per channel, unchanged. Bare-host
ephemeral shells (`posh user@host`, remote-PTY-owning) keep per-invocation
connections; they MAY later ride the mux as auto-named "default" durable
sessions (the FDR 0011 unification direction, now specified in FDR 0015 — the
`ph` front-door + durable auto-id sessions) — a future milestone, not
M2. Relay retargeting (FDR 0012) remains a later feature on top of this
infrastructure. The session-daemon socket protocol (`session/ipc.rs`
`Tag`s) is untouched.

### M2 gating and rollback

`POSH_MUX_SESSIONS`, opt-in (M1's rollout arc): sessions ride the mux only
when selected; per-invocation relay connections remain the default path AND
the automatic fallback when the endpoint cannot be reached, spawned, or the
channel open fails — byte-identical to today, pinned by test like the M1
`mux_gate_off` contract. `POSH_MUX=0` still disables the whole endpoint.
Promotion to default-on is a later dated decision; criteria: the M2 E2E
green (two sessions + agent bulk on one connection; kill/reattach; zero
cross-channel talk), the `just debug-mux-load` regression bar holding, and
a daily-driving soak.

The review's sequencing conclusion, restated: the wire increment (envelope +
agent kind + single session channel) is necessary for both milestones but
closes nothing by itself; M1 is the shortest path to closing posh#136.

### Reconnect durability (added 2026-08-24)

A promotion prerequisite surfaced while the fleet was about to move onto
`POSH_MUX_SESSIONS` by default: a riding session channel must SURVIVE the mux
wire's death+reconnect (posh#162 seam), not drop. Originally the dead-wire
verdict tore down every riding session and synthesized a per-session close, so
the foreground client — which treats an established channel's close as a
server-side shutdown — exited to the local shell; a single link blip killed
every terminal on the link at once, a regression of the FDR 0003 roaming
story worst on the lossy links posh exists for. The fix keeps the client
untouched (mosh-parity): on the verdict the daemon RETAINS each session
channel, resets it to unconfirmed with its ref held, and the existing
open-until-confirmed pass re-drives the OPEN with the stored target on the
fresh wire, reattaching to the surviving remote session daemon
(`connect_or_create` idempotency). The client is sent nothing; frames stall,
the transport-agnostic "Last contact N ago" banner counts up, and the reattach
repaint clears it — exactly the baseline per-invocation UDP experience. The
wire-lost vs genuine-end distinction is structural (a real end is a relayed
`SESSION_WIRE_CLOSE`; a wire death sends the client nothing), and a remote that
never answers the re-OPEN falls to the existing open-timeout give-up (a real
close the client exits on). Pinned by
`remote::mux::tests::a_riding_session_survives_the_wire_death_and_reattaches`.
This satisfies the "kill/reattach" leg of the promotion criteria above for the
wire-outage case (as distinct from a deliberate client teardown).

### The FDR 0014 policy M1 needs

RFC 0011 §5 binds agent serviceability to "an open `session` channel on the
connection" — which an agent-only connection never has — and delegates
alternative session-association policies with an equivalent exposure bound to
FDR 0014. M1 is exactly such a policy, and it MUST be recorded in FDR 0014
when M1 lands (the RFC's wire rule stays untouched): the client associates
the mux connection with its live local sessions to the destination (the
`MuxSessionRef` count), FAILs agent opens and closes open agent channels when
the count reaches zero, and re-enables on the next ref. Enforcement is
client-side, which is the side that matters — the client answers
server-opened agent channels, and it is the client's agent being exposed;
remote-side enforcement is advisory against a compromised remote. The linger
window (below) keeps the connection but never agent service. Exposure is
identical to today's.

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
  the count gates agent serviceability (the FDR 0014 M1 policy) and the
  linger clock.
  Registration is by open IPC connection, so a crashed client auto-unrefs on
  socket close — no pid probing.
- M2 session-channel tags (specified 2026-08-05; #54's sketch made
  concrete). Prediction, rendering, scrollview, and the palette stay in the
  foreground process; the mux daemon holds NO terminal model — it routes
  whole assembled messages between IPC conns and wire channels, cost of one
  unix hop. Each IPC session channel maps 1:1 to an RFC 0011 `session`-kind
  wire channel; the wire OPEN carries the RFC 0001 target (RFC 0011 §3.3),
  and frame reliability is the existing per-channel `frame_num`/`acked_frame`
  scheme.
  - `MuxSessionOpen` (client→mux): the RFC 0001 target string. Opens the
    wire channel; implies the session ref (below). `MuxSessionOpenAck`
    (mux→client): the assigned channel ordinal, or a failure reason — on
    failure the client falls back to a per-invocation connection.
  - `MuxSessionMsg` (client→mux): one encoded `ClientMessage`, opaque to the
    mux (input, resize, frame acks, shutdown — exactly what `drive_client`
    already produces). Relayed onto the session channel verbatim.
  - `MuxSessionFrame` (mux→client): one encoded `ServerFrame`, opaque — so
    `CAP_SESSION_SIZE` geometry (RFC 0012), scrollback caps, and codec
    selection pass through untouched.
  - `MuxSessionClose` (either direction): local close (detach) or the wire
    channel's terminal surfacing (remote daemon exit; carries the
    exit-status path's payload). Dropping the IPC conn implies close, so a
    crashed client detaches cleanly — no pid probing, as with refs.
- An open session channel IS a session ref: `MuxSessionOpen` implies
  `MuxSessionRef` for the conn; the explicit ref tags remain for agent-only
  invocations (M1-mode attaches, `posh ssh`). Agent serviceability stays
  gated on refs > 0, satisfied by either kind.

## Lifecycle

- Spawn: first invocation for a destination forks the daemon, which performs
  the ssh bootstrap and holds the connection. Invocations proceed against
  their own connections (M1) as today, minus agent forwarding.
- Linger: after the last `MuxSessionRef` drops, the endpoint keeps the
  connection `POSH_MUX_PERSIST` (default 60 s — decided 2026-07-28) for
  fast re-attach, with agent
  service OFF during the window (the FDR 0014 M1 policy). Then it closes the
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
- M2 remote peer (specified 2026-08-05): `posh-server agent` generalizes to
  the full mux peer — verb `posh-server mux`, with `agent` kept as an alias
  for the zero-session case during transition. ONE process per destination:
  the agent endpoint exactly as M1, plus a channel table — each inbound
  session-channel OPEN resolves its target and stands up one DaemonLink
  (connect-or-create the named session daemon, `Tag::Init` with forwarded
  caps, per-channel O(1) `HeldFrame` retransmit, lossy→reliable input
  bridging): the §3 relay MUSTs (no second model, terminate agent caps,
  opaque frame bodies) applied PER CHANNEL, verbatim. A channel CLOSE (or
  `CLIENT_FLAG_SHUTDOWN`) drops only its DaemonLink; a DaemonLink failure
  closes only its channel (the peer sees the terminal + exit status; other
  sessions unaffected). The remote peer's §4.1 drain sends due session
  frames (all channels) before agent bulk, under the §9.2 congestion
  response — the loadprobe starvation scenario is the standing regression
  bar, and stops being synthetic once the real loops carry N sessions.

## Security

- The mux socket is same-uid IPC under a hardened directory; version stamps
  are parsed bounds-checked like all session IPC (RFC 0008 security rules).
- Agent exposure: bounded by the FDR 0014 M1 policy above, client-enforced;
  default-on forwarding policy, notices, and opt-outs are FDR 0004's and are
  unchanged — the mux endpoint inherits the resolved agent source
  (`--forward-agent=PATH` etc.) from the invocation that spawns it. A later
  invocation with a DIFFERENT agent source than the running endpoint's warns
  and keeps the endpoint's (restart the endpoint to change it); silently
  switching sources under existing sessions would be surprising in both
  directions.
- Key lifetime: the mux connection lives longer than any session connection
  today, which is what elevates the rekey/forward-secrecy gap (posh#145,
  gated on posh#146). M1 does not wait on it, and the decided 60 s linger
  default stays conservative (seconds, not days) until rekey exists.

## Open questions (for review, not blockers to M1 build-out)

1. RESOLVED 2026-07-28: M1's agent-only remote process is a new
   `posh-server agent` subcommand — no PTY, no relay, no session; different
   enough to name, and it keeps `new`'s spawn-a-shell contract unconditional.
2. RESOLVED 2026-07-28: `POSH_MUX_PERSIST` defaults to 60 s
   (ControlMaster-ish); the agent gate covers the sensitive surface during
   linger, and the window stays conservative until rekey (#145) lands.
3. Does `posh list`/diagnostics enumerate mux endpoints? (FDR 0007's dump
   covers the transport; a `posh mux ls` is cheap once the socket dir
   exists.) Deferred to implementation. PARTIALLY ADDRESSED by the M2
   palette info surface (below); a CLI `posh mux ls` remains open.

## M2 companion: the palette info surface (added 2026-08-05, user-requested)

A new command-palette entry (*About / transport info*, RFC 0005) renders a
table so a user can VERIFY which gates are affecting the live connection:
posh version, destination key, connection mode (mux vs per-invocation,
enveloped vs baseline), each gate's RESOLVED value and source
(`POSH_MUX`, `POSH_MUX_SESSIONS`, `POSH_CONGESTION`, `POSH_CHANNELS`,
`POSH_SESSION_FRAMES`, `POSH_RELAY`), and the live congestion summary
(cwnd/cuts/streak high-water) read from the mux `MuxStatus` reply. Data
flows over the existing RFC 0005 JSON-RPC channel; no new wire surface.

## M2 tuning levers (revisit against real usage; not settled)

- `MAX_SESSION_CHANNELS` per connection: start 16 (RFC 0011 §3.4 requires a
  bound; refusal, never allocation past it). Change signal: a legitimate
  attach refused.
- `POSH_MUX_PERSIST` (60 s) now also amortizes session re-attach, not just
  agent warmth. Change signal: cold-start latency complaints (raise) or
  lingering-daemon complaints (lower).
- Per-channel `HeldFrame` on the remote peer (one encoded frame per
  channel). Change signal: remote memory pressure at high channel counts.

## References

- github #54 — the decision this doc implements; its sketch is the M2 shape.
- RFC 0011 — the wire contract; §6 (selector, version stamp), §7 (ownership,
  conditional-adoption rule), §5 (lifetime bound; the M1 policy above is the
  FDR 0014-delegated variant it anticipates).
- FDR 0014 — the feature record; the 2026-07-28 ratified two-client-host
  election this doc's remote side mechanizes.
- FDR 0004 — agent-forwarding policy surface, unchanged.
- RFC 0008 §3 — the relay contract M2 composes with.
- posh#136, posh#142–#146, posh#152 — the bug, the deferred transport
  decisions, and the interim-mitigation revisit filed alongside this doc.
