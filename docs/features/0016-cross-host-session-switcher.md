---
status: proposed
date: 2026-08-24
promotion-criteria: >
  exploring -> proposed (MET 2026-09-07): the switch mechanism is decided —
  v1 re-dials everywhere (the client returns a switch outcome and the front
  door re-attaches to the selected target; same-host and cross-host share one
  path), with the FDR 0012 same-host retarget recorded as the later
  optimization behind a viewport-initiated switch request — the picker is
  specified as an additive RFC 0005 view the renderer draws without knowing
  what a session is, and the picker surface is shared between FDR 0015's
  `ph` TUI modes and the in-session palette (one renderer, one row source,
  two hosts). FDR 0011's listing exists and FDR 0012 is experimental.
  proposed -> experimental: switching between two open sessions on different
  hosts from within a running session, driven from the palette, with a `Full`
  keyframe repaint and no drop to the local shell; and bare `ph` / `ph host:`
  open the same picker instead of erroring.
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
on any host. This record grows the palette into that picker/switcher, reusing
the palette's architecture wholesale: the same `posh-palette` renderer
subprocess, the same RFC 0005 control channel, the same compositing path.

## Interface

**One picker, two hosts.** The picker is a renderer *view* (RFC 0005 §3.5,
`ui.show view="picker"`): a titled, filterable table whose rows the client
supplies as opaque cells plus an action. The renderer knows nothing about
sessions; the client is the authority on the rows and on what selecting one
does. Two hosts drive it:

- **In-session:** the palette gains a **Switch session…** command. Choosing it
  lists the reachable sessions and re-shows the renderer in the picker view;
  selecting a row switches the viewport to that session, cancelling returns to
  the session untouched.
- **Top-level (`ph`, `ph host:`):** the front door hosts the renderer
  standalone — raw mode, the alternate screen, the renderer's screen composited
  onto a blank frame with the same compositor the in-session palette uses — and
  runs `posh attach` / `posh start` for the selection. Non-TTY invocations keep
  FDR 0011's discipline: they error with the candidate list, never launch a
  picker.

**Rows** are the FDR 0011 listing, one per session: the activity label (RFC
0013 §5; the launch command when no label is reported), the session id (so
same-label sessions stay tellable), the host (`local` for this machine), and
status (`detached`, `attached (n)`, or `stale (…)`). A last-activity age and
the FDR 0011 description join the
row once the session record carries them. A trailing **`+ create new session…`** row routes
to `posh start` on the row's host (the `:+` auto-id path). The top-level picker
lists local sessions plus every host that has a live mux endpoint (`posh mux
ls`) — the hosts a roamer is already connected to — and `ph host:` lists one
host; the in-session picker uses the same set, so a roamer can hop between
every host their mux daemons reach.

**Switching.** Selecting a row issues `session.switch {"target": …}` (RFC
0005 §7). The client answers, closes the palette, and **ends its attach with a
switch outcome** instead of an exit status; the front door then attaches to the
named target exactly as `ph <target>` would — a local socket, a mux session
channel, or a fresh bootstrap — and loops until an attach ends without a switch.

**Leaving a session.** FDR 0011 reaps nothing, so a switch is where a session
would otherwise pile up. Choosing a row from *inside* a session therefore asks
a second question, in the same renderer as a three-command palette:

- **Switch, keep it running** (the default, and what Enter on the first
  entry does) — the previous session stays detached.
- **Switch, kill it** — killed once the new attach is *established*, so a
  switch that fails to attach never destroys the session it was leaving.
  If other viewports are still attached to it, it is kept and the new
  session's banner says so.
- **Switch, kill it even with other viewports attached** — the forced
  form; those viewports are thrown out exactly as `posh kill` does.

**The title after a switch.** A viewport titles the outer terminal
`host:session` for any session that has set no title of its own (both
clients, on every compose whose model title is empty; a title the session
sets always wins). The paint rule leaves an *empty* title untouched on the
first frame by design (posh#108, so an attach does not reset an inherited
title), which would otherwise leave the previous session's title standing
after a switch into an untitled one — the default closes that gap.

The kill is the ordinary `posh kill` primitive on the session's host — the
local daemon socket, or `posh kill --unless-attached` over ssh for a remote
one (non-interactive; an auth prompt cannot be answered from inside a
session). The top-level `ph` picker, run from a plain shell, has no session to
leave and asks nothing.

### The switch mechanism (decided)

Two mechanisms were open (§Tuning Levers records both):

- **Retarget (FDR 0012).** Re-home the existing transport onto the selected
  session's daemon — no blip, one connection. FDR 0012 is experimental and its
  M2/relay re-home works, but its trigger is the **in-session** `posh attach
  <sibling>`, which sends the daemon a `SwitchRequest` from *inside* the
  session. A picker selection originates in the **viewport**, which has no
  daemon socket: driving the retarget from the palette needs a new
  client→bridge switch capability on the frame transport. It is also
  same-host only.
