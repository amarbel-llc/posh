---
status: experimental
date: 2026-10-06
---

# Flood delivery (paced screens for a viewport that cannot keep up)

## Problem Statement

posh#225: a session that out-produces a viewport dropped it. The daemon built
one visible frame per PTY read and queued every one, so a flood (`cat` of a
large file, a chatty build) grew a viewport's unsent backlog until it crossed
`MAX_CLIENT_BACKLOG` (16 MiB) and the daemon hung up on it — with no
explanation (posh#226). The viewport did not need those screens: by the time
it could draw one, a newer one existed.

Stage 1 shrank each frame to the viewport's screen (`dump_vt_mirror` instead
of the whole ring). This feature, Stage 2, bounds the queue: a paced viewport
is sent at most one screen at a time, and that screen is the newest. Stage 3
gives such a viewport its history as addressed RFC 0009 v2 bodies — rows
addressed, acknowledged, and resent only from the viewport's acknowledgement — so
history neither re-carries the ring nor costs the screen its diff base.

## Interface

- **`POSH_PACED`** on the viewport (the remote client). Default on for a
  remote attach over the mux (M2); `0`, `false`, `off` or `no` turns it off.
  Read once per attach, so a change takes effect on the next attach without
  restarting the session. When on, the viewport advertises `CAP_PACED`
  (RFC 0001 id 23) on every message and the M2 bridge carries it into the
  daemon's Init. The palette's About view lists `POSH_PACED` with the other
  gates, `on` only when this attach rides a mux session channel (a relayed
  attach is not paced, below).
- **What the user sees:** during a flood the live screen jumps to the latest
  state rather than replaying every intermediate one, and at most one screen
  is in flight to the viewport. What the user sees and what their keystrokes
  act on stay close in step — the screen after a Ctrl-C mid-flood arrives at
  the next send opportunity (at most `PACED_ACK_WAIT_MS` away), though behind
  any history already queued ahead of it — one history body of at most 256
  rows for a v2 viewport (below); up to a ring, ~1 MB, for a v1 one. The
  program in the session is never slowed: the PTY is read and the terminal
  fed exactly as before.
- **History (RFC 0009 v2):** a paced viewport that advertises `SCROLLBACK2`
  (every current roaming viewport) gets its scrollback from the daemon as
  RFC 0009 v2 bodies: rows addressed in an epoch-scoped row space and
  acknowledged cumulatively, resent from the acknowledgement, one body per
  send opportunity, at most `HISTORY_WINDOW_ROWS` (512 rows) in flight. Rows the session's ring evicted before their turn are skipped as
  one forward jump. A reconnect continues the viewport's epoch, so its ring
  is kept and history resumes forward-only, as before (posh#225 Stage 3). A
  paced viewport that does not advertise it, and every unpaced one, keeps v1
  history unchanged.
- **Interactive cost:** typing faster than one round trip, each echo frame
  waits for the previous frame's ack (at least `PACED_FRAME_FLOOR_MS`, and
  `PACED_ACK_WAIT_MS` when an ack is lost) — one frame per RTT, as mosh
  does, by design.
- **Not paced:** a local `posh attach` (it does not advertise the capability
  yet — a later stage), and a viewport reached through the relay
  (`POSH_MUX_SESSIONS=0`): the relay does not forward `CAP_PACED` (ADR 0007).
  Both get today's per-read delivery whatever `POSH_PACED` says.
