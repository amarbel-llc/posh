---
status: proposed
date: 2026-09-14
promotion-criteria: >
  experimental once the foreground-attach path (Phase 1) takes over before the
  ssh bootstrap and answers a real host-key / password prompt inside the modal
  on a live remote (nikulin); testing once a week of daily remote attaches shows
  no establishment regressions (no lost keystrokes across the Phase A→B handoff,
  no stranded takeover on ssh failure) and the mux-endpoint bootstrap (Phase 2)
  carries the same modal; accepted once both paths are the default and the
  capture_stderr host-key swallow bug is closed.
---

# Interactive establish modal terminal (take over first, host ssh in the modal)

## Problem Statement

An attach to a remote session (`ph nikulin:+`, `posh host:session`) runs its ssh
bootstrap — DNS, TCP, auth, host-key approval, `posh-server` exec, the `POSH
CONNECT` handshake — entirely BEFORE posh takes over the terminal. The alt-screen
takeover and the posh#195 "establishing connection" modal both live inside
`drive_client`, which starts only after ssh has already returned `POSH CONNECT`.
So the longest and most interaction-prone phase of establishment is uncovered:
it runs raw on the primary screen, and an interactive prompt (host-key
acceptance on first connect, a password, a 2FA challenge) either appears
unframed on the primary screen or — on a bootstrap that captures ssh's stderr —
is swallowed while ssh blocks on input the user cannot see. The takeover is not
"immediate" from the user's point of view; it is immediate only relative to the
roaming client, which is the wrong reference point.

## Interface

The observable behavior for a remote attach:

- **Takeover is the first thing that happens.** On `ph host:session` /
  `posh host:session`, posh switches to the alt screen (smcup) BEFORE the ssh
  bootstrap runs, not after. The outer terminal is posh's from the moment the
  command is invoked.
- **A single modal spans the whole establishment.** A command-palette-style
  modal (greyed backdrop, centred) is composited onto the empty viewport for the
  entire establishment, not just the roaming-handshake tail. It is dismissed on
  the first server frame; on failure it shows the reason and the process exits
  with it.
- **The modal is an interactive terminal during the ssh phase.** While the ssh
  bootstrap runs, its live output — the command, connection progress, and any
  prompt — is shown inside the modal, and the user's keystrokes are routed into
  ssh. A first-connect host-key `yes/no`, a password, or a 2FA code is typed and
  answered inside the modal. Once ssh reports `POSH CONNECT` (and exits, as it
  does today — the server has detached), the modal switches to the roaming
  "establishing connection" progress it shows now, and keystrokes switch to the
  session.
- **No ssh, no interactive phase.** When the attach rides an already-warm mux
  endpoint (the common `POSH_MUX_SESSIONS` default — no ssh is run) the modal
  opens directly in progress mode and dismisses on the first frame, exactly as
  today. The interactive phase exists only when a bootstrap ssh is actually
  spawned.
- **Off-tty / renderer-absent degrades quietly.** When stdout is not a TTY or
  the modal renderer is unavailable, there is no overlay (as today): posh takes
  over immediately with no modal and the transport-agnostic "Last contact"
  banner covers the gap, and the ssh bootstrap falls back to inherited stdio.

Nothing about the target grammar, the wire, or non-interactive paths changes.
The picker's remote kills and other `BatchMode=yes` ssh invocations stay
non-interactive; the interactive modal is only for the foreground attach
bootstrap.

## Examples

First connect to a host whose key is not yet trusted:

    $ ph nikulin:+
    # alt screen taken over immediately; modal centred on the greyed viewport:
    ┌─ establishing nikulin:+ ─────────────────────────────┐
    │ ssh nikulin posh-server new …                        │
    │ The authenticity of host 'nikulin' can't be          │
    │ established. ED25519 key fingerprint is SHA256:…      │
    │ Are you sure you want to continue connecting          │
    │ (yes/no/[fingerprint])? ▊                             │
    └───────────────────────────────────────────────────────┘
    # you type `yes` INSIDE the modal; ssh continues, prints POSH CONNECT and
    # exits; the modal switches to "connecting to nikulin:+…" and, on the first
    # frame, is dismissed onto the live session.

Warm mux endpoint (no ssh):

    $ ph nikulin:dev
    # takeover + progress modal, dismissed on first frame — no interactive phase.

Failure surfaces in the modal, then on stderr after restore:

    $ ph nikulin:+
    # modal shows: establish connection not ok — Permission denied (publickey)
    # rmcup; the reason is printed on stderr and the process exits non-zero.

## Design

