---
status: experimental
date: 2026-06-11
promotion-criteria: a manual smoke pass (attach/detach, vim-in-session,
  `reset`-in-session, remote connect/quit/suspend) shows the outer terminal
  restored byte-perfect in kitty + one other terminal, and no
  switch-virtualization repaint glitches are reported for two weeks of
  daily use.
---

# Terminal takeover and restore

## Problem Statement

Attaching to a session used to clear the user's terminal in place and draw
the session over it; detaching left whatever the session had painted (modes
reset, screen kept). The shell prompt, command line, and visible history
the user attached from were destroyed — and remote connections behaved the
same way on exit. Users expect the tmux/mosh model: the multiplexer owns
the whole terminal while connected, and leaving puts the terminal back
exactly as it was found.

## Interface

Both connection paths take over the outer terminal's **alternate screen**
for their whole lifetime and return to the primary screen on the way out:

- `posh attach <name>` (and every grammar form that attaches locally):
  enters the alt screen before the replay, leaves it after detach
  (`Ctrl-\`), session exit, or a fatal signal. The pre-attach shell screen
  — prompt and cursor included — reappears. Suspend/resume (`SIGSTOP`/
  `SIGCONT`) re-enters the alt screen and replays.
- `posh <host>` / `posh <host>:<session>` / `posh client`: the remote
  client does the same around the connection (mosh's smcup/rmcup
  behavior), including around the `Ctrl-^ Ctrl-Z` suspend, so the
  `[posh is suspended.]` notice and the exit message land on the user's
  own shell screen.

Because the outer terminal must never leave posh's screen mid-attach, the
session daemon **virtualizes screen switches** in the attach broadcast:
the application's own DECSET/DECRST 47/1047/1049 and RIS (`ESC c`) are
excised from the raw stream and replaced with a repaint of the newly
active screen generated from the daemon's terminal model (`Terminal::
dump_screen_switch`). Modes co-set in one sequence survive
(`CSI ? 1049;2004 h` forwards as `CSI ? 2004 h`); RIS additionally resets
the shared modes a real RIS would have reset. The remote path needs no
virtualization — its renderer is fully model-driven.

Attach replay now uses `Terminal::dump_vt_flat()`: the active grid only,
never switching the outer terminal's buffers and never replaying
scrollback into the outer terminal.

## Examples

    $ echo precious prompt state
    precious prompt state
    $ posh attach work        # terminal switches to the session screen
    ... work in the session, run vim, even run `reset` ...
    Ctrl-\                    # detach
    $ echo precious prompt state   # <- original screen is back, cursor
    precious prompt state           #    on the shell line it left

    $ posh box:dev            # remote session over the roaming transport
    Ctrl-^ .                  # quit
    posh: [client exited]     # printed on the restored shell screen

Running a full-screen app inside the session no longer risks the outer
terminal: when vim exits inside the session, attached clients see the
session's primary screen repainted in place — the outer terminal stayed on
posh's alt screen throughout.

## Limitations

- **The outer terminal's native scrollback is no longer populated by the
  session.** Alternate screens have no scrollback ring, so the pre-2026
  behavior of replaying session scrollback into the attaching terminal is
  gone by design; `posh history <name>` (plain or `--vt`) is the access
  path. The remote path never synced scrollback to begin with.
- Switch virtualization covers DECSET/DECRST 47/1047/1049 and RIS. Exotic
  ways of leaving the alt screen (a terminal-specific reset) would not be
  caught; none are known to be emitted by real applications.
- While a client is attached, terminal queries are answered by the real
  terminal (#13), so a DECRQM for 47/1047/1049 now reports the outer
  terminal's pinned alt screen rather than the session's own alt state.
  No real application is known to branch on it; revisit if one appears.
- The repaint substituted for a switch re-asserts region/origin/charsets/
  insert/autowrap and kitty graphics placements, but not per-screen kitty
  keyboard flags or DECSC saved cursors (a re-attach replay restores
  those); well-behaved apps push/pop their own kitty flags around alt
  usage, which passes through raw and stays balanced.
- A client built before this feature attached to a new daemon (or vice
  versa) degrades gracefully but without the takeover guarantee: the
  virtualized broadcast renders correctly on an unwrapped terminal, but an
  old daemon will forward raw 1049 to a new client, where an inner
  full-screen app's exit can pop the outer terminal back to the user's
  shell. Re-create long-lived session daemons after upgrading.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| mid-sequence hold cap (`MAX_HELD`) | 4096 bytes | real switch sequences are ~10 bytes; cap bounds memory against a malicious endless CSI | a legitimate sequence longer than 4 KiB shows up torn in a client |
| switch repaint scope | grid + cursor + drawing modes + graphics placements | keeps per-switch cost ~one screen of bytes; shared modes are already in sync via passthrough | visible desync after vim enter/exit (kitty flags, saved cursor) reported in practice |

## More Information

- RFC 0001 (`docs/rfcs/0001-target-grammar-and-capability-table.md`) — the
  attach grammar this applies to.
- mosh's `Display::open`/`close` (smcup/rmcup) in
  `zz-mosh/src/terminal/terminaldisplay.cc` — the model for the remote
  client behavior.
- `crates/posh/src/session/daemon.rs` (`ScreenSwitchFilter`) and
  `crates/posh-term/src/dump.rs` (`dump_vt_flat`, `dump_screen_switch`)
  carry the implementation.
