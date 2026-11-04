# Why `ph flic:` connects slowly on poor links, 2026-11-04

Question from the operator: on a poor link (high latency, loss, flaky wifi or
cellular), `ph flic:` can take a very long time to show a session. Where does
the time go, and what would make it faster?

Method: I read the code at `500215e` and ran a netem measurement inside a
private user+net+pid namespace (`just debug-netem-connect`, described below).
Evidence cited as `file:line` was seen in this worktree and may move. No
runtime behaviour was changed. A coordinator memory hold paused the
measurements partway through; the remaining rows were run one at a time after
it was lifted.

## What `ph flic:` is

- `ph` is posh's own front door (FDR 0015), the same binary as `posh`. I found
  no operator alias or wrapper for it in `~/eng/repos/circus`. Only one
  justfile comment mentions `ph host:+`. A wider search of `~/eng` was refused
  by the permission broker, so a wrapper elsewhere in the home config is not
  ruled out.
- `ph flic:` (trailing colon) is NOT an attach. It is the **host-scoped
  picker** (`PhRoute::PickerHost`, `crates/posh/src/main.rs:954-956`, dispatched
  at `main.rs:1005` to `cmd_ph_picker`, `main.rs:1051-1082`). The picker first
  runs `picker::rows`, which makes a **separate, complete ssh connection** for
  `posh list --json` (`picker.rs:88-96`, then `remote_list_output`,
  `main.rs:1622-1651`). Only after you choose a row does it re-enter `dispatch_ph`,
  and `cmd_ssh_session` does the actual attach (`main.rs:1337-1486`).
- flic is this host. `ph flic:` run from flic dials flic's own tailnet address,
  so the link is local and fast. The slow case must come from a different
  client host, such as a laptop on wifi or cellular. I could not tell which
  host that is.

## The establishment path, step by step (cold endpoint, mux on, on a tty)

Serial steps run in order, and each one waits for the step before it.
"RTT" means one round trip on the poor link.

| # | step | where | network cost | waits for a full timeout? |
|---|---|---|---|---|
| 1 | dial decision for the list probe: `ssh -G`, then `getaddrinfo(flic)`, then, if that fails, `tailscale status --json` | `sshwrap.rs:425-440`, `:562-580`, `:607-612`; `tailnet.rs:94-103` | local, but no bound on DNS or `tailscale` | getaddrinfo uses the resolver's own timeouts; `tailscale status` has no bound |
| 2 | **picker list ssh**: a fresh TCP+SSH connection, auth, exec of `posh list --json`, exit | `picker.rs:90`, `main.rs:1637-1640` | about 7-9 RTT for a fresh OpenSSH connection, plus TCP loss recovery, plus remote login-shell start | no `ConnectTimeout` or `ServerAlive*` is set (`main.rs:1604-1617`) |
| 3 | the user picks a row | `main.rs:1064` | none | human |
| 4 | terminal takeover and modal | `main.rs:1380-1384` | none | — |
| 5 | dial decision again (same as 1) | `sshwrap.rs:700` via `bootstrap` | local | as 1 |
| 6 | **endpoint bootstrap ssh** (`posh-server new --channels agent --client-id …`), hosted in the modal | `mux.rs:1283-1300` → `sshwrap.rs:690-726`, `:783-861` | about 7-9 RTT plus the remote `posh-server` start | `-o ConnectTimeout=10` is set (`mux.rs:1200`, `:1208-1221`); no ServerAlive |
| 7 | linger after `POSH CONNECT` until ssh exits, at most `SSH_EXIT_GRACE` = 3 s, then `modal.finish(3 s)` | `sshwrap.rs:32`, `:851-859` | at least 1 RTT for ssh's channel close / exit-status | up to 3 s if ssh does not exit by itself |
| 8 | mux daemon double-fork and hello | `mux.rs:1327-1351`, `:2526-2559`; `HELLO_TIMEOUT` 30 s (`mux.rs:2267`) | local unix socket | 30 s if the daemon never answers |
| 9 | session open, granted locally, then the OPEN goes on the wire | `mux.rs:2344-2353` (waits `HELLO_TIMEOUT` for the local ack), `:2121-2165` | 1 RTT to confirm. A lost OPEN is resent per RTO, and the RTO is **1000 ms before any RTT sample** (`datagram.rs:13-14`, `:145-151`, `:170-174`). Up to 60 sends (`mux.rs:679`) | about 60 RTOs before the open gives up |
| 10 | the client's first message (size and caps) is queued until the OPEN is confirmed, then sent; the remote replies with the first content frame | queueing `mux.rs:691-700`; client send `client.rs:2004`, `:4073-4114` | **a second RTT after the confirm** (measured, below) | — |
| 11 | resend if the client's first message is lost | `client.rs:2010-2013`, `:2481-2484`; `HEARTBEAT_INTERVAL` 3000 ms (`posh-proto/src/frame.rs:14`) | the first message has an empty outbox and no flags, so a resend happens only at the **3 s heartbeat**, not at the RTO | 3 s per loss |
| 12 | first-frame deadline: `POSH_CONNECT_TMOUT`, default 15 s | `client.rs:1983-1987`, `:2410-2426` | — | **15 s**, then `mux_establish_failed` → fall back (`main.rs:1432-1441`) to the per-invocation path, which runs the **whole bootstrap again** (a new ssh at step 6, plus a fresh UDP handshake with its own 15 s deadline) |

