---
status: accepted
date: 2026-09-23
---

# A session's working directory is a fact cascade, decided in one place

## Context and Problem Statement

Several things need "the session's current working directory": push-cmd
(FDR 0020) creates a new session there, `posh list` shows it, `posh fork`
clones into it, and FDR 0008's overlay spawns its shell there. Today posh
has two readings, and they are not two views of one fact — they mean
different things:

- **`info.cwd`** — `std::env::current_dir()` taken **once, when the daemon
  starts** (`session/daemon.rs`), and reported unchanged in `Tag::Info`. It is
  where the session *was started*. It never moves.
- **OSC 7** — `term.pwd()`, whatever the shell last *reported*. It moves, but
  only if the shell emits OSC 7, and only as of its last prompt (a TUI running
  in the foreground freezes it).

Consumers pick between them inconsistently. `posh fork` uses `info.cwd`, so it
clones the **start** directory even after the user has `cd`'d elsewhere. The
overlay uses OSC 7 and falls back to `info.cwd`. Each new consumer re-decides.

A third reading exists: the kernel's cwd of the session's own child process
(`/proc/<pid>/cwd`). It is current and needs no shell cooperation — but only
Linux has it without new platform code (macOS would need
`proc_pidinfo(PROC_PIDVNODEPATHINFO)`; posh#214).

No single fact is available, current, and portable at once. So there is no
single source of truth to choose.

## Decision Drivers

* **Correct after a `cd`.** The answer must follow the user, not the session's
  birthplace.
* **No shell cooperation required** where the platform allows it.
* **Portability without blocking.** macOS parity is deferred (posh#214); the
  rule must degrade on macOS, not wait for it.
* **One place decides.** Consumers must not each re-derive the rule, which is
  how `posh fork` and the overlay came to disagree.
* **Explainable.** A surprising directory must be traceable to the fact that
  produced it.

## Considered Options

* **OSC 7 only, with `info.cwd` as the fallback** (today's overlay rule).
* **`info.cwd` only.** Always available; wrong after any `cd`.
* **The kernel's cwd only.** Exact, but absent on macOS today.
* **An ordered fact cascade, implemented once.**

## Decision Outcome

Chosen: **an ordered fact cascade, implemented once.** The first fact that is
available *and names an existing directory* wins:

1. **The caller's own cwd** — when the request originates from a process
   *inside* the session (a CLI such as `posh start -- <cmd>`). Exact on every
   platform.
2. **The kernel's cwd of the session's child process** — the process the
   daemon spawned, whose pid it holds. Linux: `readlink /proc/<pid>/cwd`.
   macOS: skipped until posh#214.
3. **OSC 7** (`term.pwd()`), when the shell has emitted it.
4. **The daemon's start directory** (today's `info.cwd`).
5. **`$HOME`.**

There is exactly **one implementation** — a `session_cwd` that returns the
path **and the step that produced it**. Every consumer calls it: push-cmd,
`posh list`, `posh start -- <cmd>` (which replaced `posh fork`, FDR 0020), and the FDR 0008 overlay for as
long as it survives. The daemon owns steps 2–4 (it holds the child pid, the
terminal model, and its start directory), so the implementation lives with it;
a consumer outside the daemon reads the result through `Tag::Info`. As built
(2026-09-23), no caller passes step 1 as an input: the only CLI consumer
(`posh start -- <cmd>` inside a session) satisfies it by inheritance (see
below), so a daemon never reports `caller` — the source value is reserved for
a consumer that does.

The step that won is logged when a session is created from it, and shown by
`posh status`, so "why did this open in `~`?" has an answer.

### Consequences

* Good, because `posh list` and anything that clones a directory now follow
  the user after a `cd`, on Linux even with a shell that never emits OSC 7.
* Good, because the rule lives in one function; a new consumer cannot
  re-decide it, and a new fact (macOS's kernel cwd) slots in as one step.
* Good, because provenance makes the fallback steps visible instead of silent.
* Bad, because `Tag::Info` grows a second directory. As implemented
  (2026-09-23), `cwd` keeps its start-directory meaning — `posh list`'s
  piped STARTED IN column, its terminal table's `← start` marker, and
  `--json` `cwd` rely on it — and the
  cascade's answer rides an appended `cwd_now` field (directory + source
  byte), absent from an older daemon. The status socket (RFC 0014 §4.2)
  reports it as `cwd=`/`cwd_source=`. Two fields that both say "cwd" invite
  a reader to pick the wrong one.
* Bad, because answers differ by platform: the same session can report
  different directories on Linux and macOS until posh#214 lands.
* Neutral: Architecture A's `server_loop` keeps its own OSC-7 rule; it is
  bug-fixes-only under ADR 0007 and not worth converging.

## More Information

* FDR 0020 (push shell / push-cmd) is the first consumer; FDR 0008 the
  second, until the posh#213 cutover.
* posh#214 tracks macOS parity, including this cascade's step 2.
* The CLI half of push-cmd (`posh start -- <cmd>` inside a session) already
  satisfies step 1 by inheritance: the new daemon forks from the CLI and takes
  its cwd.
