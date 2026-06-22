---
status: experimental
date: 2026-06-22
---

# Command palette (Ctrl-^)

## Problem Statement

posh's roaming client accumulates client-local controls — the predictive-echo
model, debug logging, escape-to-shell, suspend, quit — that were each reachable
only as a single-key escape chord (`Ctrl-^` then a sub-key) with no
discoverability: you had to already know the keys, and every new control grew an
already-cryptic banner. A command palette makes the whole control surface
visible and filterable, and gives future controls a home without expanding the
chord table.

The palette is also a deliberate architecture seam. Rather than build the TUI in
Rust, posh hosts a charmbracelet (bubbletea) renderer as a subprocess and drives
it over a JSON-RPC control channel — decoupling the UI (frontend) from the
client's behavior (backend) and reusing mature TUI prior art.

## Interface

- **Trigger:** `Ctrl-^` (the mosh escape key, `0x1e`) opens the palette
  directly — there is no key-prefix menu. The client reads raw stdin and decides
  what to forward, so the inner app's `-isig` is irrelevant (the same
  interception point the old escape menu used).
- **Commands:** the palette *is* the escape menu. Version 1 lists:
  - **Echo: adaptive / optimistic / always / never** — set the predictive-echo
    model live (`echo.set`), overriding `$POSH_PREDICTION_MODEL` for the session.
  - **Debug logging: on/off** — toggle client debug logging (`logging.set`); the
    label reflects the current state and the banner shows the log path.
  - **Shell out (server)** — open the server-side escape-to-shell overlay in the
    session cwd (`shell.open`; FDR 0008's `CLIENT_FLAG_ESCAPE`).
  - **Suspend client** — job-control `SIGSTOP` the client (`client.suspend`); the
    remote session keeps running.
  - **Quit session** — quit the client (`app.quit`).
- **Navigation:** type to fuzzy-filter, up/down to choose, Enter to run, Esc to
  dismiss. The session greys behind the palette, which anchors a third of the way
  down, centered, in a yellow double border.
- **Fallback:** if the `posh-palette` renderer cannot be launched (binary missing
  or wedged), `Ctrl-^` instead arms a one-shot emergency prefix — `Ctrl-^ .`
  quits and `Ctrl-^ ^` (or a second `Ctrl-^`) sends a literal `Ctrl-^`. These are
  the only surviving hardcoded chords.
- **Rendering:** the client spawns `posh-palette` on a PTY plus a control
  `socketpair` (the child's fd 3), tracks its emulated screen with a
  `posh_term::Terminal`, and composites the non-blank region onto the session
  `Snapshot` each frame (the same path predicted echo and the status banner use).
  The renderer stays resident between summons.
- **Binary location:** `$POSH_PALETTE` override, else next to the running
  executable (the poshToolset co-installs `posh` and `posh-palette` side by
  side), else the first match on `$PATH`.

## Examples

Over a roaming session, press `Ctrl-^`. The palette opens; type `opt`, Enter —
predictive echo switches to optimistic and a banner confirms. Press `Ctrl-^`
again, arrow to "Debug logging: on", Enter — a per-pid log path appears in the
banner.

Run with the renderer beside the binary (the toolset layout), no env needed:

    posh client host 60001         # Ctrl-^ finds ./posh-palette next to posh

Point at an explicit renderer build:

    POSH_PALETTE=/path/to/posh-palette posh client host 60001

## Limitations

- **Roaming client only.** The session-attach client (`session/client.rs`) has no
  palette; its escape key is still `Ctrl-\` (detach). Converging the two client
  input loops onto a shared core — so the palette works in both — is tracked:
  amarbel-llc/posh#87.
- **Suspend and shell-out lost their direct chords.** They are palette commands
  now, not `Ctrl-^ Ctrl-Z` / `Ctrl-^ s`. The only hardcoded chord left is the
  emergency `Ctrl-^ .` (plus `Ctrl-^ ^` literal), and only when the renderer
  can't open.
- **Renderer dependency.** A working palette needs the `posh-palette` binary (a
  separate Go module/derivation, co-installed by the toolset). Without it you get
  the fallback prefix, which can only quit.
- **First-summon latency.** The renderer spawns lazily on the first `Ctrl-^` and
  the client blocks briefly (up to the 2s handshake timeout) on the `initialize`
  exchange; later summons reuse the resident process.
- **The renderer owns the keyboard while open.** Keystrokes forward to the
  palette, not the session; the session keeps running underneath but receives no
  input until the palette closes.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| handshake timeout | 2s | ample for a local spawn + bubbletea start | spawns time out on slow hosts |
| shutdown grace | 300ms | renderer exits promptly on `ui.shutdown`; SIGKILL backstop after | renderers routinely need longer to exit cleanly |
| anchor / width | 1/3 down, centered, width 46 | matches the POC; fits 80 columns | users want it elsewhere or width-responsive |

## More Information

- Wire contract: RFC 0005 (`docs/rfcs/0005-palette-control-protocol.md`) — the
  NDJSON JSON-RPC 2.0 control protocol, including the §7 client method registry
  (`echo.set`, `logging.set`, `shell.open`, `client.suspend`, `app.quit`).
- Hosting + compositing live in `remote/client.rs` (`open_palette`,
  `dispatch_palette_action`, `composite_palette`, the poll-loop palette fds); the
  renderer host is `remote/palette.rs`; the PTY + fd-3 spawn is
  `pty::spawn_with_control`. The renderer is the `posh-palette` Go module
  (bubbletea v2), co-installed in poshToolset.
- Supersedes the `Ctrl-^` escape-menu sub-key chords. FDR 0008 (escape-to-shell)
  is now summoned by the palette's "Shell out" command rather than `Ctrl-^ s`;
  its server-side overlay mechanism is unchanged.
- Builds on the runtime echo/logging controls and the predictive-echo models
  (FDR 0006).
- Converging the attach client so it gets the palette too: amarbel-llc/posh#87.
