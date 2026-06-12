# SSH agent forwarding over the posh transport — scoping

---
status: draft (scoping, not yet reviewed)
date: 2026-06-12
---

## What

`posh box:dev` makes the user's local ssh-agent reachable from shells
on `box` — `git push`, `ssh`, `scp` inside a posh session authenticate
against the agent on the machine the user is physically at — with posh
carrying the agent protocol over its own roaming UDP transport.
Forwarding is on by default whenever a local agent is available
(opt-out `-a`); `--forward-agent=PATH` forwards a specific socket. With
multiple posh connections to the same host, the agent is forwarded once:
one stable `SSH_AUTH_SOCK` path on the remote host, one live forwarding
channel serving it. A second phase scopes a client-side connection mux
(the ControlMaster analog) that makes "one connection per host" literal.

## Why posh has to do this itself

OpenSSH agent forwarding (`ssh -A`) cannot cover posh sessions:

1. **The ssh connection is ephemeral.** The bootstrap
   (`remote/sshwrap.rs`) runs `posh-server new` over ssh, parses
   `POSH CONNECT`, and waits for ssh to exit before the UDP client even
   starts. An OpenSSH-forwarded socket lives exactly as long as that ssh
   connection — seconds. Sessions live for days.
2. **Even with a held-open ssh, roaming breaks it.** A TCP side channel
   dies on every network change and sleep; surviving those is the point
   of the transport.
3. **Persistent sessions outlive any single connection.** The session
   shell's environment is set once at daemon spawn
   (`session/daemon.rs`); a per-connection socket path (OpenSSH's
   `/tmp/ssh-XXXX/agent.N` pattern) would go stale on the first
   reattach. The remote `SSH_AUTH_SOCK` must be a stable path whose
   *backing* changes as connections come and go — the tmux + ssh symlink
   trick, done natively.

## Constraints from the existing design

These are load-bearing facts in the current code; the design follows
from them.

- **C1 — capability table is the only extension mechanism.** RFC 0001 §3:
  datagram-protocol changes MUST ride the TLV table behind the EXTENSION
  bit. Ids 2–223 are unassigned (`TERM_FEATURES` is anticipated but not
  allocated).
- **C2 — one instruction in flight per direction.** `FragmentAssembly`
  (`remote/sync.rs`) keeps a single current fragment id; a fragment from
  a different id discards the partial assembly. Agent traffic therefore
  cannot be a parallel top-level payload kind — it must ride *inside*
  `ClientMessage` / `ServerFrame`, sharing their fragmentation.
- **C3 — frames are state, not a stream.** Server→client frames carry
  latest-screen-state (re-produced, never retransmitted verbatim);
  client→server input is a reliable cumulative-offset byte stream
  (`InputOutbox`/`InputInbox`). Agent traffic needs reliable in-order
  bytes in *both* directions, so the input-stream machinery is the model
  and gets a mirror for the server→client direction.
- **C4 — the server spawns the shell before the first datagram.**
  `remote/server.rs` forks the PTY right after daemonizing, before any
  client message can arrive. The forwarding decision and the
  `SSH_AUTH_SOCK` value must therefore travel on the bootstrap argv
  (`posh-server new -A`), not in the datagram capability alone.
- **C5 — session env is inherited at creation.** The session daemon is
  double-forked from the first `posh attach`; its spawned shell inherits
  that process's environment plus `POSH_SESSION`/`POSH_GROUP`. So if
  posh-server exports `SSH_AUTH_SOCK` before exec'ing the inner
  `posh attach`, a session *created* through an agent-forwarding
  connection gets the right value with zero IPC changes. Pre-existing
  sessions cannot be retrofitted without new IPC (see Open questions).
- **C6 — both event loops are `poll()`-based.** Unix-socket fds (the
  remote agent listener and its accepted clients; the client-side
  connections to the local agent) slot into the existing fd sets; no
  async runtime appears.

## Design — phase 1: agent channel on the existing per-connection transport

### 1. Negotiation and CLI surface

