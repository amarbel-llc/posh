---
status: proposed
date: 2026-06-30
promotion-criteria: >
  proposed -> experimental: the daemon emits ServerFrames over its socket
  behind capability negotiation, a single unified client consumes frames over
  both the Unix and UDP transports, and the RFC 0008 conformance tests (socket
  skew matrix, reliable-as-degenerate property test) are green. The double-model
  inner-attach path still exists behind the POSH_FRAMESYNC rollback switch.
  experimental -> testing: `posh attach` (picker + host-scoped + attach-or-create),
  `posh list` (unified, worker-filtered, spinclass-styled), and the command
  palette on a local session are all in daily use on both fleet hosts.
  testing -> accepted: two weeks of daily cross-host use with no fallback to the
  Tag::Output / inner-attach path, and a spinclass/clown remote worker created
  and reattached through the unified interface.
---

# Unified durable sessions (one shape, attach anywhere)

## Problem Statement

posh has three session shapes — a local persistent session (`posh dev`), an
ephemeral roaming shell (`posh box`), and a persistent remote session
(`posh box:dev`) — and two screen-sync protocols. A session's reachability and
durability depend on where and how it was started: a roaming `box` shell dies
with its transport, and a remote `box:dev` runs a whole second `posh-server` +
PTY + terminal model on top of the session daemon (FDR 0001's "Architecture A",
the double model). The command palette and the rich frame protocol
(diff/morph/scrollback-sync) are remote-only, because the local client is a dumb
byte pipe with no client-side display model.

The intent is one shape: **every shell, on or off this host, is a first-class
durable session reached through one interface.** A session started on a laptop
is attachable from a desktop; a worker spawned on a remote host (the
spinclass/clown driver) is reachable and reattachable the same way. "Local" and
"remote" become *which transport you dial*, not different kinds of session.

## Interface

**The daemon owns the screen.** Every session is a durable daemon owning the
PTY and the `posh_term::Terminal`, and it produces one `ServerFrame` stream
(`Full`/`Diff`/`Morph`/`Scrollback`). A **local** client consumes that stream
over the Unix socket; a **remote** client consumes it over the AEAD-UDP roaming
transport through a thin, disposable relay (`posh-server`'s reduced role — no
second PTY, no second model). The reliable socket is the lossless degenerate of
the datagram protocol, so local attach gains morph, scrollback-sync, and the
command palette for free. See RFC 0008 for the wire contract.

**`posh attach` is the one entry point** (`posh <target>` remains sugar for it):

    posh attach box:dev          attach-or-create session dev on box
    posh attach :dev / dev       local session dev (attach-or-create)
    posh attach box:             host-only -> TTY picker scoped to box's sessions
    posh attach user@box:        same, explicit user
    posh attach :                local host -> picker scoped to local sessions
    posh attach                  no target -> picker over local + remote sessions
    posh attach --ephemeral box  opt into a non-durable throwaway shell

The no/partial-target picker is a TTY-gated charmbracelet list (the spinclass
`internal/sessionpick` pattern) offering existing sessions and "create new",
filterable, with each row labelled by the session's description. **When stdin is
not a TTY the picker never launches** — it errors with the candidate list so
scripts and command substitution stay deterministic.

**Everything is durable.** The nameless ephemeral roaming shell is removed; a
bare host (`posh box`) opens the host-scoped picker rather than spawning a
throwaway shell. `--ephemeral` is the explicit, documented opt-out (no daemon,
dies with the transport) and the rollback anchor.

