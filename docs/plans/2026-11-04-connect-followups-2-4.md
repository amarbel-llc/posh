# Connect follow-ups 2-4, and the shared-master failure modes, 2026-11-04

Docs only; no code changes. This plans the three remaining ranked findings
of `2026-11-04-slow-connect-poor-links.md` (item 1, the ssh logins, shipped
as `POSH_SSH_SHARE` at 85c0b55). It also records how the shared ssh master
can fail, and how to tell from a real run whether it hangs the picker.
Code references are to master at 85c0b55.

## Recommended order

1. **Finding 4 first** (first frame in 1 RTT): a daemon-only change. It helps
   every mux connect, lossless too, needs no wire format change, and degrades
   to today's behaviour against an old remote.
2. **Finding 3 next** (faster first retransmit): a local change on both
   sides with no wire change. It removes the 1-3 s stalls under loss. Doing
   it after 4 means the first DATA it resends is already the one that links
   the session.
3. **Finding 2 last** (no 15 s cliff while progressing): the only one that
   changes failure UX. With 3 and 4 in, the cliff is hit far less often, so
   its new patience costs less, and the measurements can show the remaining
   cases on their own.

Each lands test-first with its own env override, as fix 1 did.

## Finding 4: the first frame in 1 RTT instead of 2

**Cause, now confirmed in the code.** The local mux daemon queues the
client's first `ClientMessage` until the remote confirms the OPEN
(`IpcSession.queued`, `remote/mux.rs:691-700`; the send pass at
`mux.rs:2121-2165`). The remote admits the OPEN, answers it with an Empty
confirm, but links the session daemon only when the first `SESSION_WIRE_DATA`
(the client's size and caps) arrives (`remote/server.rs:998-1058`,
`PeerChannel::Awaiting` → `Linked`). That costs a second RTT.

**Change (outline).** In the mux daemon's send pass (`mux_loop`, the
open-until-confirmed loop at `mux.rs:2121-2165`), send the newest queued
`SESSION_WIRE_DATA` for the channel **immediately after each OPEN
transmission**, in the same pass, instead of holding it until the confirm.
Keep queuing anything later as now. The confirm path then flushes only what
was not already sent.

- **Wire:** no format change. The OPEN body (RFC 0015 §3) and the
  opcodes are untouched. A DATA that arrives before its OPEN (reordering, or
  a lost OPEN) is dropped by the remote as a §3.3 straggler
  (`server.rs:999-1000`). The next OPEN retransmission carries the next copy.
  RFC 0011 §3.3 / the M2 notes should gain one sentence that the opener MAY
  send its first DATA right after the OPEN: a behavioural note, not a format
  change.
- **Skew:** an old remote runs the same Awaiting → Linked code (it predates
  nothing here), so it links at 1 RTT too. An old local daemon simply keeps
  today's 2 RTT.
- **Do not** carry the first message inside the OPEN body instead. RFC 0015
  §3 decodes an unrecognised tail as part of the target, so an old remote
  would fail the open and every connect would fall back.

**Default change / override.** Default: the first frame arrives one RTT
sooner on every mux attach. `POSH_MUX_EAGER_OPEN=0` restores
hold-until-confirmed.

**Red tests first.**
- `mux::tests::first_session_data_rides_right_after_the_open`: drive
  `mux_loop` in-process with a fake peer (the existing harness), queue one
  `SessionMsg` behind an unconfirmed open, and assert the peer receives OPEN
  then DATA in the same iteration, before any confirm.
- `mux::tests::an_open_retransmit_carries_the_newest_queued_data_again`: the
  OPEN is dropped; the retransmit pass re-sends the OPEN and the latest DATA.
- `mux::tests::eager_open_gate_off_holds_data_until_confirmed`: the pure
  gate parser (default on; 0/false/off/no off), and with the gate off the
  harness sees no DATA before the confirm.
- `remote::server::tests::data_before_its_open_is_dropped_then_links_on_the_next_pair`:
  pins the skew/reorder safety the change relies on.