- **Diagnostics:** the session daemon's log (`<base>/…/<session>.log`)
  carries, per paced viewport, `paced ack latency fd=… paced=1
  ack_ms=<last>/<srtt>/<min>/<max> ack_n= ack_age_ms= new=` at most every
  10 s while its frames are acked, and the same `ack_ms`/`ack_n`/`ack_age_ms`
  fields on its `client disconnected` line. `ack_ms` is the round trip of a
  paced screen frame, from when the daemon queued it to when its ack
  arrived (daemon → bridge → link → viewport → back); each frame is sampled
  at most once — an ack that confirms several frames samples only the
  newest logged frame at or below the acked number, so an ack naming a v1
  scrollback slot times the visible frame below it — whether or not a newer
  frame was already in flight. A v2 history body is never a sample (it is
  acknowledged by row, not by `FrameAck`). `srtt` is smoothed like TCP's,
  `ack_age_ms` is the time since the last ack of any kind, and `new=` counts
  samples since the previous line (posh#225 Stage 3.0). This series is
  Task 3.4's input.

The wire contract is RFC 0008 §3.2, and RFC 0009 §5 for v2 history on the
session socket.

## Examples

`ph host:s` then `cat` a multi-megabyte log: the viewport shows the log's
latest screenful as often as the link acknowledges one, while the session runs
at full speed, and stays attached when the `cat` finishes, showing its last
screen. Before this feature a viewport that fell 16 MiB behind was dropped
mid-`cat`.

`POSH_PACED=0 ph host:s` attaches the same session with today's delivery;
another viewport on that session may stay paced.

## Decisions

The user-facing decisions are the flood-delivery UX design's (2026-10-05,
`docs/plans/2026-10-05-flood-delivery-ux-design.md`): decision 2 (the live
screen jumps to latest), 5 (the program is never slowed), 6 (a viewport that
stops reading stays attached) and 13 (rollout split by path, the switch on the
viewport). Settled for this stage, 2026-10-06:

1. **A send opportunity is ack-or-wait.** A paced viewport is sent a fresh
   screen when its outgoing buffer is empty AND its last fresh frame is
   acknowledged or `PACED_ACK_WAIT_MS` has passed since it was queued. The
   daemon has no RTT for a viewport, but a bridged viewport's `FrameAck` is
   end-to-end, so waiting for it is backpressure from the real link; the
   wait keeps a lost ack from stalling the screen.
2. **A floor spaces fresh frames** (`PACED_FRAME_FLOOR_MS`) however promptly
   the viewport acks, capping encode work during a flood.
3. **The frame is built at the opportunity**, from the terminal as it is then;
   output only marks the viewport dirty, and so does every event that owes a
   frame (the attach replay, the RESYNC keyframe, the regeometry frame, an
   activity answer, an overlay source swap). Screens produced in between are
   never built, let alone queued. The poll timeout is the nearest
   opportunity, and `-1` when no paced viewport owes a frame (no busy-wait).
