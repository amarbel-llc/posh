# charm-tui-poc

Throwaway POC: a long-running Go [bubbletea](https://github.com/charmbracelet/bubbletea)
**v2** renderer driven by the Rust host over a **JSON-RPC-style control channel**,
its output composited over a retained session screen by posh's client-side
`posh_term::Terminal` emulator. Validates that because the posh client owns a
full terminal emulator, it can run an arbitrary charmbracelet renderer as a
host-driven, mux-style overlay — a local analogue of the server-side
escape-to-shell overlay (FDR 0008).

The renderer shows one of two views on demand:
- a **command palette** (a `/`-style filter list modeled on
  [trapeze](https://github.com/amarbel-llc/trapeze)'s Commands dialog), and
- a **chord-armed indicator** (shown while the `Ctrl-^` prefix is pending).

## Layout

- `tui/`   — the Go bubbletea **v2** renderer (`main.go`), on the
  `charm.land/bubbletea/v2` + `bubbles/v2` + `lipgloss/v2` stack (the versions
  trapeze uses). Long-running: it reads newline-delimited JSON-RPC on **fd 3**
  (`show {view}` / `hide`), renders the requested view to its PTY, and reports
  palette `selected`/`cancel` **events** back on the same socket. Built to
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
host -> renderer:  {"method":"show","params":{"view":"palette","commands":[{"name":"Quit","shortcut":""}]}}
                   {"method":"show","params":{"view":"chord"}}
                   {"method":"hide","params":{}}
renderer -> host:  {"method":"event","params":{"kind":"selected","command":"Quit"}}
                   {"method":"event","params":{"kind":"cancel"}}
```

## Run

```
just test    # headless: build both, run the self-test, print PASS/FAIL
just run     # interactive: drive it in your own terminal
```

In `just run`:

- A base "live session" screen is shown (stand-in for the real session).
- Press **`Ctrl-^`**: the whole session **greys out** and a centered
  charmbracelet **chord indicator** appears (`.` palette, `q` quit, any other key
  cancels). The host sends `show {view:"chord"}`; the grey-out is a host-side
  compositor pass.
- Then `.` (or a bare `/` from the base) opens the **command palette** — a popup
  anchored a third of the way down. Type to filter, `↑`/`↓` choose, `Enter` runs,
  **`Esc` cancels**. The host sends `show {view:"palette", commands:[…]}` and
  forwards keystrokes to the renderer while it's up.
- Selecting a command sends a `selected` event back; the host performs it:
  **`Quit`** exits, **`Clear session`** blanks the background, **`Redraw session`**
  restores it. Only host-supported commands are listed.
- `Ctrl-^ q` quits the driver directly.

## What it proves

- A single charmbracelet renderer can be **driven by the host over JSON-RPC**
  (host → `show`/`hide`, renderer → `selected`/`cancel`) to present different
  views, with the PTY as the visual channel and a separate control socket.
- `posh_term` composites that renderer over a retained session: the palette as an
  anchored popup, and the chord indicator over a **greyed-out** background — all
  via posh-term's own cell state + SGR emitter and a per-cell diff (only changed
  cells written; closing an overlay reveals the session underneath).

## Known limits / out of scope (deliberate POC shortcuts)

- **Chord is `Ctrl-^ .`, not a bare `Ctrl-.`** A bare `Ctrl-.` is not a control
  byte; reporting it needs the kitty / CSI-u keyboard protocol. Deferred.
- **The palette is a faithful *reproduction* of trapeze's UX, not its literal
  code** (trapeze's `Commands` dialog is a 568-line modal coupled to its custom
  `ultraviolet` cell-renderer). The command set is trimmed to what the host
  actually supports.
- **The palette anchors a third of the way down** and expands downward /
  collapses upward as it filters; a long list in a short terminal clips at the
  bottom (list scrolling is a follow-up). The real cursor is **hidden** while an
  overlay is up (cursor mapping is a follow-up). There is **no SIGWINCH/resize
  handling.**
- **Local-only, no server**, and **bare `/` is intercepted globally** in the base
  screen for convenience (a real client would route `/` to the session).
- **Rust↔Go is a spawned binary over a PTY + control socket** (no FFI).
