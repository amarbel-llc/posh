---
status: proposed
date: 2026-09-23
---

# Push shell

## Problem Statement

A user inside a full-screen session app — a clown/claude TUI, an editor —
wants to drop to a shell in the session's working directory and come back.
FDR 0008 solved that with an **overlay**: a second PTY inside the session
daemon that takes over the session's display and input until its shell
exits. It works, but it is a parallel mechanism with its own rules, and
several of them are wrong:

- It is **session-wide**. It swaps the view of *every* attached viewport, and
  input from any of them goes to the shell — one person's shell-out hijacks
  everyone's screen.
- It is **invisible**: no name, no listing, no title, no picker row, and no
  record that it ended or how.
- It is **not a session** as far as posh is concerned (it gets no
  `POSH_SESSION`), so in-session commands don't work from inside it.
- It needs its own wire bits (`CLIENT_FLAG_ESCAPE`, `FLAG_OVERLAY`), its own
  IPC verb (`Tag::Shell`), and translation shims in both bridges.

Meanwhile FDR 0016's session stack already does everything a shell-out needs:
enter a session and push the one you left; when it ends, pop back and say so.

## Interface

- **Trigger:** the command palette's *Push shell* (FDR 0009). The palette
  shows **exactly one** of *Push shell* or *Shell out*: *Push shell* when the
  session's daemon offers it and the transport carries a re-home back to the
  viewport; *Shell out* (the FDR 0008 overlay) otherwise.
- **What happens:** the session's daemon creates a new **anonymous** session
  (`s-N`) in its own group, in the session shell's OSC-7 cwd (falling back to
  the daemon's cwd, then `$HOME`), running `$POSH_ESCAPE_CMD` (unset or blank:
  a `$SHELL` login shell). It then re-homes the requesting viewport onto it
  (RFC 0008 §3.1). Arriving pushes the session you were in, like any FDR 0016
  transition.
- **Coming back:** exit the shell. That ends the pushed session, and the
  default pop returns the viewport to the one beneath, with the must-dismiss
  notice (RFC 0005 §3.6). The notice states how the pushed session ended —
  cause **and** numeric status — and its last activity label (RFC 0013 §5).
  *Back to …* in the palette works too; the pushed session then stays running,
  like any session you switch away from.
- **Environment:** the pushed shell is an ordinary session: it gets
  `POSH_SESSION`, `POSH_GROUP` and `TERM`, and the forwarded agent socket its
  parent's daemon was born with.
- **Wire:** RFC 0016 (capability ids 21, 22).

## Examples

With `POSH_ESCAPE_CMD='sc exec'` in a spinclass worktree, inside a claude
session: `Ctrl-^`, *Push shell*. A shell opens in the worktree root with its
devshell loaded; the palette's heading now reads `… · back: box:s-3`. Run a
git command, `exit`, and the viewport is back in the claude session with a
notice: `box:s-4 — ended (exit 0) · git` struck through, `→ box:s-3`.

A second person watching the same claude session sees nothing change: the
push happened only in the viewport that asked.

## Decisions

Settled 2026-09-23 (a structured review, recorded here so they are not
re-litigated):

1. **The parent daemon creates the session**, not the client. It knows the
   cwd and holds the environment `$POSH_ESCAPE_CMD` is read from; the client
   may be on another machine.
2. **Anonymous, with no push-shell marker.** A pushed shell is not special:
   no rule anywhere treats it differently from a `:+` session.
3. **Every pop is announced**, a clean exit included — the frontmost session
   can exit 0 without the user doing anything.
4. **The notice carries status and activity label**: cause and numeric status
   for the session that ended; `gone` plus the activity label recorded at push
   time for each cascade entry.
5. **Offered only end to end, with the overlay as the fallback** everywhere
   else — an old daemon, the relay, Architecture A — until the posh#213
   cutover deletes it.
6. **`$POSH_ESCAPE_CMD`, unchanged**, is the command for both mechanisms; its
   name is revisited at the cutover.
7. **One palette entry**, chosen by the offer.
8. Lifetime is independent of the parent; a push shell from a push shell
   pushes again; the session is listed, pickable and titled like any other.

## Limitations

- **Not over the relay or Architecture A.** The relay cannot report a re-home
  to its viewport (ADR 0007), and Architecture A has no daemon to create a
  session. Both get *Shell out*.
- **Old daemons get *Shell out*** until they are restarted — a daemon runs the
  code it was started with. posh#213 is the policy for retiring them.
- **cwd depends on OSC 7**, exactly as the overlay's did.
- **A pushed shell outlives you if you detach** from it — it is a durable
  session. It shows in `posh list` like any other.
- **The parent session keeps running** while you are in the pushed shell,
  and can end meanwhile; the pop then cascades past it and reports it `gone`.

## Rollback

Push shell is only ever used when the daemon offers it and the client asked.
A client that stops advertising `CAP_PUSH_SHELL` is never offered it, and its
palette falls back to *Shell out* — the FDR 0008 overlay, which is unchanged.

## More Information

- Supersedes FDR 0008 where offered; FDR 0008 stays the fallback until
  posh#213.
- Wire contract: RFC 0016. Re-home: RFC 0008 §3.1 / FDR 0012. Stack, pop and
  notice: FDR 0016, RFC 0005 §3.6. Activity label: RFC 0013 §5.
- Implementation plan: `docs/plans/2026-09-23-push-shell.md`.
