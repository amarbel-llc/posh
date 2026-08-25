---
status: proposed
date: 2026-08-25
promotion-criteria: >
  proposed -> experimental: every non-PTY-owning process class (agent
  endpoints, mux daemons, relays) cuts over by orderly termination +
  re-establish with no session loss, driven by a `posh upgrade` verb that
  detects staleness via the RFC 0013 idents; the PTY-owner handoff mechanism
  (in-place re-exec vs spawn-and-handoff) is decided and its state-blob
  format has an accepted RFC with an explicit N-generations-back
  compatibility contract.
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
   the session AEAD key. Hard.
6. **Session daemons** (`session/daemon.rs`) — own the shell's PTY and
   process group; killing one kills the user's shell. Hardest.

## Design

Phased by the inventory: generalize what already works, put the PTY owners
behind a state-blob contract, then surface the actuator.

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

### Phase 2 — PTY-owner handoff (classes 5–6, the open fork)

Two mechanisms, both passing live fds (PTY master, listener/UDP sockets, IPC
conns) plus a serialized state blob (terminal dump, scrollback ring, client
table, crypto key, counters):

- **A. In-place re-exec (nginx/systemd style).** The process detects (or is
  told about) a newer installed build and `exec()`s it, passing fds by
  inheritance and the blob via an inherited fd or handoff file. No socket
  rebinding; sessions never observe a gap. But a refused blob has no
  fallback — the old image is gone.
- **B. Spawn-and-handoff.** The old process spawns the new binary, passes
  fds over `SCM_RIGHTS` plus the blob over a unix socketpair, and exits once
  the successor acks. The two versions coexist briefly; the handoff can be
  rejected and rolled back (the old build keeps serving if the new one
  refuses the blob). More moving parts, safer failure mode.

Either way, the **state-blob format is its own RFC**, and its
compatibility contract — how many generations back a new build MUST accept —
is the load-bearing design decision. The blob rides the building blocks that
already exist: `posh_term::dump_vt()` reconstructs a `Terminal` from bytes,
and poshterity round-trips full sessions, so the hard state in a PTY-owning
process is largely already exportable.

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

- **PTY owners wait on the state-blob RFC.** Until Phase 2, roaming servers
  and session daemons upgrade only by draining that session — the record
  makes the skip explicit (`posh upgrade` reports it) rather than pretending.
- **Compatibility is bounded.** The blob contract will name a finite
  generations-back window; a process stale beyond it drains rather than
  hands off.
- **Mid-cutover skew is the normal state.** RFC 0001's ignore-unknown rule
  covers the wire, but behavioral skew (an old daemon lacking a new cap) is
  only *visible*, not prevented, until the fleet converges.
- **The relay/daemon pair cuts over in two steps.** A relay dies with its
  attach; upgrading the daemon under it is a separate Phase 2 handoff.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| PTY-owner mechanism | undecided (A vs B) | B's rejectable handoff is safer; A is simpler | prototyping shows B's coexistence window causes fd/ownership races |
| blob compatibility window | undecided (RFC-to-be) | bounded window keeps the codec auditable | fleets in practice hold builds older than the window |
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
- **RFC 0003** (`docs/rfcs/0003-castx-recording-format.md`) / poshterity —
  prior art for serializing full terminal state deterministically.
