---
status: experimental
date: 2026-06-20
---

# Transport state dump (SIGUSR2)

## Problem Statement

When a roaming session wedges — the remote client stops receiving updates while
its local scrollback still scrolls — the server side gives almost nothing to go
on. The roaming server (`remote/server.rs`) owns the PTY directly (mosh-server
style) and has no local session-daemon socket, so `posh list` does not see it;
all an operator on the server can do is read the process table, the kernel UDP
table, and `/proc`. Those prove the *process* is healthy (alive, event loop
cycling, holding its UDP socket and PTY master, shell child alive) but cannot
reveal the **userspace transport state** that says *why* frames are not landing:
the peer address the server is replying to, how long since it last heard from
the client, and whether it has frames the client has not acked.

`POSH_DEBUG_LOG` already logs that class of data, but it is **start-up-gated** —
it must be set before the process starts, so it is useless once a session is
already wedged. This adds an on-demand snapshot triggered by a signal, so a
*running* server or client can be asked for its live transport state without a
restart (which would destroy the very session being diagnosed).

## Interface

`SIGUSR2`, sent to a roaming `posh server` or `posh client` process, appends one
timestamped line of live transport state to a diagnostic sink on the next event-
loop iteration, then continues normally. It is purely additive: no effect on the
session, zero steady-state cost when unused.

- **Sink:** `$POSH_DEBUG_LOG` when set (the dump shares the periodic-stats file);
  otherwise a per-pid default `<rundir>/posh-<role>-<pid>.log`, where `<rundir>`
  resolves as the session socket dir does (`$POSH_DIR`, else
  `$XDG_RUNTIME_DIR/posh`, else a `/tmp/posh-<uid>` fallback) — so the dump file
  sits beside the sockets and is found by the same triage recipes. 5 MB rotation,
  shared with the existing logger.
- **Server line** carries: `peer_active`, `has_remote`, `remote=<addr>`,
  `last_heard_age_ms`, `last_send_age_ms`, `current_num`/`acked_num`/`unacked`,
  `outstanding`, `srtt`/`rto`/`send_interval`, `bytes_rx`/`bytes_tx`,
  `term_gen`, `pty_open`.
- **Client line** carries: `remote=<server-addr>`, `last_send_age_ms`,
  `applied_num`, `outbox_base`/`outbox_pending`, `scrollback_len`,
  `srtt`/`rto`/`send_interval`, `bytes_rx`/`bytes_tx`, `predict(...)`,
  `term_gen`, `rows`/`cols`, `echo_on`.

The client writes to a **file, never the tty** — its stdout is the alternate-
screen TUI and its stderr is the user's outer shell, so either would corrupt the
display.

`just debug-posh-dump <pid>` wraps the `kill -USR2` + tail. Implementation:
`remote/diag.rs`; documented under SIGNALS in `posh-server`(1) / `posh-client`(1)
and the debugging notes in this repo's `CLAUDE.md`.

## Examples

A wedged server that has not heard from its client in 40s and has a frame the
client never acked — a stalled delivery, peer still nominally pinned:

    role=server pid=648244 peer_active=1 has_remote=1 remote=100.85.205.39:51234 \
      last_heard_age_ms=40231 last_send_age_ms=57 current_num=918 acked_num=915 \
      unacked=3 outstanding=3 srtt=114ms rto=350ms send_interval=57ms ... pty_open=1

A healthy-but-idle server whose client roamed away (peer forgotten after the
60s timeout): `peer_active=0 remote=none` — the session is fine, waiting for the
client to reappear from a new address.

## Limitations

- **Point-in-time, not a trail.** The dump is a single snapshot at signal time.
  If the process dies before it is signalled, nothing is captured — only a pre-
  armed `POSH_DEBUG_LOG` leaves a continuous record. (This bit the original
  incident: the wedged servers vanished before they could be introspected.)
- **Server/caller env skew.** `just debug-posh-dump` guesses the sink from the
  caller's environment; if the process was started with a different
  `POSH_DEBUG_LOG`, read the file the process actually opened. The per-pid
  default is deterministic and is checked first.
- **No remote round-trip.** SIGUSR2 must be delivered locally (`kill` on the
  host running the process); there is no "dump the remote server" command. For a
  bare `host:session` the server is on the far host — signal it there.
- **Not a fix.** The dump localizes a wedge (stale peer address vs. one-way loss
  vs. dead client); recovering the session is a separate action (reconnect the
  client, which re-pins the peer).

## More Information

- The signal plumbing mirrors the existing `SIGUSR1` path (`util.rs`:
  `install_sigusr2_handler` / `SIGUSR2_RECEIVED` / `take_flag`); handlers only
  set an `AtomicBool`, and the dump runs in normal loop context, so the file I/O
  is unrestricted by async-signal-safety. No `SA_RESTART`, so `poll()` wakes on
  the signal and the dump lands within one loop iteration.
- `SIGUSR1` stays the mosh-parity idle-exit trigger (`POSH_SERVER_SIGNAL_TMOUT`);
  `SIGUSR2` is the new, distinct, side-effect-free introspection signal.
- The `Connection::remote()` accessor added for the peer address is the field
  that most directly distinguishes a roam the server has not re-pinned (the
  address it is replying to differs from where the client now sends) from
  symmetric loss.
- Companion read-only triage recipes (`just debug-posh-procs`,
  `debug-posh-sockets`, `debug-posh-proc-state`, `debug-posh-proc-sample`,
  `debug-posh-server-smoke`) cover the process/kernel/`/proc` side that frames
  when a SIGUSR2 dump is worth taking.
