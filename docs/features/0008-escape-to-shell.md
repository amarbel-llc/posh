---
status: experimental
date: 2026-06-20
---

# Escape to shell (Ctrl-^ s)

> **Trigger moved (2026-06-22):** the overlay is now summoned by the command
> palette's *Shell out* command (FDR 0009), not the `Ctrl-^ s` chord. The
> server-side mechanism, wire bits, and `$POSH_ESCAPE_CMD` below are unchanged;
> only the client-side trigger and `$POSH_ESCAPE_KEY` are superseded.

## Problem Statement

A user working inside a full-screen session app — notably a clown/claude TUI —
wants to momentarily drop to a shell in the session's working directory and come
back, the classic ctrl-z-to-shell move. Job control cannot deliver it: the TUI
holds the pts in raw mode (`-isig`), so `^Z` arrives as an inert `0x1A` byte and
no SIGTSTP is ever generated. The only interception point above the app's tty is
the posh roaming client, which already pulls one key out of the input stream for
its mosh-style `Ctrl-^` escape menu (`.` quit, `Ctrl-Z` suspend, `^` literal).

This adds a sibling: `Ctrl-^ s` opens a transient shell **on the server host**,
in the session's cwd, as an overlay the client renders until the shell exits.
Server-side is the load-bearing choice — for cross-host roaming the worktree
lives on the server, so a client-local shell would land on the wrong machine.

## Interface

- **Trigger:** the command palette's *Shell out* command (`Ctrl-^`, then select
  it — FDR 0009). Raw-mode-proof: the client reads raw stdin and decides what to
  forward, so the inner app's `-isig` is irrelevant. (Originally the `Ctrl-^ s`
  sub-key chord, `$POSH_ESCAPE_KEY`; that chord is superseded by the palette.)
- **Command:** `$POSH_ESCAPE_CMD` (read by the server), whitespace-split into
  argv; unset/blank runs `$SHELL` as a login shell (the session default). For the
  bare `posh host:session` form it is forwarded from the client over the ssh
  bootstrap (like `POSH_DEBUG_LOG`), so you set it **once on the client**
  (`export POSH_ESCAPE_CMD='sc exec'`) and it takes effect on the remote server.
  In an eng/spinclass environment it is set to `sc exec`, which resolves the
  worktree session from the cwd, loads its devshell, and sets the `SPINCLASS_*`
  identity env — so posh stays generic with no hardcoded spinclass dependency.
- **Working directory:** the session's OSC-7 cwd (`posh_term::Terminal::pwd()`),
  with a fallback to the server's own cwd then `$HOME` when the shell reported
  none.
- **Mechanism:** the server spawns a second PTY + terminal model; while it is up
  the broadcast source and input sink swap to the overlay, the live session
  keeps running underneath (still read into the main model, just not broadcast),
  and the session is repainted when the overlay shell exits.
- **Wire:** a one-shot `CLIENT_FLAG_ESCAPE` request bit; the server echoes
  `FLAG_OVERLAY` on its frames while the overlay is active (the client clears its
  "opening shell…" notice on seeing it). Both are runtime flag bits on the
  existing `flags` byte — see RFC 0001.

## Examples

Inside a claude session over a roaming connection, with `POSH_ESCAPE_CMD='sc
exec'`: press `Ctrl-^` then `s`. A shell opens in the worktree root with the
session's devshell loaded; run a git command, then `exit` to return to the live
session exactly where it was.

Plain default (`$SHELL`), inspecting the overlay's environment:

    POSH_ESCAPE_CMD=env   # the overlay prints its env, then exits → session resumes

Remap the trigger to the vi/less convention:

    POSH_ESCAPE_KEY='!'   # Ctrl-^ ! now opens the shell

## Limitations

- **OSC-7 dependency.** The worktree-root cwd relies on the session shell
  emitting OSC 7. Without it the overlay lands in the fallback cwd, and `sc exec`
  then degrades to a plain shell (its own graceful-degradation path).
- **Ctrl-^ still governs the session, not the overlay.** While the overlay is up,
  the client's `Ctrl-^` palette (and its Quit) act on the whole session, not just
  the shell; exit the overlay with the shell's own `exit`. Selecting *Shell out*
  again is ignored (the server's already-in-overlay guard).
- **Request loss.** `CLIENT_FLAG_ESCAPE` is one-shot (cleared after one send), so
  a dropped request datagram just means the user presses `Ctrl-^ s` again.
- **Roaming only (for now).** The session-attach client (`session/client.rs`),
  where a client-local spawn would suffice, is a tracked follow-up
  (amarbel-llc/posh#85).

## More Information

- The escape key reuses the existing `Ctrl-^` state machine in
  `remote/client.rs:process_user_input`; the overlay lives in
  `remote/server.rs:server_loop` (the `Overlay` struct + source/sink swap), and
  `pty::spawn_shell` gained a `cwd` parameter.
- `FLAG_OVERLAY` mirrors `FLAG_ECHO` (FDR 0006) as a per-frame server→client
  state bit; `CLIENT_FLAG_ESCAPE` mirrors `CLIENT_FLAG_SHUTDOWN`. `0x02` stays
  reserved as the caps EXTENSION bit, so the new bits are `0x04`/`0x08`.
- Coordinated with the spinclass `sc exec` payload, which is the generic seam:
  posh spawns the configured command in the session cwd, and `sc exec`
  self-resolves the session — no posh→spinclass coupling.