With a warm endpoint (connected within the last `POSH_MUX_PERSIST` seconds,
60 s by default, `mux.rs:158`, `:171`), steps 5-8 collapse to a unix-socket
connect. **Step 2, the picker's own ssh, is still paid on every `ph flic:`.**

Other timers on the path:

- Dead-wire detection on a stale warm endpoint: 15 s of silence plus a 10 s
  probe (`mux.rs:1013-1017`), then reconnect by ssh with backoff
  0/2/5/15/60 s (`mux.rs:1113-1121`). I saw it in a broken first run: "mux
  wire dead: probe unanswered (heard 25009ms ago)".
- The relay's own UDP port comes from the 60001-60999 range, bound
  server-side (`datagram.rs:12`). It is local and adds no wait.
- Name resolution runs once per ssh spawn (steps 1 and 5). Nothing is cached
  between the two spawns in one invocation.

## Measurement

`just debug-netem-connect rtt_ms loss_pct mux runs jitter_ms` (justfile,
`debug` group). It runs everything under `unshare --user --map-root-user
--net --pid --fork --mount-proc`. It refuses to apply netem unless
`ip -o link` shows `lo` alone (every run printed `netns links: lo`). It puts a
netem qdisc on that namespace's `lo` and runs the posh pair on 127.0.0.1. A
stub `ssh` runs the remote command locally, so **the ssh leg is not in these
numbers**: they cover only posh's own part (steps 6-12 minus ssh's network
cost). The first screen is detected by polling the tmux pane for a marker
printed by the session shell. Each run is a COLD attempt (a fresh `POSH_DIR`)
followed by a WARM one (lingering endpoint, existing session). All runs used
mux=1 (the default path).