**Netem proof.** `just debug-netem-connect` at 300/0 and 800/0, mux=1,
runs=3, plus `just debug-netem-picker` at 800/0 share=1 for the connect
phase.
- Before: in the logs, `session confirmed` at about 1 RTT and the first
  screen at about 2 RTT (measured 627-649 ms warm at 300, 1628-1648 ms at
  800).
- After: the first screen at about 1 RTT plus the daemon link time
  (expect about 330-380 ms at 300 and about 850-900 ms at 800).
- The 300/20 and 800/20 rows should also improve by about 1 RTT on
  attempts with no loss on the handshake.

**Risks.** Wire: none (behavioural RFC note only). Skew: safe both ways, as
above. Interaction with fix 1: none (UDP leg). One edge: a queued message
that the client later supersedes (a resize before the confirm) must still be
sent last. Sending the newest queued state message each time keeps that
invariant.

## Finding 3: a faster first retransmit under loss

Two separate timers:

- **3a, the OPEN** is retransmitted at `conn.rto()` (`mux.rs:2132-2134`),
  which before any RTT sample is the RFC 6298 placeholder: srtt 1000 ms,
  rttvar 500 ms, clamped to `MAX_RTO` 1000 ms (`datagram.rs:13-14`,
  `:145-151`, `:170-174`).
- **3b, the client's first message** is resent only at the 3 s
  `HEARTBEAT_INTERVAL`. Its outbox is empty and it carries no flags, so the
  RTO arm never fires (`remote/client.rs:2010-2013`, `:2481-2484`;
  `posh-proto/src/frame.rs:14`). On the mux path the daemon forwards it
  once, so a wire loss also waits for the client's next heartbeat.

**Change (outline).**
- **3b, client:** treat "no frame heard yet" like a non-empty outbox when
  computing the next send. In `drive_client`, the deadline at
  `client.rs:2010-2013` and `session_due` at `:2481-2484` add
  `|| !heard`. The interval is the wire's RTO on UDP. On the mux path,
  `MuxSessionTransport::rto()` is 50 ms before a hint (srtt 0, clamp
  50..1000, `mux.rs:2474-2477`), which would flood, so use a pre-establish
  floor (for example `max(rto, 500)`). Factor the decision into a pure
  `next_send_due(now, last_send, rto, heartbeat, outbox_empty, flags, heard)`
  so it is testable.
- **3a, daemon:** send the **first** OPEN (and, with finding 4, its DATA)
  twice, about 50-100 ms apart. A 20% loss on a single send becomes about 4%
  on the pair. Retransmissions keep the RTO cadence. Duplicates are already
  legal: the remote treats a repeated OPEN as a retransmit and re-confirms it
  (`server.rs:979-996`), and session messages are idempotent state sync.
- **Optional 3c:** seed the daemon's RTT estimator for a seeded wire from
  the bootstrap. The first `POSH CONNECT` arrives after a known number of
  ssh round trips, which gives only a coarse estimate, so treat this as a
  later refinement and not part of the default change.

**Default change / override.** Default: before the first frame, a lost
handshake packet costs about one RTO (≤1 s) instead of 1-3 s, and the first
OPEN is doubled. `POSH_FAST_ESTABLISH=0` restores single sends and
heartbeat-paced first-message resends.

**Red tests first.**
- `client::tests::the_first_message_is_resent_on_the_rto_until_a_frame_is_heard`:
  `next_send_due` with an empty outbox and no flags is due at
  `last_send + rto` while `heard` is false.