**`posh list` is unified and spinclass-styled.** It lists local and remote
sessions and **hides remotely-spawned detached workers by default**; `--workers`
includes them (mirroring spinclass's `running-detached` filtering). Output
modes mirror `sc list`: tab-separated plain text for non-TTY/pipes, a styled
lipgloss table on an interactive TTY, `--format json`, and a live-refreshing
`--watch` view. Columns: session URI, status, last-activity, cwd, description.

**Sessions carry a settable description.** A short label (spinclass parity)
shown in both `list` and the picker, so an attach target reads as "deploy
headscale" rather than an opaque name.

**Reachability is symmetric and already-uniform underneath.** Because a
remotely-created session is a real daemon on its host, `posh list` *there* and
`posh list box:` *from elsewhere* enumerate it; a locally-created session is
reachable as `posh box:name` from anywhere. The unification removes the
double-model plumbing under that, it does not add a new addressing scheme.

## Examples

Start a session on a laptop, roam, pick it up from a desktop — and the palette
works on both, including when the desktop is the same host (local transport):

    laptop$ posh attach box:dev        # creates dev on box, attaches
    laptop$ Ctrl-^                     # command palette (now also local)
    laptop$ Ctrl-\                     # detach
    desktop$ posh attach box:dev       # same session, full replay

Pick a session interactively, scoped to a host:

    $ posh attach box:
    ┌ box sessions ───────────────────────────┐
    │ > dev      deploy headscale   2m ago     │
    │   ci       nightly build      idle       │
    │   + create new session…                  │
    └──────────────────────────────────────────┘

Spawn a spinclass/clown worker remotely, reattach later through one interface:

    $ posh attach -g spinclass box:w1 --detach -- worker --serve
    $ posh attach box:spinclass/w1     # reattach the running worker

List, with and without workers:

    $ posh list                        # human sessions, styled table on a TTY
    $ posh list --workers              # also the detached fleet workers
    $ posh list --format json | jq .   # machine-clean, no TTY styling

## Limitations

- **Agent forwarding on durable/local-origin sessions is out of scope** here.
  The relay-owned `agent/sock` symlink (FDR 0004) is unchanged and the daemon
  never brokers keys, but a session whose shell was spawned without a
  forwarding connection (local first-attach, or a `--detach` worker) does not
  pick up a later attach's forwarded agent, because `SSH_AUTH_SOCK` is bound
  once at spawn. The unification promotes this #53 edge case to the common
  case. Near-term workaround: `Setenv` IPC / `posh setenv` (#53). End-state: a
  host-global filesystem rendezvous, tracked as **#103**.
- **Local scrollback ownership moves from the outer terminal to posh.** Today a
  local session is a raw byte passthrough, so your terminal emulator's native
  scrollbar/search owns the session's history. Once the local client consumes
  `ServerFrame`s and repaints the visible screen in place (Phase 2, RFC 0008),
  the outer terminal no longer accumulates that history; local converges to the
  *same* posh-managed scrollback as remote — the daemon-authoritative ring
  synced via the `SCROLLBACK` capability (RFC 0002 / FDR 0005) and presented
  through posh's own scroll UI. Consistent across local and remote (the goal),
  but a real UX change for people who use a bare shell precisely for
  native-terminal scrollback. **Decided:** accept the convergence; a future
  where capable terminals integrate the session's scrollback into their *native*
  history (e.g. kitty, negotiated via the reserved `TERM_FEATURES` capability)
  is tracked as **#104**. This must be resolved in Phase 2 when the local client
  gains frame consumption — it does not affect Phase 1 (inert infrastructure).
- **The grammar revision changes two RFC 0001 forms.** `box:` was a plain
  roaming shell and `:` was `LocalSession{":"}`; both now open pickers. There
  is no flag day for the *wire* (negotiated), but the *CLI* meaning of these
  two forms changes; the explicit non-interactive path is `posh attach
  box:name`. Spelled out in RFC 0008's amendments.
- **`--ephemeral` may ship later.** The durable default is the feature; the
  explicit throwaway opt-out is small but deferrable if it slips the first cut.
- **No multi-host aggregated list in v1.** `posh list box:` queries one host;
  a `posh list --all` that fans out across a configured host set is future UX.
- **No auto-reaping.** Durable shells linger until their shell exits, as local
  sessions already do. An idle-timeout for abandoned shells is a tuning lever,
  not a v1 behavior.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| idle-reap timeout for abandoned durable shells | off | durable-by-default is the whole point; tmux/zmx also never auto-reap | default-shell sprawl across hosts becomes real clutter |
| `posh list` default worker filter | hide `detached` | a human list shouldn't drown in fleet workers | users routinely want workers and forget `--workers` |
| picker prior art | spinclass `sessionpick` / `clown resume` | proven TTY-gated filterable list, local+remote rows | picker UX needs diverge from spinclass's |

## More Information

- **RFC 0008** (`docs/rfcs/0008-unified-session-frame-transport.md`) — the
  normative protocol: ServerFrames over the socket, capability negotiation, the
  reliable-as-degenerate rule, the relay contract, and the RFC 0001 §1/§2
  amendments.
- **Design trail:** `docs/plans/2026-06-30-unified-session-transport-design.md`.
- **FDR 0001** (`0001-unified-host-session-namespace.md`) — the namespace and
  the Architecture A→B→C progression this realizes (B).
- **FDR 0010** (`0010-remote-detached-spawn.md`) — the `--detach` spawn the
  worker examples use.
- **FDR 0009 / FDR 0008** — the command palette and escape-to-shell that now
  generalize to local sessions.
- **FDR 0004** (`0004-ssh-agent-forwarding.md`) and **#103** — agent forwarding
  today and its host-global future.
- **RFC 0002 (scrollback sync) / FDR 0005 (client-side scrollback)** and **#104**
  — the posh-managed scrollback local converges to, and the outer-terminal-native
  (kitty) integration future.
- **#75** — the posh-proto extraction (`framereplay`, shared codecs) the daemon
  frame engine builds on.
- **FDR 0012** (`0012-session-layer-collapse.md`) — a capability this
  unification unlocks at its tail: once `posh-server` is the RFC 0008 §3 relay,
  a remote client can collapse into an inner local session (the clown/spinclass
  posh-in-posh case) by retargeting the relay, instead of nesting a second
  session. Blocked on this feature's relay; explores the layer-collapse UX.
