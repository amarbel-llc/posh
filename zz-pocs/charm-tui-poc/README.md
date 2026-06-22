# charm-tui-poc

Throwaway POC: a Go [bubbletea](https://github.com/charmbracelet/bubbletea) **v2**
command bar — a `/`-style command palette modeled on
[trapeze](https://github.com/amarbel-llc/trapeze)'s Commands dialog — hosted on
posh's client-side `posh_term::Terminal` emulator and summoned by a chord.
Validates that because the posh client owns a full terminal emulator, it can run
an arbitrary charmbracelet program client-side, a local analogue of the
server-side escape-to-shell overlay (FDR 0008).

## Layout

- `tui/`   — the Go bubbletea **v2** command palette (`main.go`), on the
  `charm.land/bubbletea/v2` + `bubbles/v2` + `lipgloss/v2` stack (the versions
  trapeze uses). Built to `tui/tui-bin`.
- `host/`  — a standalone Rust driver (`charm-tui-host`) depending only on the
  in-repo `posh-term` crate plus `libc`. It `forkpty`-spawns the command bar,
  pumps its PTY output through a `posh_term::Terminal`, renders to the real
  terminal, and forwards keystrokes. All unsafe/PTY FFI lives here; `posh-term`
  stays 100% safe.
- `flake.nix` — isolated devShell providing Go 1.26 (the repo devShell has none).

## Run

```
just test    # headless: build both, run the self-test, print PASS/FAIL
just run     # interactive: drive it in your own terminal
```

In `just run`:

- A base "live session" screen is shown (stand-in for the real session).
- `/` **or** `Ctrl-^ .` summons the command bar (input + render swap to it).
- When `Ctrl-^` is pressed, a reverse-video **prefix-armed** status line appears
  on the bottom row, naming the keys (`.` palette, `q` quit) — so the chord
  state is legible (the earlier OK-button demo gave no hint, which made exit
  hard to discover).
- In the command bar: type to filter, `↑`/`↓` choose, `Enter` runs the
  selection (echoes `ran: <command>`), **`Esc` cancels** back to the base.
- `Ctrl-^ q` quits the driver.

## What it proves

- `posh_term` faithfully emulates a bubbletea **v2** TUI: the rendered palette
  is recoverable via `dump_text()`/`dump_vt()`, filtering narrows the list, and
  selecting a command dispatches it.
- A chord can intercept client-side input to summon/dismiss the overlay, with a
  visible armed-prefix indicator.

## Known limits / out of scope (deliberate POC shortcuts)

- **Chord is `Ctrl-^ .`, not a bare `Ctrl-.`** A bare `Ctrl-.` is not a control
  byte; reporting it needs the kitty / CSI-u keyboard protocol. Deferred.
- **The command bar is a faithful *reproduction* of trapeze's UX, not its literal
  code.** trapeze's `Commands` dialog is a 568-line modal coupled to crush/trapeze
  app state on its custom `ultraviolet` cell-renderer; this POC reproduces the
  slash-palette behavior, key bindings, command labels, and styling as a vanilla
  bubbletea v2 `Model`.
- **The host full-repaints** `dump_vt()` on every change (no frame diffing) and
  there is **no SIGWINCH/resize handling.**
- **Local-only, no server.** The real roaming client (`remote/client.rs`)
  requires a server; this POC hosts the overlay standalone to prove the
  emulator-hosting mechanic without that machinery.
- **Bare `/` is intercepted globally** in the base screen for convenience; a real
  client would route `/` to the session and reserve a chord for the palette.
- **Rust↔Go is a separate spawned binary** (no FFI; the host execs `tui-bin`).
