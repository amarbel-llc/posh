# Mux wire reconnect (posh#162)

Fix the posh#161/#162 zombie: the mux daemon establishes its wire exactly once
(`run_daemon`: `sshwrap::bootstrap` → `resolve` → key → `Connection::client`
→ `mux_loop`) and has no path back — a wire that dies after a successful
bootstrap leaves a daemon that serves refs forever, forwards nothing, and
absorbs every new invocation. Verified live in the 2026-08-20 incident
(suspend → remote 60 s exit → 10 h zombie; see the #162 incident comment).

## Approved design (2026-08-20)

- **Reconnect inside the daemon** — the wire swaps under a stable IPC
  surface; refs and invocation connections survive. (Crash-only rejected:
  invocations don't watch their `MuxHandle`, and long-lived sessions never
  would.)
- **Adaptive probe, not socket errors** — the incident proved ECONNREFUSED
  can simply never arrive (zero recv-error lines against ~1500 heartbeats
  into a closed port). When heard-age crosses a silence threshold, re-request
  the RFC 0013 §3 ident on each heartbeat; an answer resets the quiet period.
  Unanswered past a probe deadline ⇒ verdict: dead.
- **Resume fast path** — a wall-vs-monotonic clock gap detects suspend
  (`now_ms` is `Instant`/CLOCK_MONOTONIC and freezes across it). Gap beyond
  the remote's 60 s peer timeout ⇒ the endpoint is provably gone: skip the
  probe, re-bootstrap immediately. Smaller gaps arm the probe.
- **Send-path error investigation is a separate issue** — socket errors
  become opportunistic in-doubt accelerators later, never the verdict.

Constants (tuning levers, mirror the remote's thresholds):
`PROBE_SILENCE_MS = 15_000` (the remote's agent fast-fail), probe deadline
`PROBE_TIMEOUT_MS = 10_000` (~3 heartbeats), resume-gap arm at
`2 × HEARTBEAT_INTERVAL`, fast path at the remote `PEER_TIMEOUT` (60 s),
reconnect backoff `[0, 2 s, 5 s, 15 s, 60 s…]` capped, retried while refs
are held (refs=0 mid-reconnect hands over to the normal linger/exit).

## Tasks

### 1. `MuxConnState::Reconnecting`

TDD: extend `mux_hello_ack_roundtrips_all_states_and_rejects_truncation` +
`status_line` test with the new variant first. Add `Reconnecting = 3` to the
enum, `from_u8`, `label()` ("reconnecting"). Additive IPC byte; an old
invocation decoding it gets `None` → treated as a failed hello (acceptable
mixed-version shape — same build normally talks to its own daemon).

### 2. Pure liveness policy

New unit-tested pure pieces (no loop, no clocks):
- `WireLiveness` (or fns): given `now`, `last_heard`, `probe_started`,
  decide `request_ident: bool` and `verdict_dead: bool`.
- `suspend_gap(wall_delta_ms, mono_delta_ms) -> Option<u64>` — the resume
  detector; returns the invisible gap when wall ≫ mono.
- `reconnect_backoff(attempt: u32) -> u64` — the capped schedule.

### 3. The reestablish seam

Factor `establish_wire(dest, family, port_range) -> Result<Connection>` out
of `run_daemon`'s closure body (bootstrap → resolve → key → connect).
`mux_loop` gains `reestablish: &mut dyn FnMut() -> Result<Connection>`;
`run_daemon` passes `establish_wire`, existing loop tests pass a panicking
closure (they never reconnect), reconnect tests pass a closure dialing a
second in-process fake peer. Pin `establish_wire`'s call shape like
`mux_bootstrap_ssh_argv_is_bounded_by_connect_timeout` pins the options.

### 4. Probe + reconnect wiring in `mux_loop`

The heartbeat call site (`heartbeat_message(remote_ident.is_none())`)
becomes `remote_ident.is_none() || probe_active`. Any sealed inbound
refreshes `last_heard` and clears the probe. On verdict-dead:
- edge-log `mux wire dead: probe unanswered <ms>` (or `resume gap <ms>`),
- fail open agent channels (and M2 session channels) toward their local fds,
- clear `remote_ident` (mux ls `remote=` returns to `unknown` — the #162
  staleness fix), set state `Reconnecting` (status ctx stops hardcoding
  `Connected`; a `conn_state` variable),
- attempt `reestablish` per backoff, still serving IPC (hellos answer
  `Reconnecting`, refs accepted); success ⇒ swap `conn`, `last_send = None`
  (immediate first heartbeat re-requests ident), state `Connected`,
  edge-log `mux wire reconnected`.

Loop-level test (existing in-process harness + `ipc_observer` /
`wait_status_contains`): peer answers ident, goes permanently silent →
observe re-requested ident caps at the fake peer, `state=reconnecting` +
`remote=unknown` on the status line, `reestablish` called, second peer
serves an agent round trip, status returns `connected` with the new ident.

### 5. Resume fast-path wiring

Track wall clock (`SystemTime`) alongside `now_ms` per iteration; feed
`suspend_gap` → gap > `PEER_TIMEOUT` ⇒ immediate dead verdict (reason
`resume gap`), gap > `2 × HEARTBEAT_INTERVAL` ⇒ arm the probe now. Policy is
pinned by the Task 2 pure tests; the loop wiring is a thin call.

### 6. Docs + closure

- FDR 0014: reconnect section (the daemon now survives remote loss).
- `doc/posh.1.scd`: `mux ls` state values gain `reconnecting`; note that
  `remote=unknown` on a previously-identified wire means reconnect in
  progress.
- AGENTS.md debugging notes: the #161 zombie guidance becomes historical
  (daemon self-heals; `debug-posh-mux-log` shows the dead/reconnected edges).
- Final commit `Closes #162`; note the incident evidence on the issue.
