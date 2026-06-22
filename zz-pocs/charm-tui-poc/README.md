# charm-tui-poc

Throwaway POC: a chord-summoned command-palette **overlay** composited over a
**real local shell** running in a `posh_term::Terminal` — the same client-side
emulator the posh roaming client drives (`server_term`, fed by the remote
shell). The overlay is a Go [bubbletea](https://github.com/charmbracelet/bubbletea)
**v2** renderer driven by the Rust host over a JSON-RPC control channel.

The point of this shape: it mirrors the real client, so the next step is
**lifting the overlay into `remote/client.rs`** — swapping the local shell for
`server_term` — rather than rewriting it.

## Layout

- `tui/`   — the Go bubbletea **v2** palette renderer (`main.go`), on the
  `charm.land/bubbletea/v2` stack (trapeze's versions). Long-running: reads
  newline-delimited JSON-RPC on **fd 3** (`show {commands}` / `hide` /
  `shutdown`), renders the palette to its PTY, and echoes the chosen command's
  JSON-RPC **action** back. Built to `tui/tui-bin`.
- `host/`  — a standalone Rust driver (`charm-tui-host`; deps: `posh-term`,
  `libc`, `serde_json`). It:
  - spawns **`$SHELL`** on a PTY into `session: posh_term::Terminal` (like the
    client feeds `server_term`), renders it with a per-cell diff, and routes
    stdin to it;
  - owns an **`Overlay`** component (the renderer process + its emulator + the
    JSON-RPC dispatch) that, on `Ctrl-^`, greys the session and composites the
    palette over it; command results show in a transient **banner** (the POC
    stand-in for the client's `NotificationEngine`);
  - handles **SIGWINCH** to resize the shell + session live.
  All unsafe/PTY FFI lives here; `posh-term` stays 100% safe.
- `flake.nix` — isolated devShell providing Go 1.26 (the repo devShell has none).

### Control protocol (one JSON object per line on fd 3)

```
host -> renderer:  {"method":"show","params":{"view":"palette","commands":[
                     {"name":"Quit","action":{"method":"app.quit"}},
                     {"name":"Predictive echo: Optimistic",
                      "action":{"method":"echo.set","params":{"model":"Optimistic"}}}]}}
                   {"method":"hide","params":{}}
                   {"method":"shutdown"}    # quit: renderer p.Kill()s; host SIGKILLs if it lingers
renderer -> host:  the chosen command's action verbatim, e.g.
                     {"method":"echo.set","params":{"model":"Optimistic"}}
                   {"method":"ui.cancel"}
```

## Run

```
just test    # headless: build both, run the self-test, print PASS/FAIL
just run     # interactive: a real shell + the palette overlay, in your terminal
```

In `just run`:

- You get a **real shell** (`$SHELL`) — type, run commands, it behaves normally.
- Press **`Ctrl-^`**: the shell **greys out** and the yellow-bordered **command
  palette** drops in a third of the way down. Type to filter, `↑`/`↓` choose,
  `Enter` runs, `Esc` cancels.
- Selecting **`Toggle debug logging`** or **`Predictive echo: <model>`** raises a
  reverse-video **banner** (`posh: predictive echo: Optimistic`) and updates the
  POC mock state; **`Quit`** ends the POC and returns you to your parent shell.
- Resize the terminal — the shell session reflows.

The logging/echo commands are **mocks** here (the shell has no real posh echo or
logging); they become real once the overlay is lifted into the client.

## What it proves

- posh's client-side `posh_term` emulator can host a live shell session **and**
  composite a chord-summoned charmbracelet palette over it (greyed + anchored),
  with a banner for status — all via posh-term's public cell API + a per-cell
  diff.
- A single renderer is driven over **JSON-RPC** (host -> `show`/`hide`/
  `shutdown`; renderer -> the chosen command's `action`), the same method
  surface a remote peer could service over the wire.
- The overlay is factored as a self-contained `Overlay` component, so lifting it
  into the real client is a swap of the session source, not a rewrite.

## Lift map (the next step — into `remote/client.rs`)

| POC (this) | Real client |
|---|---|
| `session: Terminal` fed by local `$SHELL` | `server_term: Terminal` fed by remote shell |
| stdin -> shell PTY | stdin -> `st.outbox` (to server) |
| `compose`/`diff`/`Presenter` | `display.rs` `Snapshot`/`new_frame` |
| `Banner` | `NotificationEngine` (`set_message`/`apply`) |
| `Ctrl-^` via `is_open_trigger` | `process_user_input` chord |
| `dispatch_rpc` mock state | real: `predict::build` swap; `util::log_enable/disable` |
| local SIGWINCH resize | already wired (`util.rs` SIGWINCH flag) |

## Known limits / out of scope (deliberate POC shortcuts)

- **`Ctrl-^` opens the palette, not a bare `Ctrl-.`** A bare `Ctrl-.` is not a
  control byte; reporting it needs the kitty / CSI-u keyboard protocol. Deferred.
- The palette anchors a third down and clips at the bottom in a very short
  terminal (list scrolling is a follow-up); the real cursor is hidden while it's
  open (cursor mapping is a follow-up).
- The logging/echo commands are **mocks** (the lift makes them real).
- **Local-only, no server** — the lift is what reaches the real roaming client.
- **Rust↔Go is a spawned binary over a PTY + control socket** (no FFI).
