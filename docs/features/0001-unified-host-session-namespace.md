---
status: proposed
date: 2026-06-11
promotion-criteria: >
  proposed -> experimental: the v1 implementation (typed parser, transport
  composition, EXIT_STATUS capability, completion) lands with the RFC 0001
  conformance tests green. experimental -> testing: both fleet hosts run a
  capability-table build and `posh box:dev` is in daily use.
  testing -> accepted: two weeks of daily cross-host use with no grammar
  surprises and no tuning-lever adjustments.
---

# Unified host:session namespace

## Problem Statement

posh has two attach worlds with two interfaces: local persistent sessions
(`posh dev`) and mosh-style remote shells (`posh box`), and no way to
combine their defining strengths — a *persistent named session* on
another machine, reachable over the *roaming* transport, detachable from
one machine and reattachable from another. Reaching one today requires
manually composing `posh ssh box -- posh attach dev` and knowing why that
works.

## Interface

One scp-style namespace covers everything attachable. A bare first
argument resolves per RFC 0001's total grammar:

    posh dev                  # local session, attach-or-create (unchanged)
    posh :dev                 # explicit local; :grp/dev addresses group grp
    posh box                  # plain roaming remote shell (unchanged)
    posh box:dev              # session dev on box: attach-or-create,
                              # transported over roaming UDP
    posh user@box:grp/dev     # full form
    posh [fe80::1]:dev        # bracketed IPv6 host
    posh list box:            # remote listing, names printed host-prefixed
    posh box:<Tab>            # completes remote session names (cached
                              # ssh query); local Tab offers sessions,
                              # ssh aliases, and subcommands together

Behavioral contract:

- **Attach-or-create symmetry**: `box:dev` creates the session on box if
  absent, exactly like local `posh dev`.
- **Persistence lives in the session daemon** on the remote host. The
  transport pair (`posh-server` + inner `posh attach`) is ephemeral; one
  pair per roaming client; concurrent attaches from several machines all
  multiplex through the daemon.
- **Both exits work and both leave the session running**: `Ctrl-\`
  detaches the inner attach through the transport; `Ctrl-^ .` quits the
  transport and the inner attach detaches on hangup. Network loss roams
  and reconnects, mosh-style.
- **Exit status is end-to-end**: when the remote session's shell exits,
  `posh box:dev; echo $?` reports its shell-style code, negotiated over
  the wire via the EXIT_STATUS capability (RFC 0001 §3) so mixed-version
  peers degrade to today's behavior instead of breaking.
- Explicit forms (`posh attach`, `posh ssh`) remain available and
  unchanged — they are both the escape hatches for names the grammar
  cannot express (sessions containing `/` or `:`) and the permanent
  rollback path.

## Examples

Start a session on box from a laptop, roam, and pick it up from a desktop:

    laptop$ posh box:dev          # creates dev on box, attaches
    laptop$ ... laptop sleeps, network changes — session keeps running ...
    laptop$ Ctrl-\                # detach; back at the laptop shell
    desktop$ posh box:dev         # same session, full replay

Exit status propagates:

    $ posh box:ci                 # session whose command exits 3
    $ echo $?
    3

Remote listing and completion:

    $ posh list box:
    box:dev
    box:grp/scratch
    $ posh box:<Tab>
    box:dev    box:grp/scratch

## Limitations

- **v1 is transport composition** (architecture A): each roaming client
  re-models the terminal twice (session daemon's model feeds a pty that
  the transport server re-models). Interactively invisible, but it is
  deliberate overhead. Natural progressions, in order:
  - **B — native session transport**: `posh-server` gains a mode that
    connects to the session's unix socket and proxies IPC frames over
    UDP directly, eliminating the double model and the inner pty. The
    RFC's grammar and remote-command contract are designed to survive
    this swap unchanged.
  - **C — daemon-native roaming**: session daemons own UDP listeners,
    removing the per-attach server entirely; requires per-session
    key/port management and enlarges every daemon's security surface.
    Only worth it if B's proxy hop ever matters.
- Session names containing `/` or `:` are not addressable in namespace
  form; use `posh attach` (documented in RFC 0001 §1).
- `box:` is a plain remote shell, not a "default session" — by design,
  so the grammar has no special cases.
- Group-qualified completion (`box:grp/<Tab>`) is deferred; remote
  listing crosses only the default group in v1.
- Completion requires non-interactive ssh to the host (BatchMode); hosts
  needing interactive auth complete from the cache only.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| completion ssh ConnectTimeout | 2s | bound a dead host's Tab stall | latency complaints on slow links |
| completion cache TTL | 30s | repeated Tabs instant; staleness window small | stale names surprise users, or hosts hammered |
| capability table in every datagram | yes | connectionless/idempotent, no handshake state | TERM_FEATURES-scale payloads make overhead visible → send-until-first-ack |

## More Information

- RFC 0001 (`docs/rfcs/0001-target-grammar-and-capability-table.md`) —
  the normative grammar, remote command contract, and capability
  registry.
- Design trail: `docs/plans/2026-06-11-host-session-namespace-design.md`.
- ADR 0001 (`docs/decisions/`) — platform constraints on the transport.
- github #37 (part 2), which this feature resolves; #18 (exit status,
  local path), #14 (signal handling the teardown paths rely on).