The command palette (`posh-palette`) and today's establish modal (`CrapModal`)
are the same mechanism: a subprocess on a PTY whose output is emulated into a
`posh_term::Terminal` and composited onto the greyed viewport with
`composite_palette`. `CrapModal` already implements spawn / `pump` / `screen` /
`resize` / `teardown`. The only missing capability is routing the user's
keystrokes INTO the hosted child.

The feature generalizes `CrapModal` into a `ModalTerminal` that hosts EITHER:

- an **ndjson-crap progress renderer** (`crap-present`, spawned via
  `pty::spawn_capture` with stdin = a data pipe) — today's establish/verdict UX;
  or
- an **interactive subprocess on a full PTY** (the bootstrap ssh, spawned like
  `pty::spawn_shell` so the child's controlling terminal IS the PTY and prompts
  land there), with the user's stdin forwarded to the PTY master and the raw PTY
  byte stream scraped for the `POSH IP` / agent-export ack / `POSH CONNECT`
  handshake lines (`sshwrap::ServerReport::feed` already parses them).

The establishment becomes one modal across two phases sharing the same
`ModalTerminal`:

1. **Takeover + modal up** at the top of the remote-attach path
   (`cmd_ssh_session`), before any ssh — smcup moves UP out of `drive_client`.
2. **Phase A (ssh bootstrap):** `sshwrap::bootstrap` changes from a blocking
   `BufRead`-to-CONNECT subroutine into an event-loop phase co-driving STDIN, the
   ssh PTY master, and the modal — ssh output emulated into the modal, STDIN
   written to the ssh PTY, the stream scraped for the handshake. Resolves to
   `(host, port, key)` or a failure reason. Skipped entirely when no ssh runs
   (warm mux endpoint).
3. **Phase B (roaming establish):** the resolved transport is stood up
   (`drive_client` / `run_over_mux`) WITHOUT re-smcup — the terminal is already
   ours — and the SAME modal keeps compositing in progress mode until the first
   frame. STDIN routes to the predictor/session from here.
4. **Dismiss** on the first frame (`ok`); **failure** at any phase shows
   `not_ok(reason)`, tears the modal down, rmcups, and returns the reason.

The ssh PTY keeps a normal cooked discipline (not `quiet_emulator_slave`): a
host-key `yes/no` wants echo + canonical line editing, and ssh turns echo off
itself for a password.

## Limitations

- **Phase 1 is the foreground attach only** (`sshwrap::run` from
  `cmd_ssh_session`). The per-destination mux-endpoint bootstrap (`ensure_mux`,
  which spawns ssh as a background daemon and captures its stderr) and the
  `--detach` remote spawn (`run_detached`) are Phase 2. Until then, a host-key
  prompt on the `capture_stderr` mux-endpoint path is still swallowed into the
  log while ssh blocks — a separate bug to close under this umbrella.
- **`BatchMode=yes` paths stay non-interactive** by design (the picker's remote
  kills, endpoint probes). The interactive modal never applies to them; a prompt
  there still fails fast rather than blocking.
- **Off-tty has no interactive modal.** With no viewport to type into, the ssh
  bootstrap degrades to inherited stdio (today's behavior). Scripted/headless
  attaches are unaffected.
- **The modal hosts one subprocess at a time.** ssh runs to `POSH CONNECT` and
  exits before Phase B; posh does not keep an interactive ssh alive underneath a
  live session.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| Phase A ceiling | ssh's own `ConnectTimeout` + a modal-level cap | bound a wedged bootstrap without cutting off a human mid-prompt | users hit the cap while legitimately answering a slow 2FA challenge |
| interactive-modal mechanism | generalize the Rust `CrapModal` → `ModalTerminal` | reuses the built compositing + PTY emulation; no Go/JSON-RPC round-tripping of raw terminal bytes | a need for the Go `posh-palette` renderer to own the terminal pane (RFC 0005 view) emerges |

## More Information

- Reshapes the posh#195 connect-establishing modal (immediate takeover +
  crap-present overlay): that modal moves earlier and gains an interactive ssh
  phase.
- Extends FDR 0009 (command palette) — reuses the captured-PTY-composited-modal
  pattern (`composite_palette`) — and relates to FDR 0007 (transport
  diagnostics) and FDR 0011 (unified durable sessions, the remote attach path).
- Touches `remote::connect_progress` (`CrapModal` → `ModalTerminal`),
  `remote::sshwrap` (`bootstrap` → event-loop phase), `remote::client`
  (`drive_client`/`run_over_mux` accept a pre-existing modal, drop their own
  smcup), `main::cmd_ssh_session` (takeover moved up), and `pty` (a full-PTY ssh
  spawn alongside `spawn_capture`).
- If the mechanism decision lands on a `posh-palette` terminal-pane view instead
  of the Rust `ModalTerminal`, RFC 0005 (palette control protocol) gains a new
  view type and this FDR's Design section is revised accordingly.
