---
status: proposed
date: 2026-08-25
promotion-criteria: >
  proposed -> experimental: every non-PTY-owning process class (agent
  endpoints, mux daemons, relays) cuts over by orderly termination +
  re-establish with no session loss, driven by a `posh upgrade` verb that
  detects staleness via the RFC 0013 idents. Phase 2 (session-daemon
  handoff via sync-as-client + the cutover kernel) is deliberately
  SEQUENCED AFTER FDR 0011 reaches accepted — the unification retires the
  Architecture-A roaming server as a PTY owner, collapsing Phase 2 to one
  process class — and needs the cutover-kernel RFC (fd pass + ownership
  flip + the parentage sub-decision) accepted.
---

# Live version cutover (upgrade running servers+clients without draining sessions)

## Problem Statement

A profile update makes a new posh build current on disk, but every long-lived
process keeps running the build it started with — often for days. Today the
only way to get new code live end-to-end is to drain: kill or exit every
session, daemon, and endpoint on both hosts and reconnect. In practice fixes
(the posh#161 instrumentation, the posh#162 reconnect) sit inert on the very
processes that need them, and version skew across the fleet is the normal
state rather than a transient.

This record designs **version cutover**: moving running posh processes onto
the new installed build without losing sessions or forwarding. The visibility
half already exists — RFC 0013/0014 make "who runs what build" observable
from either end — this is the actuator half.

## Cutover inventory (what has to move, hardest last)

1. **Interactive clients** — foreground, user-restartable; detach/reattach is
   the cutover. Baseline, no design needed.
2. **Remote agent-only endpoints** (`posh-server agent`) — already cheap
   since the posh#162 reconnect: kill one and the client-side mux daemon
   re-bootstraps it, spawning whatever binary is current on the remote. A
   cutover is one deliberate kill.
3. **Client mux daemons** — partially covered by the `MUX_PROTO_STAMP`
   variant-socket machinery: a new-generation invocation spawns the
   `<key>.<ver>` socket and the old daemon drains (`mux ls` labels it
   `old-generation`). But that fires only on a *protocol* stamp bump, not a
   build change, and the old daemon drains only as its refs exit —
   long-lived sessions pin it indefinitely.
4. **Relay processes** (`posh relay`) — per-attach, die with their attach;
   they cut over with the client.
5. **Roaming servers** (`remote/server.rs`) — own a PTY, a UDP socket, and
   the session AEAD key. Hard — but scheduled out of existence: FDR 0011
   reduces `posh-server` to a disposable relay (class 4), which is why
   Phase 2 is sequenced behind it.
6. **Session daemons** (`session/daemon.rs`) — own the shell's PTY and
   process group; killing one kills the user's shell. Hardest, and after
   FDR 0011 the only PTY owner.

## Design

Phased by the inventory: generalize what already works, shrink the PTY-owner
problem by sequencing FDR 0011 first, then hand off the one remaining class
through the machinery sessions already use, then surface the actuator.

### Phase 1 — termination-and-re-establish, generalized (classes 1–4)

Extend the posh#162 pattern: make every non-PTY-owning peer relationship
re-establishable, then cutover = orderly kill. Already true for agent
endpoints; the gaps are the *triggers*:

- Mux daemons need a **build-generation** drain in addition to the protocol
  stamp: a new invocation whose build differs from the running daemon's
  spawns the variant socket and marks the old daemon draining, and a drain
  deadline (or an explicit `posh upgrade`) migrates pinned refs instead of
  waiting for them to exit.
- The M2 session channels a mux daemon carries already survive a wire
  death+reconnect (posh#162); a deliberate daemon swap rides the same seam —
  the successor re-drives the OPENs and the remote session daemons see a
  reattach.

### Phase 2 — session-daemon handoff: sync-as-client, then a cutover kernel

**Sequencing decision (2026-08-25): FDR 0011 lands first.** The unification
makes the daemon own the screen and reduces `posh-server` to a thin,
disposable relay — no second PTY, no second model. Once Architecture A is
retired (the `POSH_RELAY=0` rollback and the explicitly non-durable
`--ephemeral` throwaway are out of cutover scope), class 5 stops being a PTY
owner: a relay dies with its attach like class 4, and its roaming client
reattaches through the surviving daemon. Phase 2 then targets exactly ONE
process class — the session daemon — and that daemon holds **no transport
crypto** (the relay owns the AEAD wire), so no key or replay-counter state
crosses the handoff at all. The nonce-epoch trick below survives only as a
contingency note for the Arch-A residual.

**The mechanism: the new daemon is temporarily a client.** Instead of
inventing a state-blob codec, the successor bootstraps through the machinery
every attach already exercises:

1. **Sync as an observer client.** The new build's daemon process connects
   to the old daemon over the local IPC as a frame-capable client and
   mirrors state the way any client does — `Full` keyframe, diffs, the RFC
   0009 scrollback stream. It attaches as an **observer**: excluded from
   smallest-wins size arbitration (a syncing successor must never resize the
   user's session) and from input.
2. **Verify.** The successor proves it is in sync before anything is
   irreversible — the RFC 0006 base-integrity checksum is the ready-made
   primitive ("my mirror hashes identical to yours"). A successor that
   cannot get in sync (or a build the old daemon rejects) simply detaches:
   the old daemon keeps serving. Rejectability is inherited from the
   multi-client machinery rather than choreographed.
3. **Cutover kernel.** The old daemon pauses PTY reads, drains in-flight
   output, sends an authoritative `dump_vt()` snapshot over the local
   channel (VT bytes — the existing, inherently version-tolerant serializer
   contract, closing any gap between the frame mirror and the true terminal
   state), passes the PTY master, listener, and live client conns via
   `SCM_RIGHTS`, and flips ownership on the successor's ack. Local and
   fast — the pause is microseconds, invisible under the existing "last
   contact" banner machinery.
4. **Parentage residual (the surviving A-vs-B sub-decision).** The daemon
   is the shell's *parent*: only it can `wait()` for the exit code, and it
   kills the process group on exit — neither transfers to another pid. The
   fork the original design space called A-vs-B survives only here, demoted
   to a sub-decision: **exec-in-place** for the daemon pid (state arrives
   via steps 1–3, exec preserves parentage), or **demote the old daemon to
   a reaper shim** (a minimal loop: `waitpid` the shell, forward the exit
   status over the IPC conn it already holds, exit). The shim keeps a sliver
   of old code alive but bounds it to tens of lines.

What remains RFC-worthy shrinks accordingly: not a state-blob format but the
**cutover-kernel contract** — the observer-attach flag, the verify/flip
handshake, the fd-pass manifest, and the parentage mechanism. Version
tolerance rides RFC 0001 caps and VT bytes, both of which already have a
skew story.

**The flag day is structural.** The *old* build must already contain the
sender side (observer accept, verify, fd pass), so the first upgrade under
this scheme is still a drain. Shipping the mechanism early and dormant is
what starts the clock — an argument for landing the cutover kernel well
before the `posh upgrade` surface that drives it.

### Phase 3 — orchestration surface

- **`posh upgrade [host]`** — the one-host actuator: compare each running
  process's ident (RFC 0013/0014 carry build + `start_unix_ms`, so "stale
  since when" is knowable) against the installed binary, and drive the
  applicable mechanism per class.
- **Palette command** — "server is N days behind installed build — upgrade
  now": the RFC 0013 About dialog already renders the staleness; this adds
  the button.
- **Automatic cutover** on ident-mismatch detection stays behind a safety
  gate (default off) until the handoff mechanisms have soaked.

### Why this is tractable now (building blocks)

- **Version visibility (RFC 0013/0014):** both ends see the far end's build
  (`CAP_SERVER_IDENT`/`CAP_CLIENT_IDENT`, `mux ls remote=`/`self=`, the
  palette About, the status sockets). The trigger is "running ident ≠
  installed binary's version".
- **Reconnect (posh#162):** any process whose peer can re-establish is
  upgradeable by termination.
- **Ignore-unknown caps (RFC 0001):** the wire tolerates version skew
  mid-cutover by design.
- **Nix store immutability:** the running binary's store path never mutates
  under it; "current" is a stable profile symlink. Detection is a clean path
  comparison, and the old build stays runnable throughout the cutover — no
  half-replaced-binary hazard.

## Examples

    # one host, everything stale
    $ posh upgrade box
    mux daemon box: 0.4.1 -> 0.4.4 (drained, 2 session channels migrated)
    agent endpoint box: 0.4.1 -> 0.4.4 (re-bootstrapped)
    session daemon dev: 0.4.1, PTY owner — handoff not yet implemented, skipped

    # from inside a session: palette About shows the server build; once
    # Phase 3 lands, the staleness line grows an "upgrade now" action.

## Limitations

- **Phase 2 is gated on FDR 0011 reaching accepted.** Until then, PTY-owning
  processes upgrade only by draining that session — the record makes the
  skip explicit (`posh upgrade` reports it) rather than pretending. The
  gate is deliberate: doing daemon handoff before the unification would
  mean designing it twice (once for each PTY-owner shape).
- **The Arch-A residual has no handoff.** `POSH_RELAY=0` rollback sessions
  and `--ephemeral` throwaways keep the drain-only story. If handoff for a
  crypto-owning Arch-A server is ever wanted, transplant no counters:
  reserve high nonce bits as a handoff epoch and bump it at cutover, so the
  successor cannot collide with any nonce the old server used.
- **The flag day is structural.** The old build must already speak the
  sender side of the cutover kernel; every fleet has one last drain when
  the mechanism first ships.
- **Mid-cutover skew is the normal state.** RFC 0001's ignore-unknown rule
  covers the wire, but behavioral skew (an old daemon lacking a new cap) is
  only *visible*, not prevented, until the fleet converges.
- **The relay/daemon pair cuts over in two steps.** A relay dies with its
  attach; upgrading the daemon under it is a separate Phase 2 handoff.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| Phase 2 mechanism | sync-as-client + cutover kernel | reuses attach/frame/dump machinery; rejectable via plain detach | the observer-attach or verify seam proves harder than a bespoke handoff channel |
| parentage sub-decision | undecided (exec-in-place vs reaper shim) | exec keeps parentage cleanly; the shim avoids exec's no-fallback flip | prototyping either |
| Phase 2 sequencing | after FDR 0011 accepted | one PTY-owner class, no crypto in the handoff | FDR 0011 stalls while daemon staleness hurts in practice |
| mux build-generation drain | not implemented | protocol stamp alone misses build-only changes | — |
| auto-cutover | off (explicit verb only) | handoff must soak before it fires unattended | Phase 2 soak time with zero lost sessions |

## More Information

- **posh#164** — the originating design issue this record promotes.
- **posh#161 / posh#162** — the mux instrumentation and reconnect this
  builds on; #162 is the re-establish seam Phase 1 generalizes.
- **RFC 0013** (`docs/rfcs/0013-server-introspection-caps.md`) and
  **RFC 0014** (`docs/rfcs/0014-client-introspection-caps.md`) — the
  visibility layer: build idents on both ends, the staleness trigger.
- **RFC 0001** (`docs/rfcs/0001-target-grammar-and-capability-table.md`) —
  the ignore-unknown-caps rule that tolerates mid-cutover skew.
- **FDR 0011** (`0011-unified-durable-sessions.md`) — the sequencing gate:
  the daemon owns the screen, `posh-server` becomes a disposable relay, and
  Phase 2 collapses to the session-daemon class.
- **RFC 0006** (`docs/rfcs/0006-diff-base-integrity.md`) — the checksum the
  successor's pre-cutover verify step reuses.
- **RFC 0009** (`docs/rfcs/0009-scrollback-stream-separation.md`) — the
  scrollback stream the sync-as-client bootstrap rides.
- **RFC 0003** (`docs/rfcs/0003-castx-recording-format.md`) / poshterity —
  prior art for serializing full terminal state deterministically;
  `dump_vt()` is the cutover kernel's authoritative-snapshot format.