4. **v1 history rides the opportunity**, right behind the paced visible frame
   (posh#181 threading), so a scrollback frame is never queued per read —
   for a paced viewport without v2 (decision 8 replaces it for the rest).
5. **A RESYNC releases the ack wait** — the viewport rejected what was
   outstanding, so its ack is not coming. Nothing else does.
6. **A regeometry frame waits for the next opportunity** and does not release
   the ack wait: frames in flight for the old geometry are still valid input,
   so the new-geometry screen lands within one ack (≤ `PACED_ACK_WAIT_MS`).
7. **The session's last screen is flushed before `Exit`**, whatever the
   viewport's pacing. Only the screen: pending v2 history is not.

Settled for Stage 3 (addressed history), 2026-10-06:

8. **v2 is gated on pacing**, not just on the advertisement: the daemon opens
   a viewport's history cursor only when its Init carried both `CAP_PACED` and
   a well-formed `SCROLLBACK2` entry. `POSH_PACED=0` therefore returns a
   viewport to v1 history, and an unpaced viewport's bytes do not change.
9. **One body per opportunity, with a coin; the first tie goes to the
   screen.** The screen keeps decision 1's opportunity, floor included;
   history has its own — an empty buffer, and fresh rows with room in the
   window (due at once: the frame floor caps the screen's encode cost, and a
   body is a row copy) or rows in flight (the resend deadline). When both
   are due, the kind that did not go last goes (`server_loop`'s coin), and
   the first tie goes to the screen (live screen first, UX decision 3), so
   the backlog is at most one body. A history body rides the newest visible
   frame number without taking a producer slot, and is never acknowledged
   by `FrameAck` (RFC 0009 §2, §4).
10. **History is rate-limited by the window and the socket only**: at most
    `HISTORY_WINDOW_ROWS` (512 rows, two full bodies) in flight before the
    ack — a fresh body carries at most the room left in it — and a body only
    into an empty buffer. Throughput is about one window per round trip; the
    window is the daemon's static stand-in for `server_loop`'s SRTT-paced
    send interval (Task 3.4 makes it dynamic).
11. **Resend from the ack after twice the measured ack latency**, never under
    `PACED_ACK_WAIT_MS`, `HISTORY_RESEND_INITIAL_MS` before any sample, and
    doubled per resend without ack progress up to
    `HISTORY_RESEND_MAX_DOUBLINGS` times. The bridge stops retransmitting a
    body once the visible frame it rode is acked, so this resend is required
    for correctness, not an optimisation.
12. **The epoch bumps for a viewport on its own size change** (it cleared
    its ring). **A session width change, or a height grow, re-anchors
    every other viewport without a bump**: a width reflow renumbers the
    daemon's ring and a height grow pops ring rows back onto the grid
    without lowering the total, so the row space is re-anchored at the
    viewport's row count as of the resize — its ring and ack stay valid.
    Rows scrolled before the resize but not yet sent, and rows sent but
    lost, reach the viewport as ONE forward jump at that count (a resend
    from a lagging ack is floored there as an empty body), so nothing is
    silently skipped (RFC 0009 §5; UX decision 1). Another viewport attaching narrower, or leaving, therefore never
    clears this one's history (v1 never did). The epoch byte skips 0 on wrap
    (255 → 1): a viewport advertises 0 to mean it holds none.
13. **A new attachment continues the viewport's epoch at its count** (epoch
    0, held by a viewport that has none, opens a fresh epoch): a fresh epoch
    on every mux reconnect would make the viewport clear its ring.
14. **History comes from the session terminal and pauses while the escape
    overlay is up**; it does not pause on the alternate screen.

## Limitations

- **History flows at most one window per round trip** (`HISTORY_WINDOW_ROWS`
  = 512 rows per RTT, and no faster than the socket drains) for a v2
  viewport, so a flood that outruns that for longer than the ring loses its
  oldest rows as forward jumps — silent until Stage 4 draws it. The window
  is static until Task 3.4 (held for the field ack-latency data) sizes the
  history share by backpressure. Measured
  (2 MiB flood, 20,511 rows, 10,000-row ring): prompt acks deliver every
  row once at both 1 KiB/ms and 4 KiB/ms (≈ 40,000 rows/s), as a paced v1
  viewport does; RTT 50 ms delivers every row at 1 KiB/ms but 14,127 at
  4 KiB/ms (6,384 in 4 jumps; 512 rows / 50 ms ≈ 10,000 rows/s); RTT
  300 ms delivers 12,400 at 1 KiB/ms (8,111 in 13 jumps), and 1,500 and
  2,500 ms, or 300 ms at 4 KiB/ms, about the ring (10,512–10,774). No row
  is ever delivered twice below the first resend floor, none differs from
  the session's, and every row is acked. (A paced v1 viewport keeps
  Stage 2's cliff: above ≈ 5 × `PACED_ACK_WAIT_MS` RTT its base is lost and
  v1 history is withheld until the flood ends — 0 rows acked at 1,500 ms.
  The v2 viewport's visible base survived every measured RTT up to 2,500 ms:
  no screen was built without an acked base, against 3 for v1 at 1,500 ms;
  but the 2 MiB flood ends before a 2,500 ms ack could land, so that RTT's
  visible cliff is not exercised.)
- **A never-acking v2 viewport is re-sent at most one window (512 rows) per
  resend floor**, backing off to one per 8 × the floor (measured: rounds at
  1, 2, 4 and 8 s after the first window). This is bandwidth, not backlog:
  at most one body is ever queued. Over a 2 MiB flood and a 20 s tail it
  was sent 160–163 KB of history (1,536 rows); a paced v1 viewport
  that never acks is re-sent up to one ring every `PACED_ACK_WAIT_MS`
  (2.2–7.9 MB over the flood alone).
