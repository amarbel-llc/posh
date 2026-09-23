---
status: experimental
date: 2026-09-23
---

# Push command (push-cmd)

## Problem Statement

A user inside a session often wants a second thing running *right here* — a
shell in the same directory, an editor on a file, a build — and to come back
when it is done. posh has grown three unrelated ways to approximate that, each
with its own rules:

- **FDR 0008's escape-to-shell overlay**: a second PTY inside the session
  daemon that takes over the session's display and input until its shell
  exits. It is **session-wide** (one viewer's shell-out hijacks every attached
  viewport), **invisible** (no name, listing, title or record of how it
  ended), **not a session** (no `POSH_SESSION`), and needs its own wire bits
  (`CLIENT_FLAG_ESCAPE`, `FLAG_OVERLAY`), IPC verb (`Tag::Shell`) and bridge
  shims.
- **`posh fork`**: clones the current session's *command* into a new detached
  session, in the directory the session was **started** in (`info.cwd`) — not
  where the user is now — and does not move the viewport there.
- **`posh start -- <cmd>` typed inside a session**, which already creates a
  session and re-homes the viewport onto it (posh#183).

Meanwhile FDR 0016's session stack already does everything these need: enter
a session and push the one you left; when it ends, pop back and say so.

## Interface

**push-cmd** runs a command as a new anonymous session in the current
session's working directory and pushes the viewport onto it. It has two entry
points:

- **CLI:** `posh start -- <cmd>` from inside a session. It creates an auto-id
  anonymous session running `<cmd>` in the caller's own directory and re-homes
  the viewport it was typed in (the existing FDR 0012 path). `posh start
  --detach -- <cmd>` creates it without moving (everything after `--` is the
  command). This entry point exists today;
  push-cmd makes it the documented one.
- **Palette:** *Push shell* (FDR 0009) — push-cmd with `$POSH_ESCAPE_CMD`
  (unset or blank: a `$SHELL` login shell). The client may be on another
  machine, so the session's daemon creates the session (RFC 0016). The palette
  shows **exactly one** of *Push shell* or *Shell out*: *Push shell* when the
  daemon offers it and the transport carries the re-home back; *Shell out*
  (the FDR 0008 overlay) otherwise.

Both entry points:

- **Directory:** ADR 0008's cascade — the CLI's own cwd; else the kernel's cwd
  of the session's child (Linux); else OSC 7; else the session's start
  directory; else `$HOME`. The step that won is logged and shown by
  `posh status`.
- **Arriving** pushes the session you were in, like any FDR 0016 transition.
- **Coming back:** the command ends → the pushed session ends → the default
  pop returns the viewport to the one beneath, with the must-dismiss notice
  (RFC 0005 §3.6). The notice states how the pushed session ended — cause
  **and** numeric status — and its last activity label (RFC 0013 §5). *Back
  to …* in the palette also works; the pushed session then keeps running,
  like any session you switch away from.
- **Environment:** an ordinary session — `POSH_SESSION`, `POSH_GROUP`, `TERM`,
  and the forwarded agent socket its creator was born with.

`posh fork` is removed: `posh start -- <cmd>` covers it, in the directory the
user is actually in.

## Examples

With `POSH_ESCAPE_CMD='sc exec'` in a spinclass worktree, inside a claude
session: `Ctrl-^`, *Push shell*. A shell opens in the worktree root with its
devshell loaded; the palette heading now reads `… · back: box:s-3`. Run a git
command, `exit`, and the viewport is back in the claude session with a notice:
`box:s-4 — ended (exit 0) · git` struck through, `→ box:s-3`.

From a shell inside a session: `cd ~/src/app && posh start -- nvim main.rs`
opens nvim there as a pushed session; `:q` pops back to the shell.

A second person watching the same session sees nothing change: the push
happened only in the viewport that asked.

## Decisions

Settled 2026-09-23 (a structured review, recorded here so they are not
re-litigated):

1. **Replace, don't fold in.** The overlay and `posh fork` are superseded;
   anything worth keeping comes from the stack.
2. **For the palette, the parent daemon creates the session.** It holds the
   environment `$POSH_ESCAPE_CMD` is read from and can run the cwd cascade;
   the client may be remote.
3. **Anonymous, with no push-cmd marker.** No rule anywhere treats a pushed
   session differently from a `:+` one.
4. **Every pop is announced**, a clean exit included — the frontmost session
   can exit 0 without the user doing anything.
5. **The notice carries status and activity label**: cause and numeric status
   for the session that ended; `gone` plus the activity label recorded at push
   time for each cascade entry.
6. **Palette push-cmd only end to end, with the overlay as the fallback**
   everywhere else — an old daemon, the relay, Architecture A — until the
   posh#213 cutover deletes it.
7. **`$POSH_ESCAPE_CMD`, unchanged**, is the palette's command for both
   mechanisms; its name is revisited at the cutover.
8. **One palette entry**, chosen by the offer.
9. **Directory: ADR 0008's cascade**, one implementation for every consumer.
   The CLI and the palette legitimately enter it at different steps.
10. Lifetime is independent of the parent; push-cmd from a pushed session
    pushes again; the session is listed, pickable and titled like any other.

## Limitations

- **The palette cannot run an arbitrary command** — only `$POSH_ESCAPE_CMD`.
  Arbitrary commands are the CLI's job; RFC 0016 reserves room to add one.
- **Palette push-cmd is not available over the relay or Architecture A.** The
  relay cannot report a re-home to its viewport (ADR 0007), and Architecture A
  has no daemon. Both get *Shell out*.
- **Old daemons get *Shell out*** until they are restarted — a daemon runs the
  code it was started with. posh#213 is the policy for retiring them.
- **macOS directory accuracy**: the cascade's kernel step is Linux-only
  (posh#214), so on macOS the palette falls to OSC 7 or the start directory.
- **A pushed session outlives you if you detach** from it; it shows in
  `posh list` like any other.
- **The parent keeps running** while you are in the pushed session and can end
  meanwhile; the pop then cascades past it and reports it `gone`.
- **`posh fork`'s removal is a CLI break.** Its two behaviors that are not
  carried over: cloning the source's *command* (name it explicitly) and the
  `<source>-N` Named naming (push-cmd sessions are auto-id anonymous).

## Rollback

Palette push-cmd is only ever used when the daemon offers it and the client
asked. A client that stops advertising `CAP_PUSH_CMD` is never offered it, and
its palette falls back to *Shell out* — the FDR 0008 overlay, unchanged. The
CLI entry point is pre-existing behavior.

## More Information

- Supersedes FDR 0008 where offered (FDR 0008 remains the fallback until
  posh#213), and `posh fork`.
- Wire: RFC 0016. Directory: ADR 0008. Re-home: RFC 0008 §3.1 / FDR 0012.
  Stack, pop and notice: FDR 0016, RFC 0005 §3.6. Activity label: RFC 0013 §5.
- Implementation plan: `docs/plans/2026-09-23-push-cmd.md`.
