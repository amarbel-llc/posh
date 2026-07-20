---
status: experimental
date: 2026-06-28
promotion-criteria: phase-1 implementation (github #55) landed. The agent
  E2E suite (`just debug-agent-e2e`) is green and now proves, with a real
  `ssh-agent` and a real `ssh-add -l` through the forwarded socket: the
  forwarding round-trip; the real detached `posh server -A` process (CLI
  arg-parse + `AgentEndpoint::from_env` + `SSH_AUTH_SOCK` export); and roam
  survival (the client rebinds its source port mid-stream, the server
  re-pins, and the agent op still completes). Wire ids are allocated in the
  RFC 0001 registry (6/7/8). Advanced to `experimental` on that basis. The
  remaining bar for `stable` is the fully out-of-process real-world clause:
  a real `git push` from inside a forwarded `posh host:session` over a real
  network roam. The raw `posh client` carries no forwarding (it lives in the
  ssh-bootstrapped `posh host` path), so that clause needs real ssh and is
  covered by the manual walkthrough (docs/manual-testing.md §5), not an
  automated test.
---

# SSH agent forwarding over the posh transport

## Problem Statement

`posh box:dev` puts the user in a shell on `box`, but `git push`, `ssh`,
and `scp` from inside that shell have no access to the keys on the
machine the user is physically at — there is no agent to authenticate
against. OpenSSH solves this with `ssh -A`, but `ssh -A` cannot cover
posh: the bootstrap ssh connection exits seconds after `POSH CONNECT`
(an OpenSSH-forwarded socket would die with it), a TCP side channel
dies on every network roam and sleep, and a persistent session's
`SSH_AUTH_SOCK` is baked into the shell environment once at daemon spawn
while its backing must change as connections come and go. posh must
therefore carry the agent protocol over its own roaming UDP transport.

## Interface

Agent forwarding is **on by default** on every roaming connection
whenever a usable local agent socket exists; with none, posh silently
proceeds without it. This deliberately diverges from OpenSSH's opt-in
default — posh targets are overwhelmingly the user's own hosts, and the
stable remote endpoint only pays off if most connections participate.
The added exposure is owned in [Limitations](#limitations), not
hand-waved.

**Flag and environment surface** (resolution order: flag > env >
default):

| Surface | Effect |
|---|---|
| `-a` / `--no-forward-agent` | Disable forwarding for this connection. |
| `POSH_FORWARD_AGENT=no` (or `0`) | Profile-level opt-out — the default-off switch for users who roam to hosts they don't fully trust. |
| `-A` / `--forward-agent` | *Explicit* enable: unlike the best-effort default, a missing or unconnectable source socket **warns loudly** (and forwarding stays off) rather than passing silently. |
| `--forward-agent=PATH` / `POSH_FORWARD_AGENT=PATH` | Forward `PATH` instead of `$SSH_AUTH_SOCK` (a second agent, gpg-agent's ssh socket, etc.) — same shape as OpenSSH 8.2's `ForwardAgent <path>`. The path lives only in the local client; nothing about it touches the wire or the remote host. Long-option-with-`=` only, so bare `-A host` can never swallow the target word. |

On the remote host, the forwarded agent is reachable at one stable path,
`SSH_AUTH_SOCK=<base>/agent/sock`, where `<base>` is the existing
session-dir resolution (`POSH_DIR` > `XDG_RUNTIME_DIR/posh` >
`TMPDIR/posh-{uid}` > `/tmp/posh-{uid}`). That one path covers both
plain `posh host` shells and sessions created through the connection,
and stays valid across detach/reattach forever.

**Forwarded once.** With multiple forwarding-active connections to the
same host, the agent is forwarded exactly once: the newest such
connection atomically repoints `agent/sock` at its own per-pid socket
(`agent/srv-<pid>.sock`) — the proven tmux symlink pattern, no lock and
no election protocol. Additional connections cost only their 2-byte
`AGENT_FORWARD` capability entry until the symlink points at them; idle
channels generate zero wire traffic. Direct connection to a specific
`srv-<pid>.sock` keeps working for anyone who wants a *particular*
attach's agent.

Ownership tracks the newest connection *with an active client*, not merely
the newest process: an endpoint whose client has roamed away (peer
inactive) relinquishes `agent/sock` on its slow tick so a sibling
connection whose client is still active takes it over, and reclaims it if
its own client returns. Without this, a roamed-away owner keeps the link
pinned to its still-bound-but-unserved socket (`socket_is_dead` reports the
live listener "alive," so no takeover fires) and starves the active
siblings — requests route to it and fast-fail. posh#136. This narrows the
window rather than closing it: the handoff waits on the slow-tick cadence,
and the definitive removal of the election race is the phase-2 mux (see
More Information), where a single endpoint owns the socket by construction.

**On the wire**, three new RFC 0001 capability ids
(`6 = AGENT_FORWARD`, `7 = AGENT_DATA`, `8 = AGENT_ACK`) carry a second
reliable cumulative byte stream in each direction —
mirroring the existing input-stream machinery — whose payload is framed
channel records (`channel:u32 kind:u8 len:u32 payload`) with kinds
`OPEN` / `DATA` / `CLOSE` / `FAIL`. Channels map 1:1 to unix
connections accepted on the remote agent socket; the posh client opens
one connection to the local `$SSH_AUTH_SOCK` per channel and proxies
opaque bytes — no agent-message parsing on the wire path. Baseline peers
skip the unknown ids and are untouched.

The ids are `6`/`7`/`8`, not the `2`/`3`/`4` the original design
proposed: ids `3`/`4`/`5` were taken by `SCROLLBACK`/`MORPH`/`BASE_SUM`
between that design and this implementation, and `2` is held for the
long-anticipated `TERM_FEATURES`. The exact numbers were never
load-bearing — the agent caps just need their own contiguous block; the
registry in RFC 0001 is the source of truth.

## Examples

Default — forward the local agent to a roaming session, push from the
remote:

    $ posh box:dev
    box$ git push          # signed by the agent on your laptop

Opt out for one connection to a host you don't fully trust:

    $ posh -a box:dev
    box$ git push          # no agent; falls back to whatever box has

Forward a specific socket (e.g. gpg-agent's ssh socket) instead of
`$SSH_AUTH_SOCK`:

    $ posh --forward-agent=$(gpgconf --list-dirs agent-ssh-socket) box:dev

Explicit enable that complains if no agent is reachable, instead of
silently proceeding:

    $ posh -A box:dev
    posh: -A given but no usable agent at $SSH_AUTH_SOCK; forwarding off

Chaining falls out for free — inside a forwarded session
`SSH_AUTH_SOCK` already points at `agent/sock`, so a nested hop forwards
the chain another link, requests flowing all the way back to the origin
agent:

    $ posh box:dev
    box$ posh box2:build   # box2 reaches your laptop's agent through box

## Limitations

- **Default-on is a real posture change** relative to OpenSSH: every
  roaming connection exposes the agent to the remote host unless opted
  out. The trust model is identical to `ssh -A` — anyone with the same
  uid (or root) on the remote can use the forwarded agent while a
  connection is live, and a compromised posh-server can request
  signatures at will while connected. Mitigated three ways: the
  profile-level `POSH_FORWARD_AGENT=no` kill switch plus per-connection
  `-a`; the man page recommending confirm-constrained keys
  (`ssh-add -c`) for semi-trusted hosts; and a rate-limited per-request
  client notice (shipping enabled in v1, `POSH_AGENT_NOTICE=no` to
  silence — default-on forwarding is what makes that banner the only
  ambient signal a remote is using the agent). The agent only ever
  *signs* — keys never leave the local machine — and nothing is
  reachable once the connection's client goes inactive.
- **Sessions created while forwarding was off** (opt-out, or no local
  agent at the time) and attached later with forwarding on have no
  `SSH_AUTH_SOCK` in their shell environment, because session env is
  inherited once at daemon spawn and there is no IPC to inject it
  afterward. Rarer under default-on; retrofitting it is tracked as
  github #53 (a `Setenv` IPC verb + `posh setenv`, the tmux analog) and
  is deliberately out of scope for phase 1.
- **`posh ssh` stays a thin ssh wrapper** — only roaming targets get
  posh forwarding; `posh ssh -A` passes through to real ssh semantics.
- **No second UDP association** and **no second key**: agent bytes ride
  the same AEAD-sealed, replay-protected datagrams as keystrokes,
  sharing one NAT/firewall hole and one roaming state. The transport
  has ample capacity for agent-sized traffic.
- **Phase 2 (connection mux, the ControlMaster analog) is out of scope
  here** — scoped at decision level in the plan and tracked as github
  #54. The phase-1 agent stream is channel-addressed from day one
  specifically so it rides a future mux channel unchanged.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| `AGENT_PEER_ACTIVE` | 15 s | Fast-fail `SSH_AGENT_FAILURE` rather than hang a `git push` when the peer is gone; stricter than the 60 s `PEER_TIMEOUT`. | Spurious failures on flaky links → raise; hung-push complaints → lower. |
| `AGENT_REQUEST_TMOUT` | 10 s | A single outstanding request that exceeds this returns `SSH_AGENT_FAILURE`. | Slow real agents (HSM, confirm prompts) time out legitimately. |
| slow tick | 5 s | Symlink liveness/takeover check + dead-`srv-*.sock` GC cadence. | Takeover after an owner quits feels sluggish → lower. |
| flow-control window | 64 KB / direction | Backpressure point: stop `poll()`ing the feeding unix fds when the outbox exceeds it. No new wire mechanism. | Large identity lists stall. |
| `MAX_AGENT_CHANNELS` | 8 / connection | Bounds concurrent agent clients and memory. | Legitimate parallel agent use (many concurrent `git` ops) hits the cap. |
| per-channel buffer | 256 KB | OpenSSH's max agent message; bounds memory per channel. | A real agent message exceeds it. |

## More Information

- **RFC 0011** (`docs/rfcs/0011-multiplexed-datagram-channels.md`) — **the
  mechanism this record describes is superseded there.** RFC 0011 §5 makes each
  forwarded agent connection its own mux channel, retiring the `AgentRecord`
  framing and the `CAP_AGENT_DATA`/`CAP_AGENT_ACK` carriage (RFC 0001 ids 6/7/8,
  now permanently reserved); §7 removes the `agent/sock` symlink election in
  favour of a bound socket owned by the single endpoint one connection per
  client-host pair implies. This record remains the account of WHY posh forwards
  an agent over its own transport and of the user-facing behaviour (on-by-default
  policy, roam survival, the tuning levers) — that part is unchanged. What is
  superseded is the wire carriage and the endpoint-ownership scheme. See also
  FDR 0014 (the stable-endpoint record) and posh#136 (the 9.9 s handoff outage
  the election produces).
- **github #55** — phase-1 implementation tracking issue (the sizing
  table as a checklist, the resolved open questions).
- **github #53** — follow-up: `Setenv` IPC + `posh setenv` to retrofit
  the agent socket into sessions created while forwarding was off.
- **github #54** — follow-up: phase-2 connection mux (ControlMaster
  analog), where "forwarded once" becomes true by construction.
- **`docs/plans/2026-06-12-ssh-agent-forwarding-design.md`** — the full
  scoping/design trail this record summarizes: constraints C1–C6,
  reliability and record-codec detail, failure-mode table, alternatives
  considered, and the original open questions.
- **RFC 0001** (`docs/rfcs/0001-target-grammar-and-capability-table.md`)
  — the capability table the three new ids extend, and the
  one-instruction-in-flight / state-not-stream rules the design works
  within.
- **FDR 0003** (`0003-mosh-parity-surface.md`) — the roaming transport
  surface this feature layers agent forwarding onto.
- **ADR 0001** — macOS/Linux portability; everything here is
  `poll`/unix-socket/`rename`, no new dependency and no async runtime.
