---
status: experimental
date: 2026-06-29
promotion-criteria: implementation landed (github #67) and merged to master,
  paired with group-scoped remote liveness (#66) and the global-`-g`
  resolution for the session forms. Unit tests cover the argv build
  (`remote_session_argv`), the `--detach`/`--` parse (`parse_remote_session_extra`),
  the effective-group resolution (`effective_remote_group`), and the inner
  command shell-quoting (`detached_command`). The real cross-host proof — a
  detached spawn over real ssh that returns promptly, is idempotent, and is
  reattachable by a later foreground `posh host:group/session` — is the manual
  walkthrough (docs/manual-testing.md §4), because the path needs a real sshd
  (same reason the foreground remote attach is manual, not automated). The bar
  for `testing` is a consumer wiring the remote template (spinclass FDR 0006 /
  clown remote-spawn follow-up) plus that cross-host exercise run green; the bar
  for `accepted` is a roaming session reattached to a worker created this way.
---

# Detached remote session spawn

## Problem Statement

`posh [user@]host:[group/]session` puts the user in a *foreground* roaming
session: it bootstraps `posh-server new` over ssh and the local UDP client
stays attached for the life of the connection. There was no way to express
"create a detached worker on a remote host running `<command>` and return
promptly" — the gap between posh's remote story and first-class remote
workers. A remote session manager (spinclass FDR 0006, clown) needs exactly
that fire-and-return primitive to launch workers it will reattach to later,
mirroring its local `posh attach <id> --detach <entry>` spawn.

## Interface

A leading `--detach` on the remote namespace form requests a detached spawn:

    posh [user@]host:[group/]session --detach [-- command...]

It creates-or-ensures the session on the host (running `command`, default
`$SHELL`) and returns promptly, **without** attaching or standing up the
roaming transport. Concretely it execs the inner
`posh [-g GROUP] attach SESSION --detach [command...]` *directly* over ssh —
no `posh-server new`, no UDP client — so the remote `posh attach --detach`
double-forks a session daemon on the host and exits, and the ssh call returns
with that command's exit status. The session keeps running as a daemon on the
host; a later foreground `posh host:[group/]session` (no `--detach`) attaches
to that same session through a fresh, disposable transport pair. This is the
remote analog of local `posh attach <name> --detach`.

**`--detach` must lead** the post-target args (a `--detach` after the command
separator is part of the command). An optional single `--` after `--detach`
separates posh from an opaque create-command; or omit it and the command
starts at the first non-flag token. Both `posh host:id --detach -- cmd` and
`posh host:id --detach cmd` work.

**Idempotent.** Re-running a spawn for a live session is a no-op: the remote
`posh attach --detach` reports `session "<id>" already exists` (vs. `created`)
and ignores the command, exiting 0. The status line is passed through on
stdout; stderr and stdin are inherited so ssh auth prompts still reach the
user's terminal.

**Group resolution.** The group resolves from the target
(`host:group/session`) when given, else the global `-g`/`$POSH_GROUP`, else
the default group — uniform with the group-scoped remote list (#66) and the
local `:session` path. So `posh -g spinclass host:id --detach` and
`posh host:spinclass/id --detach` are equivalent, and spawn pairs
symmetrically with the `posh -g spinclass list host:` liveness probe.

**Environment.** The inner command rides with the same locale/`TERM`
env-prefix forwarding the foreground bootstrap applies (`LANG`, `LC_*`,
`TERM`, `COLORTERM`, plus `POSH_DEBUG_LOG`/`POSH_ESCAPE_CMD`), each value
shell-quoted, so a worker's environment matches a foreground session's.

## Examples

Spawn a detached worker on a remote host and return immediately:

    $ posh box:dev --detach -- sleep 600
    session "dev" created
    $                              # control returns at once; no attach

Idempotent re-spawn (the session is already live):

    $ posh box:dev --detach -- sleep 600
    session "dev" already exists

Attach to the worker later from anywhere (foreground roaming attach):

    $ posh box:dev                 # reattaches to the running daemon session

Grouped worker, spawn + liveness symmetric under `-g` (the spinclass shape):

    $ posh -g spinclass box:w1 --detach -- worker --serve
    $ posh -g spinclass list box: | grep -qxF box:spinclass/w1   # liveness

## Limitations

- **Spawn is a plain ssh exec, not roaming.** `--detach` returns as soon as
  the remote session daemon is ensured; there is no transport to survive a
  roam *during the spawn* (there is nothing to roam — the call is over in one
  ssh round-trip). Roaming applies to the later foreground attach, as before.
- **Agent forwarding (FDR 0004) does not apply to the spawn.** There is no
  posh transport and no agent endpoint at spawn time; the forwarded agent
  rides the later foreground `posh host:session` attach. A command that needs
  the user's keys *at creation* will not have them.
- **Remote host requirements are unchanged** from the foreground form:
  `posh`/`posh-server` on the non-interactive ssh PATH, and the session's
  `$SHELL` (or the given command) available there.
- **The inner remote command is not wrapped in a `--` separator** by
  `remote_session_argv`, so a create-command whose first token is literally
  `--detach` would be misparsed as a second flag on the remote side. This is
  an absurd edge (no real command is named `--detach`) and is not guarded;
  the local `posh attach` path *does* handle it via its `--` separator.

## More Information

- **github #67** — implementation tracking issue (the gap, the proposed
  direction, the verified code paths).
- **github #66** — group-scoped remote liveness, the paired half: spawn +
  liveness both work remotely under `-g`.
- **FDR 0001** (`0001-unified-host-session-namespace.md`) — the
  `host:[group/]session` namespace this feature extends with `--detach`.
- **FDR 0003** (`0003-mosh-parity-surface.md`) and **FDR 0004**
  (`0004-ssh-agent-forwarding.md`) — the foreground roaming attach and agent
  forwarding that the *later* attach to a spawned worker uses.
- **RFC 0001** (`docs/rfcs/0001-target-grammar-and-capability-table.md`) —
  the target grammar the `--detach` form parses within.
- **spinclass FDR 0006** — the detached-worker launch (`zmx attach {id}
  --detach {entry}`) this is the remote analog of, and a primary consumer.