Time to the first screen, in ms (posh's part only):

| RTT | loss | cold | warm |
|---|---|---|---|
| 0 | 0% | 121 | 96 |
| 300 | 0% | 710, 723 | 627, 649 |
| 800 | 0% | 1710, 1741 | 1648, 1628 |
| 300 | 5% | 686, 696, 686 | 646, 626, 1270 |
| 300 | 20% | 3666, 3402, 3426 | 1184, 1414, 1322 |
| 800 | 5% | 1783, 1704 | 1636, 3861 |
| 800 | 20% | 2692, 2120, 2858 | 2678, 1669, 4677 |
| 300 | 20%, **mux=0** | 386, 3367 | 2373, 2391 |

With mux=0 (the per-invocation relay path) the stub ssh runs on every
attempt, so in real use that row also pays a fresh ssh every time. Its
lossless best case is about 1 RTT (386 ms), not 2, because the client's
first message goes straight to the relay. Under loss it shows the same
1-3 s stalls, with the client sitting at the 1000 ms placeholder RTO.

What the per-attempt logs show:

- **Without loss, the first screen takes 2 RTT after the OPEN.** At 800 ms
  RTT: OPEN sent at +64 ms, confirmed at +864 (1 RTT, `first reply kind=0`,
  an empty frame), screen at +1710 (2 RTT). The same pattern holds at
  300 ms. This matches step 10: the client's first message waits for the
  confirm.
- **Loss before the first RTT sample costs whole seconds.** 300 ms / 20%
  cold: the OPEN needed 4 sends, about 1 s apart (65 → 3077 ms; the
  1000 ms placeholder RTO), and the screen came at 3.4-3.7 s against 0.7 s
  without loss. In 300/20 run2-cold the OPEN was confirmed at +368 ms but the
  content frame arrived only at about +3384 ms, a 3 s gap. In 800/20
  run3-warm the gap was 850 → 4659 ms, and in 800/5 run2-warm it was
  834 → 3838 ms. All three match the 3 s heartbeat resend
  of a lost first client message (step 11).
- **The fallback cliff (step 12) is real.** In my first, misconfigured run
  (an inherited `SSH_CONNECTION` made the stub "remote" report an address
  that does not exist in the namespace), the mux session waited exactly 15 s
  (`mux session not established within 15s`), fell back, re-ran the
  bootstrap, and waited another 15 s before failing. This was a
  configuration fault, not a link fault. It shows the cost structure: a
  link bad enough to miss the 15 s first-frame deadline pays 15 s and then a
  second full ssh bootstrap.

The ssh leg, estimated rather than measured: a fresh OpenSSH connection is
about 7-9 serial RTTs (TCP, banner, KEX, NEWKEYS/service, auth query and
signature, channel open, exec, close). At 300 ms RTT that is about 2-3 s; at
800 ms it is about 6-7 s. A lost SYN costs the 1 s initial TCP RTO, then
doubles. A cold `ph flic:` pays this **twice** (steps 2 and 6) and a warm one
pays it **once** (step 2). The ssh cost therefore outweighs posh's own part
in every row of the table above.

## Ranked: where the time goes

1. **Serial fresh ssh connections: the picker list ssh plus the endpoint
   bootstrap ssh** (steps 2 and 6). This costs about 14-18 RTT before the
   first UDP datagram, and posh itself sets no connection reuse
   (`sshwrap.rs:649-683`, `main.rs:1604-1617`). The picker ssh is paid even
   with a warm endpoint. Estimated, not measured: about 4-6 s at 300 ms RTT
   and about 11-14 s at 800 ms, more under loss.
2. **The 15 s first-frame deadline followed by a full re-bootstrap**
   (step 12; `client.rs:1983-1987`, `main.rs:1432-1441`). On a very bad link
   the mux path can miss 15 s, and the fallback then pays another ssh and
   another 15 s window. Observed: 15 s + 15 s in a broken run.
3. **Loss before the first RTT sample**: a 1000 ms placeholder RTO for OPEN
   resends and a 3000 ms heartbeat for a lost first client message (steps 9
   and 11). Measured: 20% loss turns a 0.7 s posh-side establish into
   3.4-3.7 s cold, and individual gaps of about 3 s appear.
4. **The first frame takes 2 RTT after the OPEN** because the client's
   first message is queued behind the confirm (step 10). Measured: 1 extra
   RTT, about 0.8 s at 800 ms RTT.
5. **The ssh exit linger after `POSH CONNECT`** (step 7), at least 1 RTT
   and up to 3 s. Not measured: the stub exits at once.
6. **Unbounded name resolution** per ssh spawn (`getaddrinfo`, `tailscale
   status --json`), run twice per invocation. This matters only when DNS or
   tailscaled is itself slow on that link. Not measured.

## What helps today (no code change)

- **Name the session: `ph flic:NAME` instead of `ph flic:`.** It skips the
  picker and its list ssh (`main.rs:1040-1044`; "no probe round-trip is
  needed"). This removes item 1's first ssh on every connect.
- **Keep the endpoint warm**: `POSH_MUX_PERSIST=<seconds>` (default 60;
  `man posh`, `mux.rs:171`). A later connect within that window skips the
  bootstrap ssh (steps 5-8).
- **ssh connection sharing in the client's own ssh config**
  (`ControlMaster auto`, `ControlPath`, `ControlPersist`) for the host. posh
  runs plain `ssh` and sets no control options, so an existing master is
  reused: the picker ssh and the bootstrap ssh then cost about 1-2 RTT each
  instead of 7-9. Not tried here. Also check that the master does not end up
  owned by the detached mux daemon.
- **`POSH_CONNECT_TMOUT=<seconds>`** (`client.rs:1983`; 15 by default, 0
  disables) can be raised on a known-bad link, so a slow but progressing
  establish is not abandoned at 15 s for a second full bootstrap. It is
  not documented in the man pages (a doc gap).
- `POSH_MUX_SESSIONS=0` / `POSH_MUX=0` do **not** help: they force a fresh
  ssh bootstrap on every connect.

## Proposed code changes (each to be done test-first later)

1. **`ph host:` should not open a second ssh to list.** Serve `list --json`
   over a warm mux endpoint, or draw the picker at once from the completion
   cache and refresh it in the background. When the endpoint is cold, start
   the endpoint bootstrap in parallel with the listing, or return the
   listing from the bootstrap itself.
2. **Do not fall back after progress.** If the OPEN was confirmed (the remote
   is reachable), keep waiting for the frame instead of firing the 15 s
   per-invocation fallback. Fall back early only when nothing at all has been
   heard.
3. **Resend the first client message on the RTO, not the heartbeat**, while
   no frame has been heard (treat "not established" as pending, like a
   non-empty outbox, at `client.rs:2011`, `:2482`).
4. **Seed the RTO from the bootstrap**: use the measured ssh handshake time,
   or a first-reply timestamp, instead of the 1000 ms / 500 ms placeholder,
   for both the mux wire and the relay.
5. **Save the second RTT**: carry the client's first message (size and caps)
   in `SESSION_WIRE_OPEN`, or let the remote send a frame as soon as it
   opens. Either change touches RFC 0011 / RFC 0015, so it needs an RFC
   update.
6. **Do not wait for ssh to exit** after `POSH CONNECT` in the modal driver.
   Hand the child to a background reaper and continue
   (`sshwrap.rs:851-859`).
7. **Resolve once per invocation, with bounds**: cache the `SshDest` decision
   between the list and the bootstrap, and put a time limit on `tailscale
   status --json`.
8. Document `POSH_CONNECT_TMOUT` in `posh-client(1)` ENVIRONMENT.

## Not verified

- **The real ssh leg was not measured.** The netem runs used a stub `ssh`.
  The 7-9 RTT figure for a fresh connection is protocol arithmetic, and how
  TCP behaves under loss was not exercised.
- Which client host the slow `ph flic:` was seen from, and that host's DNS,
  tailscaled and ssh config. In particular, I did not check whether a
  `ControlMaster` is already configured. Fleet ssh/tailnet config was out of
  scope.
- Remote login-shell and nix-profile start-up time on flic for the
  `posh-server` / `posh list` exec.
- How the modal's ssh-exit linger (step 7) behaves with real ssh.
- That the 2-RTT first frame comes from the client message being queued
  behind the OPEN confirm, and that the 3 s gaps come from the heartbeat
  resend of a lost first message. Both are inferred from timing plus the
  code, not traced packet by packet.
- Not run: any jitter, mux=0 at other RTT/loss points, and the 0 ms row
  more than once. Sample sizes are 1-3 per cell, so the tails (the 3.8-4.7 s
  warm outliers) are anecdotes, not distributions.
- No `ph` wrapper outside `~/eng/repos/circus`: a search of `~/eng` and a
  listing of `~/eng` were refused by the permission broker.
