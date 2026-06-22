# charm-tui-poc

Throwaway POC: a long-running Go [bubbletea](https://github.com/charmbracelet/bubbletea)
**v2** renderer driven by the Rust host over a **JSON-RPC-style control channel**,
its output composited over a retained session screen by posh's client-side
`posh_term::Terminal` emulator. Validates that because the posh client owns a
full terminal emulator, it can run an arbitrary charmbracelet renderer as a
host-driven, mux-style overlay — a local analogue of the server-side
escape-to-shell overlay (FDR 0008).

The renderer shows a **command palette** on demand — a `/`-style filter list
modeled on [trapeze](https://github.com/amarbel-llc/trapeze)'s Commands dialog,
opened by `Ctrl-^`.

## Layout

- `tui/`   — the Go bubbletea **v2** renderer (`main.go`), on the
  `charm.land/bubbletea/v2` + `bubbles/v2` + `lipgloss/v2` stack (the versions
  trapeze uses). Long-running: it reads newline-delimited JSON-RPC on **fd 3**
  (`show {view, commands}` / `hide`), renders the palette to its PTY, and echoes
  the chosen command's JSON-RPC **action** back on the same socket. Built to
  `tui/tui-bin`.
- `host/`  — a standalone Rust driver (`charm-tui-host`) depending on the in-repo
  `posh-term` crate, `libc`, and `serde_json`. It spawns the renderer once on a
  PTY + a control socket (fd 3), keeps a `posh_term::Terminal` for the renderer
  and another for the retained session background, owns input routing and
  command dispatch, and **composites** to the real terminal with a **per-cell
  diff** (reusing `posh_term::sgr_params`). All unsafe/PTY FFI lives here;
  `posh-term` stays 100% safe.
- `flake.nix` — isolated devShell providing Go 1.26 (the repo devShell has none).

### Control protocol (one JSON object per line on fd 3)

```
host -> renderer:  {"method":"show","params":{"view":"palette","commands":[
                     {"name":"Quit","action":{"method":"app.quit"}},
                     {"name":"Predictive echo: Optimistic",
                      "action":{"method":"echo.set","params":{"model":"Optimistic"}}}]}}
                   {"method":"hide","params":{}}
renderer -> host:  the chosen command's action verbatim, e.g.
                     {"method":"echo.set","params":{"model":"Optimistic"}}
                   {"method":"ui.cancel"}
```

## Run

```
just test    # headless: build both, run the self-test, print PASS/FAIL
just run     # interactive: drive it in your own terminal
```

In `just run`:

- A base "live session" screen shows the current stand-in state (debug logging
  on/off, predictive echo model).
- Press **`Ctrl-^`**: the session **greys out** and the **command palette**
  opens directly — a yellow-bordered box anchored a third of the way down (it
  expands downward / collapses upward as you filter). The host sends
  `show {view:"palette", commands:[…]}` and forwards keystrokes to the renderer
  while it's up. (A bare `/` is intentionally *not* a trigger — too common a
  character to hijack.)
- Type to filter, `↑`/`↓` choose, `Enter` runs, **`Esc` cancels**.
- Selecting a command echoes its JSON-RPC **action** back, which the host
  dispatches: **`Toggle debug logging`** (`logging.toggle`) and **`Predictive
  echo: <model>`** (`echo.set`) flip stand-in state shown on the base screen;
  **`Quit`** (`app.quit`) exits; **`Clear`/`Redraw session`** blank/restore the
  background. The logging + echo entries are **mocks** — posh's real logging and
  predictive echo need runtime toggles / a protocol change that don't exist yet
  (the #2/#3 follow-ups); the same `method` surface is what could ride the wire.

## What it proves

- A single charmbracelet renderer can be **driven by the host over JSON-RPC**
  (host → `show`/`hide`; renderer → the chosen command's `action`), with the PTY
  as the visual channel and a separate control socket. Commands and their option
  actions form a small JSON-RPC API — the same surface a remote peer could
  service over the wire.
- `posh_term` composites that renderer over a retained session: the palette as a
  yellow-bordered popup anchored a third down, over a **greyed-out** session
  background — all via posh-term's own cell state + SGR emitter and a per-cell
  diff (only changed cells written; closing the palette reveals the session
  underneath).

## Known limits / out of scope (deliberate POC shortcuts)

- **`Ctrl-^` opens the palette, not a bare `Ctrl-.`** A bare `Ctrl-.` is not a
  control byte; reporting it needs the kitty / CSI-u keyboard protocol. Deferred.
- **The palette is a faithful *reproduction* of trapeze's UX, not its literal
  code** (trapeze's `Commands` dialog is a 568-line modal coupled to its custom
  `ultraviolet` cell-renderer). The command set is trimmed to what the host
  actually supports.
- **The palette anchors a third of the way down** and expands downward /
  collapses upward as it filters; a long list in a short terminal clips at the
  bottom (list scrolling is a follow-up). The real cursor is **hidden** while an
  overlay is up (cursor mapping is a follow-up). There is **no SIGWINCH/resize
  handling.**
- **Local-only, no server** — this POC hosts the overlay standalone; the real
  roaming client (`remote/client.rs`) requires a server.
- **Rust↔Go is a spawned binary over a PTY + control socket** (no FFI).