- **Default on, best-effort.** Every roaming connection forwards the
  agent when a usable source socket exists; with none, it silently
  proceeds without forwarding. This diverges from OpenSSH's default
  deliberately: posh targets are overwhelmingly the user's own hosts,
  per-invocation opt-in fits long-lived reattach workflows badly, and
  the stable remote endpoint only pulls its weight if most connections
  participate. The exposure this adds is owned in Security
  considerations, not hand-waved.
- **Flag surface** (resolution order: flag > env > default):
  - `-a` / `--no-forward-agent` disables for this connection;
    `POSH_FORWARD_AGENT=no` (or `0`) disables by default from the
    environment — the profile-level opt-out for users who roam to
    hosts they don't trust.
  - `-A` / `--forward-agent` is an *explicit* enable: unlike the
    best-effort default, a missing or unconnectable source socket
    warns loudly (and forwarding stays off) rather than passing
    silently.
  - `--forward-agent=PATH` (equivalently `POSH_FORWARD_AGENT=PATH`,
    any value that isn't a yes/no word) forwards PATH instead of
    `$SSH_AUTH_SOCK` — a second agent, gpg-agent's ssh socket, etc.;
    the same shape as OpenSSH 8.2's `ForwardAgent <path>`. The path
    rides only in the *local* client (it is where channels connect, §5);
    nothing about it appears on the wire or the remote host. Bare `-A`
    stays a boolean — the path form is long-option-with-`=` only, so
    `-A host` can never swallow the target word.
- The source socket is resolved once at client startup; each channel
  `OPEN` connects to that resolved path. A path that dies mid-session
  degrades to per-request `FAIL` like any unreachable agent.
- The bootstrap remote command carries the *outcome*, not the policy:
  the client appends `-A` to `posh-server new …` (C4) only when
  forwarding is actually active. The server prepares the remote
  endpoint and exports `SSH_AUTH_SOCK` only when started with `-A`, so
  disabled or agent-less connections leave the symlink machinery
  completely untouched.
- **Chaining falls out for free:** inside a forwarded posh session,
  `SSH_AUTH_SOCK` points at `agent/sock`, so a nested `posh box2:…`
  picks it up by default and forwards the chain another hop, requests
  flowing all the way back to the origin agent.
- On the wire, three new capability ids (RFC 0001 registry; final
  numbers at allocation time — see Open questions):

  | Name | Direction | Payload | Meaning |
  |---|---|---|---|
  | `AGENT_FORWARD` | both | empty | Sender participates in agent forwarding on this connection. |
  | `AGENT_DATA` | both | `u64` stream offset + ≤247 bytes | One contiguous chunk of the sender's agent byte stream. Multiple entries per message allowed; offsets must be contiguous within a message. |
  | `AGENT_ACK` | both | `u64` | Cumulative ack of the peer's agent stream. |

  Per RFC 0001's rules, neither side sends `AGENT_DATA`/`AGENT_ACK`
  until it has seen `AGENT_FORWARD` from the peer; baseline peers skip
  unknown ids and are untouched. Encoding agent bytes as chunked TLV
  entries (rather than a negotiated change to the message body layout)
  keeps the body formats byte-identical in all negotiation states —
  zero compat risk — at ~1.2% chunking overhead (3 bytes per ≤255).
  The table's `count: u8` bounds one message at ~61 KB of agent data,
  comfortably above the flow-control window below. The alternative
  (a cap-gated extra section in the message body) is cleaner on paper
  but introduces a second body layout to test in every version-skew
  combination; recorded here, not recommended.

### 2. Reliability: a second cumulative byte stream, each direction

Reuse the input-stream design verbatim (C3): each side keeps an
`InputOutbox`-style outbox for its agent stream (unacked tail
retransmitted with each send, dropped on `AGENT_ACK`) and an
`InputInbox`-style inbox (dedupe retransmissions by offset, refuse
gaps). Agent activity sets a `force_ack`-style wake so chunks and acks
go out at `SEND_INTERVAL_MIN` pacing rather than waiting for the next
frame or heartbeat; an agent signature round-trip then costs ~1 RTT
over the existing flow.

### 3. Stream content: channel records

The byte stream carries framed records:

```
channel: u32   kind: u8   len: u32   payload: len bytes
```

kinds: `OPEN` (remote: a new unix client connected to the agent
socket), `DATA`, `CLOSE` (either side; half-close collapses to full
close — the agent protocol is strict request/response), `FAIL` (client:
local agent unreachable; the remote end answers the unix client with
`SSH_AGENT_FAILURE` and closes).

Channels map 1:1 to unix connections accepted on the remote agent
socket; the posh client opens one connection to the local
`$SSH_AUTH_SOCK` per channel. Records are protocol-agnostic byte pipes —
no agent-message parsing on the wire path (the agent and its clients
already do that; see Security for the limits this requires).

Limits (tuning levers): `MAX_AGENT_CHANNELS` 8 per connection;
per-channel buffered bytes capped at 256 KB (OpenSSH's max agent
message); per-direction flow-control window 64 KB — when the outbox
exceeds it, stop `poll()`ing the feeding unix fds (natural
backpressure, no new wire mechanism).

### 4. The remote endpoint: per-server socket + stable symlink ("forwarded once")

Directory: `<base>/agent/` where `<base>` is the existing session-dir
resolution (`POSH_DIR` > `XDG_RUNTIME_DIR/posh` > `TMPDIR/posh-{uid}` >
`/tmp/posh-{uid}`), created 0700 and checked with the same
`validate_session_dir` hardening (self-owned, no symlink, github #7).

- Each agent-capable posh-server binds its own socket
  `agent/srv-<pid>.sock` and atomically repoints the well-known symlink
  `agent/sock` at itself (symlink to a temp name + `rename`):
  **newest agent-forwarding connection wins**, the proven tmux
  pattern. No lock, no election protocol.
- `SSH_AUTH_SOCK=<base>/agent/sock` is exported by posh-server before
  exec'ing the inner command (C5). That one path covers plain
  plain `posh host` shells and sessions created through the connection
  alike, and stays valid across detach/reattach forever.
- **Liveness/takeover:** a server answers its unix clients only while
  its peer is active (heard within `AGENT_PEER_ACTIVE`, default 15 s —
  stricter than the 60 s `PEER_TIMEOUT`); otherwise it answers
  `SSH_AGENT_FAILURE` immediately rather than hanging the user's `git
  push`. Each agent-capable server checks the symlink on a slow tick
  (~5 s) and on reattach: if it dangles or its target server is
  inactive, repoint to self. A request outstanding longer than
  `AGENT_REQUEST_TMOUT` (10 s) also returns `SSH_AGENT_FAILURE`.
- "Forwarded once" semantics: there is one designated endpoint at one
  stable path. Additional forwarding-active connections cost only their 2-byte
  `AGENT_FORWARD` table entry until the symlink points at them; idle
  channels generate zero wire traffic. (Direct connections to a
  specific `srv-<pid>.sock` keep working — deterministic routing for
  anyone who wants the agent of a *particular* attach.)
- Cleanup: a server unlinks its own socket on exit; dangling
  `srv-*.sock` files for dead pids are garbage-collected by the same
  slow tick.

### 5. Client side

The client loop adds: on `OPEN`, connect to local `$SSH_AUTH_SOCK`
(nonblocking) and add the fd to the poll set; proxy bytes both ways
through the outbox; on connect failure send `FAIL`. Suspend/resume
(Ctrl-^ Ctrl-Z) and roaming need no special handling — the stream
machinery retransmits across gaps exactly like keystrokes, and agent
clients block on their unix socket meanwhile.

### 6. Failure modes (explicit)

| Situation | Behavior |
|---|---|
| Client roams / briefly offline | Requests stall up to `AGENT_PEER_ACTIVE`, then `SSH_AGENT_FAILURE`; channel stream resumes intact if the peer returns before the request timeout. |
| Owning connection quits | Symlink repointed by the next live forwarding-active connection within one slow tick; until then `SSH_AGENT_FAILURE` (dangling-symlink `connect` fails fast). |
| No forwarding-active connection at all | `SSH_AUTH_SOCK` points at a dangling symlink → immediate `ECONNREFUSED`/`ENOENT`, the same UX as a killed ssh-agent. |
| Local agent gone | `FAIL` → `SSH_AGENT_FAILURE` per request. |
| Session created while forwarding was off (opt-out, or no local agent) and attached later with forwarding on | Shell env lacks `SSH_AUTH_SOCK` — rarer under default-on, but not retrofittable without new IPC; documented, follow-up below. |

## Phase 2: connection mux (the ControlMaster analog)

Scoped here at decision level; it warrants its own design doc before
implementation.

- **Local mux daemon** per destination (keyed by canonicalized
  `user@host` + family + port-range), socket under `<base>/mux/`,
  started by the first remote-target invocation (daemonized like
  session daemons), lingering `POSH_MUX_PERSIST` (default ~60 s) after
  its last client detaches. It owns the ssh bootstrap, the key, the UDP
  connection, roaming state, RTT — and the agent channel.
- **Per-invocation CLI processes** speak the existing zmx-style unix
  IPC (`session/ipc.rs` framing, new tags) to the mux: open channel
  with target + size, then input/resize up, assembled frames down.
  Prediction and rendering stay in the foreground process; the mux
  relays opaque assembled messages, so display latency cost is one
  unix-socket hop.
- **Wire:** a `MUX` capability adds a channel id to `ClientMessage` /
  `ServerFrame`; today's per-connection state (frame numbering, acks,
  input/echo streams, client size) becomes per-channel on both ends.
  The remote posh-server generalizes from one PTY to a channel table
  (each channel = one inner command, typically `posh attach …`).
  `FragmentAssembly`'s single-id constraint (C2) becomes a real
  head-of-line problem under multiplexing — it grows into a small
  per-id assembly map (bounded, LRU) as part of this work.
- **Why it's worth it:** subsequent attaches to a host skip ssh
  entirely (the dominant connect latency, exactly ControlMaster's win);
  one NAT binding and one heartbeat per host instead of N; one roaming
  state; and agent forwarding is "once" by construction — phase 1's
  symlink machinery degenerates to the single mux connection owning the
  endpoint.
- **Risks:** blast radius (mux crash detaches every session on that
  host — sessions survive, as with a killed terminal); CLI↔mux version
  skew (mux socket carries a version stamp; mismatch starts a fresh mux
  and lets the old one drain); a substantial refactor of both event
  loops.
- **Sequencing:** phase 1 first. The agent stream is channel-addressed
  from day one specifically so it rides a mux channel unchanged; the
  capability registry entries don't change. Nothing in phase 1 is
  throwaway: per-channel stream state moves intact, only the remote
  endpoint election (symlink repointing) simplifies away.

## Alternatives considered

- **Drive OpenSSH: background `ssh -A -N -o ControlMaster` per host.**
  Rejected: dies on roam/sleep (point 2 above), adds an OpenSSH
  config-surface dependency, and still needs the stable-symlink half of
  this design to serve sessions. It is, however, a zero-code stopgap
  users can run today.
- **Second UDP association for agent traffic.** Rejected: a second
  NAT/firewall hole, second key, second roaming state to keep
  consistent; the existing flow has ample capacity.
- **Per-connection remote sockets, env updated per attach (OpenSSH
  model).** Rejected: cannot update a persistent session's environment
  (C5) — this is the problem statement, not a solution.
- **Negotiated body-layout extension instead of chunked TLV entries.**
  Recorded in §1; revisit if profiling ever shows the 1.2% chunk
  overhead or the 61 KB/message ceiling mattering (they shouldn't:
  agent messages are typically < 1 KB, identity lists a few KB).

## Security considerations

- **Same trust model as `ssh -A`, and the same warning applies:**
  anyone with the same uid (or root) on the remote host can use the
  forwarded agent while a connection is live. A compromised posh-server
  can request signatures at will while connected — inherent to agent
  forwarding, not introduced by this design.
- **Default-on is a real posture change** relative to OpenSSH and must
  be documented as such: every roaming connection exposes the agent to
  the remote host unless opted out. Accepted because posh targets are
  typically the user's own machines, and mitigated three ways: a
  profile-level kill switch (`POSH_FORWARD_AGENT=no`) plus per-connection
  `-a`; the man page recommending confirm-constrained keys
  (`ssh-add -c`) for semi-trusted hosts; and the per-request client
  notice (Open questions §3), which default-on argues for shipping
  enabled. Note the agent only ever *signs* — keys never leave the
  local machine — and nothing is reachable once the connection's
  client goes inactive (`AGENT_PEER_ACTIVE`).
- **Endpoint hardening:** `agent/` reuses the 0700/self-owned/no-symlink
  validation of the session dir (github #7); sockets are unlinked-on-exit
  and pid-scoped; symlink repointing is rename-atomic inside the
  validated dir, so no traversal/squat games from other uids.
- **Wire:** agent bytes ride the same AEAD-sealed, replay-protected
  datagrams as keystrokes; the TLV parser is already bounds-checked
  (RFC 0001 security considerations). New parsing surface — record
  headers and chunk offsets from an *authenticated* peer — must be
  bounds-checked the same way: bogus offsets/lens drop the message,
  never panic or over-allocate (channel count and buffer caps in §3
  bound memory).
- **DoS by same-uid remote processes:** connection-count and buffer
  caps in §3; a flooded channel stalls (backpressure), it does not grow.

## Sizing (phase 1)

| Piece | Where | Rough size |
|---|---|---|
| Stream + record codec, outbox/inbox mirror | `remote/sync.rs` | ~200 loc + tests (pure, table-driven like the rest of the file) |
| Capability entries + chunking | `remote/caps.rs`, both message paths | ~80 loc |
| Remote endpoint: listener, channels, symlink election, GC | `remote/server.rs` (+ small `remote/agent.rs`) | ~250 loc |
| Client proxy: connect, poll integration, FAIL paths | `remote/client.rs` | ~150 loc |
| CLI `-A`, bootstrap argv, env export | `main.rs`, `sshwrap.rs`, `server.rs` | ~60 loc |
| Docs: RFC 0001 registry update (or RFC 0002), FDR, README | `docs/` | — |
| E2E test: loopback connection + fake ssh-agent over the real loops | `crates/posh/tests/` | the bulk of the effort, as usual |

No new dependencies; no async runtime; macOS/Linux portability per ADR
0001 (everything here is `poll`/unix-socket/`rename`, already used).

## Tuning levers

- `AGENT_PEER_ACTIVE` 15 s (signal: spurious `SSH_AGENT_FAILURE` on
  flaky links → raise; hung `git push` complaints → lower).
- `AGENT_REQUEST_TMOUT` 10 s; slow-tick 5 s; window 64 KB; channels 8.

## Open questions

1. **Capability id allocation:** take the next free ids (2–4), or hold
   2 for the anticipated `TERM_FEATURES`? Proposal: allocate
   sequentially now (2 = `AGENT_FORWARD`, 3 = `AGENT_DATA`,
   4 = `AGENT_ACK`); the registry is the source of truth, anticipation
   is not allocation.
2. **Retrofitting env into existing sessions** (a `Setenv` IPC verb +
   `posh setenv`, the tmux analog, so old sessions can adopt
   `agent/sock`): proposed **out of scope** as a follow-up issue —
   independently useful, orthogonal machinery.
3. **Per-request notice** (one-line client banner "agent signature
   requested by box", rate-limited): cheap, and default-on forwarding
   strengthens the case for shipping it enabled — it is the only
   ambient signal that a remote host is using the agent. Noisy under
   heavy git use, hence rate-limited (e.g. one line per host per
   minute). Proposal: in v1, enabled, `POSH_AGENT_NOTICE=no` to
   silence.
4. **`posh ssh -A`** (the plain ssh wrapper): pass through to real ssh
   semantics or route through posh forwarding? Proposal: `posh ssh`
   stays a thin wrapper; only roaming targets get posh forwarding.
5. **RFC packaging:** fold the registry entries into RFC 0001's table
   plus a short RFC 0002 for the stream/record/endpoint contract, or
   one combined RFC 0002? (The stream semantics deserve normative text
   either way; this plan is the trail, not the spec.)
