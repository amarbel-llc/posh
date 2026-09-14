---
status: experimental
date: 2026-09-14
promotion-criteria: >
  testing once a week of daily remote attaches shows no establishment
  regressions — no lost keystrokes across the ssh→roaming hand-off, no stranded
  alt screen on a failed or interrupted bootstrap, every fallback warning
  readable on the primary screen afterwards — and a real first-connect host-key
  prompt has been answered inside the modal on a live remote through the
  seeded endpoint path (no fallback warning); accepted once that has held for
  a further week with no lever adjustments.
---

# Interactive establish modal terminal (take over first, host ssh in the modal)

## Problem Statement

Before this feature an attach to a remote session (`ph host:+`,
`posh host:session`) ran its ssh bootstrap — DNS, TCP, auth, host-key approval,
`posh-server` exec, the `POSH CONNECT` handshake — entirely BEFORE posh took
over the terminal: the alt-screen takeover and the posh#195 "establishing
connection" modal both lived inside the roaming client, which starts only after
ssh has returned `POSH CONNECT`. So the longest and most interaction-prone phase
of establishment was uncovered: it ran raw on the primary screen, and an
interactive prompt (host-key acceptance on first connect, a password, a 2FA
challenge) appeared unframed there. Worse, the common first hop is the
per-destination mux endpoint, whose detached bootstrap has no tty at all — ssh
cannot ask, that bootstrap fails, and the attach falls back to the foreground
path, which is where the prompt finally showed up, after two fallback warnings.
The takeover was "immediate" only relative to the roaming client, which is the
wrong reference point.

## Interface

The observable behavior for a remote attach on a terminal:

- **Takeover is the first thing that happens.** On `ph host:session` /
  `posh host:session` / `posh start --ephemeral host`, posh switches to the alt
  screen before the mux endpoint is ensured and before any ssh. The outer
  terminal is posh's from the moment the command is invoked; nothing typed
  meanwhile echoes over it.
- **A single modal spans the whole establishment.** A command-palette-style
  modal (greyed backdrop, centred) is composited onto the empty viewport for the
  entire establishment — the endpoint ensure, the session open, the ssh
  bootstrap, the roaming connect — and dismissed on the first server frame. On
  failure the reason is printed on stderr after the terminal is restored and
  the process exits with it.
- **The modal is an interactive terminal during the ssh phase.** While the
  bootstrap ssh runs, its live output — the tailnet dial notice, connection
  progress, any prompt — is shown inside the modal, and keystrokes are routed
  into ssh. A first-connect host-key `yes/no`, a password, or a 2FA code is
  typed and answered inside the modal; Ctrl-C interrupts ssh. Once ssh reports
  `POSH CONNECT` (and exits, as it does today — the server has detached) the
  modal returns to the "establishing connection" progress it shows for the
  roaming connect, and keystrokes go to the session from the first frame on.
  The `POSH CONNECT` line, and the session key on it, are never rendered.