- **A history body lost on the wire while a later body is in flight is
  accepted by the viewport as a forward jump** and cannot be repaired by the
  daemon's resend-from-ack (RFC 0009 has no eviction-vs-loss marker); with
  several bodies in flight, a body lost under a flood becomes a hole Stage 4
  will draw as "not received". Removing the hole needs one BODY in flight,
  not merely a 256-row window: since bodies go without a floor they are
  often smaller than 256 rows, so `HISTORY_WINDOW_ROWS = SB2_ROWS_PER_BODY`
  alone no longer does it — an operator decision (posh#243).
- **A reader slower than the flood loses the rows the ring evicts before
  their turn**; for a v2 viewport each loss is a forward jump of exactly that
  span, and the reader ends holding the whole retained ring. The live screen
  still ends on the last screen. Measured (1 KiB/ms reader, 4 KiB/ms
  flood): 14,111 of 20,511 rows delivered, 6,400 lost in 7 jumps (prompt
  acks); 13,007 delivered, 7,504 lost (50 ms RTT) — more than the ~4,300 a
  v1 viewport never shipped (not yet analysed: v2 moves at most 256 rows per
  body and alternates bodies with screens), but with nothing repeated and
  one ≤ 26.7 KB body queued at a time (v1: one ring-sized ~1 MB frame).
- **A mismatched-geometry viewport** (wider, narrower or shorter than the
  session), or any viewport while an application has switched column mode
  (DECCOLM), still gets ring-sized frames (`dump_vt`'s fallback) — but one at
  a time.
- **Rows scrolled while a viewport is detached are not delivered**: a
  reconnect keeps the viewport's ring and continues forward-only from the
  attachment (decision 13); history across a reconnect is Stage 5. Rows a
  session width change or height grow left unsent become a forward jump
  at the resize (decision 12) — labelled by Stage 4, not repaired.
- **An unpaced or relayed viewport stays on v1 (RFC 0002) history** — a
  local `posh attach` (Stage 6 moves it), a relayed one
  (`POSH_MUX_SESSIONS=0`), or one with `POSH_PACED=0` — with v1's re-carry
  of every un-acked row and, on a lossy link, posh#240's double append,
  until Stage 7 retires the old delivery mode.
- **A viewport that is dropped is not told why** (posh#226); with pacing the
  drop is now a backstop, not the flood outcome.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| `PACED_FRAME_FLOOR_MS` (`session/daemon.rs`) | 20 ms | `server_loop`'s send-interval floor (`SEND_INTERVAL_MIN`); ≤ 50 frames/s of encode work per viewport | prompt acks: 26 visible frames for 512 chunks, peak backlog one frame pair (below); revisit if a measured flood shows encode cost or a frame rate the eye notices |
| `PACED_ACK_WAIT_MS` (`session/daemon.rs`) | 250 ms | `server_loop`'s send-interval ceiling (`SEND_INTERVAL_MAX`); bounds the stall a lost ack can cause | never-acked: one visible frame per wait; it sets a v1 viewport's history RTT cliff (≈ 5 × this, Limitations) — raise it if field RTTs approach the cliff; it is also the v2 resend floor's minimum |
| `HISTORY_WINDOW_ROWS` (`session/daemon.rs`) | 512 rows (2 × `SB2_ROWS_PER_BODY`) | v2 rows in flight before the ack — with the socket, THE history rate limit (no frame floor): one window per round trip, `server_loop`'s SRTT/2 send interval as a static window; Task 3.4 makes it dynamic | prompt acks deliver a 4 KiB/ms flood whole; 50 ms RTT loses 6,384 of it and RTT ≥ 300 ms keeps about the ring (below); the field ack latencies (Task 3.0) decide Task 3.4's share |
| `HISTORY_RESEND_INITIAL_MS` (`session/daemon.rs`) | 1000 ms (4 × `PACED_ACK_WAIT_MS`) | the v2 resend floor before any ack latency is measured: TCP's initial RTO; afterwards `max(PACED_ACK_WAIT_MS, 2 × srtt)` | at RTT 1,500 ms one window (512 rows) is re-sent spuriously before the first sample; none at ≤ 300 ms |
| `HISTORY_RESEND_MAX_DOUBLINGS` (`session/daemon.rs`) | 3 | the resend floor doubles per resend without ack progress, so a never-acking viewport is re-sent one window per 1, 2, 4, 8, 8, … × the floor | never-acked: 160–163 KB of history over 2 MiB + 20 s, against 2.2–7.9 MB for v1 |
| `SB2_ROWS_PER_BODY` (`remote/history.rs`, shared with `server_loop`) | 256 rows | RFC 0009 §2's per-body cap: bounds one body, and with it the backlog ahead of a screen | every v2 run's backlog peaks at one body (≤ 26,676 B); with prompt acks bodies are about one chunk's rows, and the peak backlog 5,132–8,476 B |

The first two start at `server_loop`'s send-interval clamp, are tuned
independently of it, and every lever here changes only with a measurement
recorded here.

**Measured** (2026-10-06, ideal-reader harness: 50x200, socket buffers
pinned to 128 KiB, one write per chunk, 2 MiB flood, fake clock 1 ms per
chunk, `--release`). Re-run with
`just debug-cargo test --release -p posh --bin posh posh225_flood_backlog_ideal_reader_measurement -- --ignored --nocapture`.

| Case | Visible frames | Peak backlog | History | Outcome |
|---|---|---|---|---|
| unpaced, 4 KiB chunks, prompt acks | 512 for 512 chunks | 9,369 B | — | — |
| unpaced, never acked | one per chunk | crosses 16 MiB | — | dropped after 655,360 B fed (empty ring) / 651,264 B (full ring) |
| paced, 4 KiB chunks, prompt acks | 26 for 512 chunks (≤ 1 per floor); at most 1 queued | 88,719 B (empty ring) / 92,352 B (full ring) = one 5,145 B visible frame + one ~800-row scrollback frame | 20,511 / 20,560 rows scrolled, all acked | ends on the last screen |
| paced, never acked | 3 in 512 ms (4 KiB chunks), 10 in 2048 ms (1 KiB chunks): the first at the floor, then one per ack wait | 1,045,143–1,045,177 B = one scrollback frame carrying the whole 10,000-row ring | not acked (no acks) | never crosses the cap; ends on the last screen |
| paced, 1 KiB chunks, RTT 50 ms | — | 58,405 B | 20,129 / 20,511 acked (empty ring) | ends on the last screen |
| paced, 1 KiB chunks, RTT 300 ms | — | 527,217 B | 17,730 / 20,511 acked | ends on the last screen |
| paced, 1 KiB chunks, RTT 1500 ms | every one a `Full` | 1,045,177 B (7 scrollback frames) | 0 acked: base lost (the RTT cliff) | still attached |
| paced, slow reader (1 KiB/ms) | at most 1 queued | one ring-sized scrollback frame | ~4,300 of 20,511 rows never shipped (ring eviction) | ends on the last screen |

The same command prints the v2 rows (posh#225 Stage 3; empty ring shown — a
full ring differs by the 49 rows the prefill's last screen adds). History is
"delivered / lost in forward jumps" of 20,511 rows scrolled; every acking
run acked all 20,511, and every run delivered no row that differed from the
session's and ended on the last screen with no screen built against a lost
base.

| Case | Peak backlog | History bytes | Delivered / jumped | Repeated |
|---|---|---|---|---|
| v2, 1 KiB chunks, prompt acks | 5,149 B | 2.23 MB (1,942 bodies) | 20,511 / 0 | 0 |
| v2, 1 KiB chunks, RTT 50 ms | 5,132 B | 2.24 MB (2,004) | 20,511 / 0 | 0 |
| v2, 1 KiB chunks, RTT 300 ms | 26,676 B | 1.30 MB (256) | 12,400 / 8,111 (13 jumps) | 0 |
| v2, 1 KiB chunks, RTT 1,500 ms | 26,676 B | 1.13 MB (93) | 10,774 / 9,737 (3) | 0 |
| v2, 1 KiB chunks, RTT 2,500 ms | 26,676 B | 1.12 MB (92) | 10,768 / 9,743 (2) | 0 |
| v2, 1 KiB chunks, never acked | 26,676 B | 163 KB (55) | 1,024 / 9,743 (2) | 512 |
| v2, 4 KiB chunks, prompt acks | 8,476 B | 2.16 MB (486) | 20,511 / 0 | 0 |
| v2, 4 KiB chunks, RTT 50 ms | 26,676 B | 1.48 MB (134) | 14,127 / 6,384 (4) | 0 |
| v2, 4 KiB chunks, RTT 300 ms | 26,676 B | 1.10 MB (54) | 10,543 / 9,968 (2) | 0 |
| v2, 4 KiB chunks, RTT 1,500 ms | 26,676 B | 1.10 MB (53) | 10,512 / 9,999 (1) | 0 |
| v2, 4 KiB chunks, RTT 2,500 ms | 26,676 B | 1.12 MB (54) | 10,512 / 9,999 (1) | 256 |
| v2, 4 KiB chunks, never acked | 26,676 B | 161 KB (17) | 768 / 9,999 (1) | 768 |
| v2, slow reader (1 KiB/ms), prompt acks | 26,676 B | 1.47 MB (57) | 14,111 / 6,400 (7) | 0 |
| v2, slow reader, RTT 50 ms | 26,676 B | 1.36 MB (56) | 13,007 / 7,504 (4) | 0 |

The regression tests (`posh225_v2_*`, a 256 KiB flood of 2,521–2,570 rows,
inside the ring) pin: every row delivered exactly once and acked at prompt,
50 ms and 300 ms RTTs; at 1,500 ms every row delivered with one spurious
window (512 rows) re-sent; a backlog of one body (< 64 KiB) for every
cadence; a never-acked viewport holding exactly the first window with resend
rounds at 1, 2, 4 and 8 s; and a slow reader losing only rows the ring
evicted before their turn.

Unpaced streams are byte-identical to before this feature, including beside a
paced viewport on the same session — and unpaced and paced v1 streams are
byte-identical beside a v2 viewport (every non-v2 row above is unchanged by
Stage 3).

## Rollback

`POSH_PACED=0` on the viewport, effective on its next attach; no session
restart. The daemon falls back to per-read delivery for any viewport that does
not advertise `CAP_PACED`, so two viewports on one session may run different
modes, and an older viewport keeps working against a newer daemon.

## More Information

- Wire: RFC 0008 §3.2; the capability id: RFC 0001 (id 23, `CAP_PACED`).
- Relay exclusion: ADR 0007. Frame dumps: Stage 1 (RFC 0008 §2,
  `Terminal::dump_vt_mirror`).
- Code: `session::daemon` (`Pacing`, `ClientConn::paced_send_at`,
  `request_frame_from`, `send_paced_frames`, `paced_poll_timeout`,
  `flush_paced_frames`; v2: `ClientConn::open_history`, `history_send_at`,
  `send_history_body`, `reset_history_on_resize`), `remote::history`
  (`crates/posh/src/remote/history.rs`: `HistoryCursor`, `HistoryStart`,
  `SB2_ROWS_PER_BODY`, shared with `server_loop`),
  `session::parse_paced_gate`, `remote::client::outgoing_caps`, the M2
  bridge's Init and ack forward in `remote::server` (`bridge_init_content`,
  `bridge_client_message`). The flood harness and its viewport model are
  `session::daemon`'s tests (`measure_flood`, `FloodLedger`,
  `FloodViewport`).
- Wire for v2 history on the session socket: RFC 0009 §5.
- Implementation plan: `docs/plans/2026-10-05-flood-delivery.md`.