- **Re-dial.** Tear down the current attach and establish a new one to the
  selected target. Works cross-host — the whole point — and reuses the M2 wire
  when the target host already has a mux endpoint (a new session channel on an
  established connection, so the "blip" is one `Full` keyframe on a live
  connection, not an ssh bootstrap).

**v1 re-dials everywhere.** One code path serves same-host and cross-host, the
switch is an explicit user action for which a repaint gap is acceptable, and no
new wire capability is needed. The same-host retarget is the recorded
optimization: once a viewport-initiated switch request exists on the wire, the
front door can prefer it for a target on the current host and fall back to
re-dial. The UX is identical either way; the user never sees which fired.

### Staging

1. **Renderer + protocol** (landed 2026-09-07). The `picker` view in
   `posh-palette` and the `Palette::show_picker` host call (RFC 0005 §3.5, §7
   `session.list` / `session.switch`).
2. **Row source + top-level picker** (landed 2026-09-07). The `picker` module's
   row listing (local + live mux hosts, or one host) and the standalone chooser
   behind bare `ph` / `ph host:`.
3. **In-session switch** (implemented 2026-09-07, awaiting fleet verification
   for `experimental`). The palette's *Switch session…* command re-shows the
   renderer as the picker; a selection records the target
   (`picker::request_switch`) and ends the attach — quit on the roaming client,
   detach on the local one — and `run()`'s re-attach loop dispatches the target
   through `ph`'s routing. Both clients.
4. (Later) **Same-host retarget** behind a viewport switch capability.

## Examples

    <Ctrl-^>                              # palette
    › Switch session…
    ┌ switch session ──────────────────────────────────────┐
    │ / _                                                   │
    │ › cargo build        box     running   3s ago         │
    │   vim ~/notes        dev     idle      2m ago         │
    │   deploy headscale   web     running   just now       │
    │   + create new session…   local                       │
    └───────────────────────────────────────────────────────┘
    # select "vim ~/notes on dev": the attach ends with a switch outcome,
    # ph re-dials dev:s-2 (a channel on dev's mux endpoint if one is up),
    # a Full keyframe repaints; the old session keeps running detached.

    $ ph                                  # top-level: the same picker, all hosts
    $ ph box:                             # the same picker, one host

## Limitations

- **Depends on FDR 0011, FDR 0015, FDR 0009, and (for the retarget follow-on)
  FDR 0012.** The listing, the front door, the palette renderer, and the
  retarget mechanism are prerequisites; this record is the UX that composes
  them.
- **Re-dial's blip.** A switch tears down the old attach and establishes the
  new one; the user sees a repaint gap (short on an established mux wire,
  longer when a bootstrap is needed). Acceptable for an explicit switch; the
  same-host retarget follow-on removes it where it can.
- **Host set = mux endpoints.** The "all hosts" picker enumerates hosts with a
  live mux daemon, not every ssh-config or tailnet host — listing an arbitrary
  host costs an ssh round trip per host, and the reachable set a roamer wants
  is the one they are connected to. `ph host:` reaches any host explicitly.
- **Listing latency.** Rows come from `posh list --json` per host over ssh;
  the picker shows after every host answers (bounded by the ssh connect
  timeout). A live-updating picker is future UX.
- **The renderer owns the keyboard while the picker is up** (as for the
  palette); the session keeps running underneath.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| switch mechanism | re-dial everywhere | one path, cross-host, no new wire cap; the blip is one keyframe on an established mux wire | a viewport switch cap lands and same-host switches are frequent enough that the blip grates |
| same-host switch mechanism (follow-on) | FDR 0012 retarget | reuses one transport, no blip, matches the collapse story | the two-path split (retarget + re-dial) costs more than re-dial-everywhere saves |
| picker host set | local + live mux endpoints (`ph host:` = one host) | the connected set, no per-host ssh fan-out to cold hosts | users routinely want a cold host in the all-hosts picker (add the ssh-config/tailnet union behind a flag) |
| switch affordance | palette "Switch session…" | one discoverable home, shared with `ph` | a dedicated keybind proves faster than the palette round-trip |

## More Information

- **FDR 0011** (`0011-unified-durable-sessions.md`) — the reachable-session
  listing and durable model the picker enumerates.
- **FDR 0015** (`0015-ph-front-door.md`) — the front-door whose bare / `host:`
  TUI modes open this same picker surface and whose re-attach loop hosts the
  switch outcome.
- **FDR 0012** (`0012-session-layer-collapse.md`) — the relay-retarget mechanism
  the same-host follow-on reuses; its constraints bound the retarget path.
- **FDR 0009** (`0009-command-palette.md`) — the palette this grows out of.
- **RFC 0005** (`docs/rfcs/0005-palette-control-protocol.md`) — the palette
  control channel: §3.5 the `picker` view, §7 `session.switch`.
- **RFC 0013 §5** (`docs/rfcs/0013-server-introspection-caps.md`) — the activity
  label the picker rows are keyed on.
- **RFC 0011** (`docs/rfcs/0011-multiplexed-datagram-channels.md`) — the M2 mux
  endpoints re-dial reuses.