- **No ssh, no interactive phase.** When the attach rides an already-warm mux
  endpoint (the `POSH_MUX_SESSIONS` default — no ssh is run) the modal shows
  progress only and dismisses on the first frame. A COLD endpoint's bootstrap
  ssh (`posh-server agent`) runs in the modal too (posh#198): its prompt is
  answered there, once, and the endpoint daemon is seeded with the resulting
  connection, so a first connect never needs the foreground fallback to
  answer a prompt. If that bootstrap fails, the attach falls back to the
  foreground ssh inside the same takeover, exactly as any endpoint failure.
- **Messages wait for the terminal.** Anything posh would have printed to
  stderr while the alt screen is up (a mux fallback warning, an unacknowledged
  agent-export warning, the client's own exit line) is captured and replayed on
  the primary screen once the terminal is restored — where it used to print
  before the takeover moved up.
- **Off-tty degrades to the old shape.** When stdout is not a terminal there is
  no takeover: the ssh bootstrap runs with inherited standard streams and the
  client takes over only once connected, exactly as before. When the progress
  renderer (`crap-present`) is unavailable the progress panels are absent but
  the ssh phase still shows.

Nothing about the target grammar, the wire, or non-interactive paths changes.
The picker's remote kills and other `BatchMode=yes` ssh invocations stay
non-interactive; the interactive modal is only for the foreground attach
bootstrap.

## Examples

First connect to a host whose key is not yet trusted (endpoint gated off, or
its bootstrap having failed at the same prompt):

    $ ph nikulin:+
    # alt screen taken over at once; the modal on the greyed viewport shows
    # ssh's own terminal:
    #
    #   The authenticity of host 'nikulin' can't be established.
    #   ED25519 key fingerprint is SHA256:….
    #   Are you sure you want to continue connecting (yes/no/[fingerprint])? yes
    #   Warning: Permanently added 'nikulin' (ED25519) to the list of known hosts.
    #
    # `yes` is typed INSIDE the modal (echoed by ssh's tty, as in a plain ssh);
    # ssh continues, prints POSH CONNECT (scraped, never shown) and exits; the
    # modal shows "establishing nikulin:+" and, on the first frame, is
    # dismissed onto the live session.

Warm mux endpoint (no ssh):

    $ ph nikulin:dev
    # takeover + progress modal, dismissed on first frame — no interactive phase.

Failure surfaces after the terminal is restored:

    $ ph nikulin:+
    # ssh: "me@nikulin: Permission denied (publickey)." shown in the modal as
    # ssh prints it; ssh exits; rmcup; then on the primary screen:
    posh: did not find posh server startup message (is posh-server on the
    server's non-interactive PATH?); ssh: me@nikulin: Permission denied (publickey).

Hand-check without a remote: `just debug-verify-establish-modal` drives the
whole flow in a tmux pane against a stub `ssh` that prompts on its tty and then
runs the server locally.

## Design

The command palette (`posh-palette`) and the posh#195 establish modal
(`CrapModal`) were already the same mechanism: a subprocess on a PTY whose
output is emulated into a `posh_term::Terminal` and composited onto the greyed
viewport with `composite_palette`. The feature adds the one missing capability
— routing the user's keystrokes INTO a hosted child — and moves the takeover
above the bootstrap. Three pieces, in `remote::connect_progress`:

- **`Takeover`** — begun at the top of the remote-attach entry points
  (`cmd_ssh_session`, `cmd_ssh`), before the mux endpoint ensure. Raw mode,
  smcup, and a stderr capture (fd 2 is dup'd onto an anonymous file; the real
  stderr is restored and the capture replayed when the takeover drops, after
  rmcup). It carries the progress modal between phases and owns a small
  differential painter for the phases no client loop is running. Returns
  `None` off a tty.
- **`CrapModal`** — the progress modal, unchanged in mechanism (`crap-present`
  via `pty::spawn_capture`, stdin = ndjson pipe, stdout = captured PTY).
- **`SshModal`** — the interactive modal: the bootstrap ssh spawned with
  `pty::spawn_shell` on a full PTY that is its controlling terminal, with a
  normal cooked+echo discipline (not the `quiet_emulator_slave` hardening the
  captured renderers get: a host-key `yes/no` wants echo and line editing, and
  ssh turns echo off itself for a password). Keystrokes are written to the
  master; Ctrl-C therefore reaches ssh as SIGINT through the slave's ISIG. The
  PTY byte stream passes through `sshwrap::LineScraper`, a byte-fed
  (ADR-0003) splitter that withholds a line only while it could still be a
  `POSH …` protocol line and consumes such lines whole into the
  `ServerReport`; everything else — a prompt with no trailing newline included
  — reaches the emulated screen the moment it is disambiguated.

The establishment is one takeover across the phases:

1. **Takeover + progress modal** at the top of the attach path.
2. **Endpoint ensure / session open.** A cold endpoint (its socket not
   connectable) first runs the endpoint's bootstrap ssh in the interactive
   modal (`mux::seed_cold_endpoint`, posh#198) and hands the report to
   `ensure_mux` as a `SeededEndpoint`; the daemon's FIRST establish connects to
   that already-bootstrapped remote (the key rides in memory across the double
   fork), and its reconnects bootstrap detached as before, trust now existing.
   A warm endpoint runs no ssh. Either way the takeover is handed to the
   client loop (`Takeover::handoff` → `Handoff`: the progress modal plus the
   painter's last frame, so the client's first paint diffs against what the
   takeover drew). A fallback keeps the takeover and re-raises the progress
   modal for the next attempt.
3. **Phase A (ssh bootstrap):** `sshwrap::bootstrap_in_modal` retires the
   progress modal, spawns the `SshModal`, and runs an event loop over STDIN and
   the ssh master — forwarding input, pumping output, painting on change,
   honoring SIGWINCH and the terminating-signal flag — until ssh exits (on its
   own after `POSH CONNECT`, bounded by a short grace) or is aborted. Resolves
   to the report or to the failure, whose message carries the last lines of the
   modal's text (what ssh itself said) exactly as the piped path carries a
   captured stderr tail.
4. **Phase B (roaming establish):** `sshwrap::run` hands the takeover to
   `client::run` (a fresh progress modal). `drive_client` with an inherited
   takeover neither smcups nor rmcups and composites the supplied modal until
   the first frame; without one (the `posh client` entry, off-tty) it takes
   over itself as before.
5. **Dismiss** on the first frame (`ok`); **failure** at any phase shows
   `not_ok(reason)` and returns the reason; the takeover's drop restores the
   terminal and replays the captured stderr, then the front door prints the
   error.

A cold endpoint spawn double-forks a daemon while the takeover is live; the
grandchild closes every inherited descriptor but its listener
(`util::close_inherited_fds`) so it never pins the modal's ndjson pipe (whose
EOF `crap-present` waits on) or its PTY.

## Limitations

- **Only the FIRST connect of an endpoint is interactive.** The daemon's own
  reconnects (`establish_wire` after a dead-wire verdict) are detached, with
  no tty; a prompt there — a host key that CHANGED after trust was
  established — fails the reconnect, and the endpoint keeps retrying until an
  invocation's foreground ssh answers it. The `--detach` remote spawn
  (`run_detached`) is untouched too. A seeded bootstrap whose daemon spawn
  loses the bind race leaves its remote `posh-server agent` to time out on its
  own.
- **`BatchMode=yes` paths stay non-interactive** by design (the picker's remote
  kills, endpoint probes). A prompt there still fails fast rather than blocking.
- **Off-tty has no takeover at all.** With no viewport to type into, the ssh
  bootstrap runs with inherited stdio (the old behavior). Scripted/headless
  attaches are unaffected.
- **The modal hosts one subprocess at a time.** ssh runs to `POSH CONNECT` and
  exits before Phase B; posh does not keep an interactive ssh alive underneath
  a live session.
- **A blocking step freezes the spinner (posh#199).** The endpoint's socket
  connect, spawn, and hello, and the session open, block the foreground
  process, so the progress modal's animation stands still until the client
  loop starts pumping it; the header is rendered before the step begins so the
  screen is never empty. A terminating signal during such a step is noted and
  acted on when the next loop runs. The long piece — a cold endpoint's ssh —
  no longer blocks (it runs in the modal, posh#198).
- **Type-ahead reaches the session, not ssh.** Keys typed before the ssh
  phase are flushed as it starts (posh#201), so a prompt is never answered by
  stale input; keys typed after the hand-off are the session's (mosh parity).

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| Phase A ceiling | none beyond ssh's own connect/auth timeouts; Ctrl-C in the modal aborts | a human answering a slow 2FA challenge must not be cut off; today's blocking bootstrap had no cap either | a wedged bootstrap that ssh itself never times out is reported |
| ssh exit grace after `POSH CONNECT` (`SSH_EXIT_GRACE`) | 3 s, then SIGKILL + reap | ssh exits on its own once the detached server closes its end; the bound keeps a lingering channel from delaying the roaming connect | reaps observed in the wild (ssh regularly needing the full grace) |
| progress-modal prime wait (`PRIME_WAIT`) | 150 ms | long enough for `crap-present` to render its header before a blocking step hides the loop, short enough to be invisible | an empty modal seen at the start of a cold endpoint ensure |
| interactive-modal mechanism | the Rust `SshModal` beside `CrapModal` | reuses the built compositing + PTY emulation; no Go/JSON-RPC round-tripping of raw terminal bytes | a need for the Go `posh-palette` renderer to own the terminal pane (RFC 0005 view) emerges |

## More Information

- Reshapes the posh#195 connect-establishing modal (immediate takeover +
  crap-present overlay): the takeover moves to the front door and the modal
  gains an interactive ssh phase.
- Extends FDR 0009 (command palette) — reuses the captured-PTY-composited-modal
  pattern (`composite_palette`) — and relates to FDR 0007 (transport
  diagnostics) and FDR 0011 (unified durable sessions, the remote attach path).
  ADR-0003 (byte-fed stream parsing) governs the handshake scraper.
- Code: `remote::connect_progress` (`Takeover`, `Handoff`, `CrapModal`,
  `SshModal`), `remote::sshwrap` (`ssh_argv`, `LineScraper`, `bootstrap` →
  `bootstrap_in_modal` / `bootstrap_piped`, `run`), `remote::client`
  (`run` / `run_over_mux` / `client_loop` accept a `Handoff`; `drive_client`
  takes `inherited`), `main::cmd_ssh_session` / `cmd_ssh` (takeover first),
  `remote::mux::run_daemon` + `util::close_inherited_fds`.
- If the mechanism decision ever lands on a `posh-palette` terminal-pane view
  instead, RFC 0005 (palette control protocol) gains a new view type and the
  Design section is revised accordingly.