- `client::tests::an_established_client_keeps_the_heartbeat_cadence`: the
  same inputs with `heard` true are due at `last_send + HEARTBEAT_INTERVAL`
  (pins today's steady state).
- `client::tests::the_mux_pre_establish_resend_has_a_floor`: the mux wire's
  50 ms pre-hint RTO is raised to the floor.
- `mux::tests::the_first_open_is_sent_twice`: in the harness, the peer sees
  two OPEN copies before one RTO has elapsed, then RTO-paced retransmits.
- `client::tests::fast_establish_gate_off_restores_the_heartbeat_resend`
  plus the gate parser.

**Netem proof.** `just debug-netem-connect` at 300/20 and 800/20 (mux=1,
runs=5 for variance), 300/20 with mux=0, and 300/0 as the no-regression
control.
- Before (measured): cold 300/20 3402-3666 ms; OPEN sends about 1 s apart;
  gaps of about 3 s between confirm and frame (for example 368 → 3384 ms).
- After: no 3 s gaps in the logs; OPEN retransmits at the RTO; cold 300/20
  median toward 1-1.5 s.
- 300/0 unchanged (about 0.7 s).

**Risks.** Wire: none. Skew: none (receivers already absorb retransmits).
Bandwidth: a handful of extra small datagrams per connect. Interaction with
fix 1: none. Interaction with 2: a faster establish means the 15 s deadline
is reached less often, so do 3 first.

## Finding 2: the 15 s first-frame deadline, then a serial re-bootstrap

**Cause.** `drive_client` gives up after `POSH_CONNECT_TMOUT` (default 15 s)
without a frame (`client.rs:1983-1987`, `:2410-2426`). On the mux wire,
`cmd_ssh_session` treats that as an establish failure and falls back to the
per-invocation path, which runs a second ssh bootstrap and a fresh UDP
handshake with its own 15 s window (`main.rs:1432-1441`, then `:1453-1485`).
The 15 s exists for posh#162: an endpoint stuck reconnecting cannot answer
an agent prompt, while the foreground fallback can.

**Change (outline).** Make the mux first-frame deadline depend on what the
endpoint reported:
- `ensure_mux` already returns the endpoint state from `MuxHelloAck`
  (`MuxHandle::state()`, `mux.rs:2314`). Thread it into the
  `MuxSessionTransport` (`open_session`, `mux.rs:2344-2370`).
- Add a pure `mux_first_frame_deadline(state, env, connect_tmout) ->
  Option<u64>`:
  - `Connected` → no client-side deadline. Rely on the daemon's own
    give-up, which already exists: 60 OPEN sends at ≤1 s each, then a
    `SessionClose` that the client already treats as a fallback cue
    (`mux.rs:2124-2204`).
  - `Reconnecting` / `Bootstrapping` → keep the fixed deadline.
- `drive_client` (`client.rs:2410-2426`) uses that deadline for
  `Wire::Mux`; `Wire::Udp` is unchanged.
- Optionally let the fallback bootstrap reuse the fix-1 master
  (`control_path` in `cmd_ssh_session`'s `SshOptions`, `main.rs:1466-1478`)
  so a fallback costs about 2 RTT of ssh instead of a fresh login. This
  needs a check that a slave session's `-A`/`-a` choice behaves as intended
  (agent forwarding over a multiplexed connection uses the master's agent).

**Default change / override.** Default: a mux attach to an endpoint that
reported `Connected` keeps waiting while the endpoint is still trying,
instead of abandoning it at 15 s for a second bootstrap. The modal should
say so (for example "remote reachable, waiting for the session"). A
reconnecting endpoint keeps the 15 s fallback. `POSH_MUX_FALLBACK_TMOUT=15`
restores the fixed deadline for every mux attach (any number of seconds;
unset means "wait for the endpoint's verdict when connected").

**Red tests first.**
- `client::tests::a_connected_endpoint_has_no_client_side_first_frame_deadline`
- `client::tests::a_reconnecting_endpoint_keeps_the_fixed_deadline`
- `client::tests::mux_fallback_tmout_env_restores_the_fixed_deadline`
  (`Some("15")` → 15 000 ms for any state; unparsable → the default rule).
- `mux::tests::an_unanswered_open_closes_the_channel_after_its_budget`:
  probably already covered; pin that the `SessionClose` payload is the
  fallback cue the client acts on.
- `main::tests::the_mux_establish_failure_still_falls_back_on_a_close`:
  `mux_establish_failed` on the close error stays true.

**Netem proof.** The cliff needs more than 15 s to the first frame, so use
harsher points: `just debug-netem-connect` at 800/40 and 1500/30 (mux=1,
runs=3).
- Before: some attempts log `[mux-fallback] ... not established within
  15s` followed by a second `ssh-stub start` (a serial re-bootstrap), with
  totals above 15 s plus another establish.
- After: no fallback lines and one ssh-stub start per attempt; the total is
  the time to the first frame.
- Control: 300/20 unchanged.
- The `Reconnecting` branch is not reachable with these recipes (it needs
  a wire death mid-attach), so it rests on the unit tests.

**Risks.**
- Wire: none.
- Local IPC: none, because the hello state already exists in every
  `MuxHelloAck` generation.
- Skew: an old daemon reports state the same way.
- Behaviour: an endpoint that reports `Connected` but whose remote never
  answers (for example a pre-M2 remote) now costs up to the daemon's OPEN
  budget (≤60 s) instead of 15 s before falling back. Lowering
  `SESSION_OPEN_RETRANSMITS_MAX` for initial opens (for example 30) bounds
  that.
- Interaction with fix 1: the fallback bootstrap does not use the master
  today (`control_path: None` at `main.rs:1466-1478`), so a fallback still
  pays a fresh login. That is why the optional reuse above is worth
  considering.

## (a) A stale shared master: what the first `ph host:` looks like

Sources:
- `ssh_config(5)` of the installed OpenSSH 10.5p1, read with `just
  debug-ssh-config-sharing-doc` (added here because `ssh_config(5)` is not
  on the agent's MANPATH).
- The OpenSSH source as I remember it (`mux.c`, `ssh.c`
  `control_persist_detach`). I did not re-read the source in this work, so
  everything taken from it is **inferred**.

**Verified from the docs:**
- A client "will fall back to connecting normally if the control socket
  does not exist, or is not listening".
- `ControlPersist=<seconds>` ends an idle backgrounded master after that
  many seconds with no clients.
- `ServerAlive*`: with `ServerAliveInterval` 15 and the default count of 3,
  "ssh will disconnect after approximately 45 seconds". By the same
  arithmetic, posh's 15 × 2 is about 30 s.

In posh, the list probe runs **before** any terminal takeover (`picker::rows`
→ `remote_list_output`, `main.rs:1622-1651`). The seed bootstrap runs
**inside** the establish modal (`mux.rs:1283-1300`).

| # | state of `<base>/ssh/%C` | what ssh does | cost | what the user sees | verified? |
|---|---|---|---|---|---|
| M1 | no socket | fresh login, becomes the master | one fresh login (the pre-fix cost) | nothing unusual | docs |
| M2 | socket file, nobody listening (master killed or crashed; host slept long enough for the master to exit uncleanly) | falls back to a normal connection; with `auto` it unlinks the stale socket ("stale control socket") and becomes the new master | one fresh login | nothing unusual | fallback: docs; unlink and re-master: inferred from source |
| M3 | master alive, its TCP dead (roam wifi → cellular, NAT rebinding, suspend/resume) inside the 60 s persist window | the slave connects to the live master, which writes the new session request into the dead TCP. Nothing answers until the master's ServerAlive gives up (`ServerAliveInterval=15`, `CountMax=2`). The master then exits (and, inferred, removes its socket). The slave errors out; for that one invocation it does **not** fall back to a fresh connection (inferred). `ConnectTimeout` does not apply to a slave | up to about 30 s counted from the master's last received data, so 0-30 s depending on how long it had already been idle; then the failure path below | **List probe:** no takeover yet, so the terminal just pauses (blank) for up to about 30 s. Then stderr shows `posh: <host>: remote list failed ...` with ssh's error, and the picker opens with **only "+ create new session…"**, because `picker::rows` skips a failed host (`picker.rs:88-96`). That is misleading: it looks as if the host has no sessions. **Seed bootstrap:** the modal shows the ssh pane silent for up to about 30 s, then the mux fallback warning, then a fresh per-invocation login | hang bound: docs arithmetic; no slave fallback and socket removal: inferred |
| M4 | master alive, TCP alive but slow or lossy | shares normally (2 RTT per session open; TCP head-of-line blocking applies) | the intended saving | nothing unusual | docs |
| M5 | master process stopped (SIGSTOP) or wedged | the slave waits; the master's own ServerAlive timer cannot run | unbounded until Ctrl-C | a pause with no end | inferred |
| M6 | two first connects at once (two `ph host:` in parallel) | one becomes the master; the other cannot bind the path and continues without multiplexing ("ControlSocket … already exists, disabling multiplexing") | one extra fresh login, no hang | nothing unusual | inferred |
| M7 | the control path is too long for a unix socket (a very deep `POSH_DIR`) | ssh cannot listen and continues without multiplexing | sharing silently off | nothing unusual | inferred (posh's `%C` keeps the leaf at 40 characters) |
| M8 | `<base>/ssh/` cannot be created or fails the ownership check | posh passes no control options at all (`shared_control_path` → `None`) | sharing off | nothing unusual | code (`sshwrap.rs` `shared_control_path`) |

**M3 is the one that matters.** It needs a `ph host:` (or a typed `posh list
host:`) within about 60 s of the previous one **and** a network change in
between: the "close the laptop, walk out, reopen on cellular, `ph flic:`"
case. Proposed mitigations, not done:
- Bound the list probe with its own deadline (for example 2 × the
  keepalive). On expiry, run `ssh -O exit` on the control path and retry
  once without sharing.
- Have the picker say "listing failed" in place of an empty list, so a
  failed host cannot pass for a host with no sessions. That is a UX bug
  independent of sharing (it exists for any failed list).
- Optionally probe the master with `ssh -O check` before reuse. This only
  proves the master process is alive, not its TCP, so it does not catch M3
  on its own.

## (b) Review caveat: does the ControlPersist master hang the picker?

**The concern.** The picker's list probe runs under `Command::output()`
(`main.rs:1651-1654`), which returns only when the child exits **and**
stdout and stderr reach EOF. When that ssh becomes the master, OpenSSH
forks: the parent stays the mux client that runs `posh list --json`, and the
child becomes the background master. If the background master kept the
inherited stdout/stderr pipes open, `output()` would block until the master
exits. With `ControlPersist=60` and no other client, that is about 60 s.
The picker would then appear only after that wait, and because the master is
gone by then, the seed bootstrap would pay a fresh login after all.

**Expected behaviour (inferred, not verified).** OpenSSH's
`control_persist_detach()` redirects the backgrounded master's stdin and
stdout to `/dev/null`, and its stderr too unless ssh runs with `-v`, before
daemonising. If so, no pipe stays open and there is no hang. The risk case
is an ssh run with `-v` (debug logging to stderr), which posh never passes.
The 2026-11-04 netem runs cannot show this: their ssh is a stub.

**What to look for in the operator's summary**
(`scripts/posh-ph-flic-timing.bash`, `summary.txt` and `ssh.log`):
- The wrapper writes its `end` line when the **foreground** ssh exits. In
  the hang case that is still prompt, because the background master holds
  the pipe, not the wrapper. So a list-ssh `end` about 60 s after its
  `start` is **not** the signature to expect; the hang sits inside posh,
  after `end`.
- The signature is two things together:
  1. a gap of 60 s or more between the list ssh's `end` and the bootstrap
     ssh's `start`, and
  2. a bootstrap ssh that takes as long as the list ssh did (a fresh
     login, because the master has already expired), not about 2 RTT.
  The operator would also see the picker take about a minute to appear,
  with a blank terminal meanwhile.
- **The current output cannot fully separate this from the operator's own
  choice time.** The gap between list `end` and bootstrap `start` includes
  however long the operator looked at the picker, and the script records no
  stamp for "picker drawn". Indicator 2 (the bootstrap ssh's duration) does
  not depend on choice time and is the reliable tell in today's output.

**Proposed script tweaks (not made):**
1. A background watcher in the script that polls (about every 100 ms) for
   the `posh-palette` child of this `ph` (`pgrep -n -f posh-palette`, which
   posh spawns for the picker) and writes `picker-up <ms>` to `ssh.log`.
   This gives list `end` → `picker-up` (the hang, if any) and `picker-up` →
   bootstrap `start` (the choice time).
2. After the list ssh's `end`, have the wrapper record `ssh -O check -o
   ControlPath=<base>/ssh/%C <host>` and its time. "Master running
   (pid=N)" proves the master exists, and comparing it after the bootstrap
   shows whether the bootstrap reused it.
3. Add `ssh -V` to the summary, since this behaviour is OpenSSH-version
   dependent.
