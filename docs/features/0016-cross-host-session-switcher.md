---
status: exploring
date: 2026-08-24
promotion-criteria: >
  exploring -> proposed: the switch mechanism (retarget vs. re-dial, and which
  applies same-host vs. cross-host) is decided, the palette is decoupled from the
  session-composite context so it can render as a top-level chooser, and the
  picker surface is shared with FDR 0015's `ph` (one implementation, two entry
  points). Blocked on FDR 0011's reachable-session listing and, for the retarget
  path, on FDR 0012's relay retarget being at least `experimental`.
  proposed -> experimental: switching between two open sessions on different
  hosts from within a running session, driven from the palette, with a `Full`
  keyframe repaint and no drop to the local shell.
---

# Cross-host session switcher (grow the palette into a picker)

## Problem Statement

Once sessions are durable and uniformly reachable (FDR 0011), and a terse
front-door exists to start/attach them (FDR 0015), the next gap is *switching*:
moving between open sessions — including across hosts — from **within** a running
session, without dropping to a shell to re-run `ph` and without nesting a second
posh layer (the FDR 0012 problem).

The command palette (FDR 0009) is the natural home, but today it is a fixed
escape menu (echo, logging, shell-out, suspend, quit) composited onto the live
session view. Two things it cannot yet do: (1) render as a **top-level chooser**
detached from any one session (the surface FDR 0015's bare `ph` / `ph host:`
forms need), and (2) enumerate and **switch to** an arbitrary reachable session
on any host. This record explores growing the palette into that picker/switcher.
It is deliberately deferred behind FDR 0011 and FDR 0015; it is recorded now to
fix the dependency and to hold the switch-mechanism fork.

## Interface (exploratory)

**The palette gains a "Switch session" mode**: the same reachable-session picker
that FDR 0015's bare `ph` opens, invocable live from within a session (`Ctrl-^`).
Rows are the FDR 0011 listing — labelled by activity label (RFC 0013 §5), host,
status, last-activity — filterable, with a "create new" row that routes to `posh
start`. Selecting a row switches the client's attachment to that session.

**Palette cleanup is a precondition.** `posh-palette` (the Go/Bubble Tea
renderer, RFC 0005) is today driven as a subprocess composited onto a session
`Snapshot`. To serve as a top-level chooser it must be decoupled from the
session-composite context and able to render standalone. This is the "clean up
the palette" work; the switcher and FDR 0015's TUI modes both consume the result.

### The switch mechanism (the open fork)

Switching the client's attachment to the selected session can be done two ways,
and they differ by whether the target is on the same host:

- **Retarget (FDR 0012).** Re-home the existing transport onto the selected
  session's daemon, reusing one connection; a `Full` keyframe repaints. This is
  exactly FDR 0012's relay-retarget mechanism. It is elegant but bounded by FDR
  0012: it is **same-host only** in v1 (the transport already reaches that host),
  and it requires the RFC 0008 §3 relay. It does **not** reach a *different*
  host.
- **Re-dial.** Tear down the current attach and establish a new one to the
  selected target: a fresh session channel on the target host's mux endpoint if
  one exists (cheap — the M2 wire is already up), else spin up that host's
  endpoint. Buildable without FDR 0012; costs a visible blip on switch; works
  **cross-host**, which is the whole point of this record.

**Recommended staging (to validate):** same-host switching via FDR 0012
retarget, cross-host switching via re-dial, with the palette UX unified so the
user never sees which mechanism fired. Whether that split is worth the two code
paths — versus re-dial everywhere for simplicity — is the central open question.

## Examples

    <Ctrl-^>                              # palette
    ┌ switch session ──────────────────────────────────────┐
    │ > cargo build        box     running   3s ago         │
    │   vim ~/notes        dev     idle      2m ago         │
    │   deploy headscale   web     running   just now       │
    │   + create new session…                               │
    └───────────────────────────────────────────────────────┘
    # select "vim ~/notes on flac": same-host retarget or cross-host
    # re-dial as appropriate; a Full keyframe repaints the new session.

## Limitations

- **Depends on FDR 0011, FDR 0015, FDR 0009, and (for retarget) FDR 0012.** The
  listing, the shared picker surface, the palette renderer, and the retarget
  mechanism are all prerequisites; this record is the UX that composes them.
- **Retarget inherits FDR 0012's blockers.** Same-host-only in v1, needs the
  relay at `experimental`+, and the agent-forwarding gap (#103) on the target
  session.
- **Re-dial's blip.** Cross-host switching via re-dial is not seamless — the old
  attach tears down and the new one establishes; the user sees a repaint gap.
  Acceptable for an explicit switch; not for an implicit one.
- **Palette decoupling is real work.** Making `posh-palette` render standalone
  (not composited on a session) touches the RFC 0005 control channel and the
  client's compositing path; it is the bulk of the near-term cost.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| same-host switch mechanism | FDR 0012 retarget | reuses one transport, no blip, matches the collapse story | the two-path split (retarget + re-dial) costs more than re-dial-everywhere saves |
| cross-host switch mechanism | re-dial | retarget cannot cross hosts (FDR 0012 §Limitations) | a future cross-host relay chain makes retarget reach |
| switch affordance | palette "Switch session" mode | one discoverable home, shared with `ph` | a dedicated keybind proves faster than the palette round-trip |

## More Information

- **FDR 0011** (`0011-unified-durable-sessions.md`) — the reachable-session
  listing and durable model the picker enumerates.
- **FDR 0015** (`0015-ph-front-door.md`) — the front-door whose bare / `host:`
  TUI modes open this same picker surface.
- **FDR 0012** (`0012-session-layer-collapse.md`) — the relay-retarget mechanism
  the same-host switch reuses; its constraints bound the retarget path.
- **FDR 0009** (`0009-command-palette.md`) — the palette this grows out of.
- **RFC 0005** (`docs/rfcs/0005-palette-control-protocol.md`) — the palette
  control channel the standalone-chooser decoupling touches.
- **RFC 0013 §5** (`docs/rfcs/0013-server-introspection-caps.md`) — the activity
  label the picker rows are keyed on.
- **RFC 0011** (`docs/rfcs/0011-multiplexed-datagram-channels.md`) — the M2 mux
  endpoints re-dial reuses/spins up per host.
