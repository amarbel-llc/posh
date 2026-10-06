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
is sent at most one screen at a time, and that screen is the newest.

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
  send opportunity, at most two bodies (`HISTORY_WINDOW_ROWS`, 512 rows) in
  flight. Rows the session's ring evicted before their turn are skipped as
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
  newest — whether or not a newer frame was already in flight. `srtt` is smoothed like TCP's, `ack_age_ms` is the time
  since the last ack of any kind, and `new=` counts samples since the
  previous line (posh#225 Stage 3.0).

The wire contract is RFC 0008 §3.2.

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
9. **One body per opportunity, with a coin.** The screen keeps decision 1's
   opportunity; history has its own — an empty buffer, and fresh rows with
   room in the window (the floor after the last body) or rows in flight (the
   resend deadline). When both are due, the kind that did not go last goes
   (`server_loop`'s coin), so the backlog is at most one body. A history body
   rides the newest visible frame number without taking a producer slot, and
   is never acknowledged by `FrameAck` (RFC 0009 §2, §4).
10. **The window is `HISTORY_WINDOW_ROWS` (two bodies) in flight**: the
    daemon's static stand-in for `server_loop`'s SRTT-paced send interval —
    without it history would flow at 256 rows per floor whatever the link.
11. **Resend from the ack after twice the measured ack latency**, never under
    `PACED_ACK_WAIT_MS`, `HISTORY_RESEND_INITIAL_MS` before any sample, and
    doubled per resend without ack progress up to
    `HISTORY_RESEND_MAX_DOUBLINGS` times. The bridge stops retransmitting a
    body once the visible frame it rode is acked, so this resend is required
    for correctness, not an optimisation.
12. **The epoch bumps for a viewport on its own size change** (it cleared
    its ring). **A session width change re-anchors every other viewport
    without a bump**: the reflow renumbered the daemon's ring, so the row
    space continues at the reflowed total from the viewport's send cursor —
    its ring and ack stay valid, and rows not yet sent become a forward
    jump. Another viewport attaching narrower, or leaving, therefore never
    clears this one's history (v1 never did). The epoch byte skips 0 on wrap
    (255 → 1): a viewport advertises 0 to mean it holds none.
13. **A new attachment continues the viewport's epoch at its count** (epoch
    0, held by a viewport that has none, opens a fresh epoch): a fresh epoch
    on every mux reconnect would make the viewport clear its ring.
14. **History comes from the session terminal and pauses while the escape
    overlay is up**; it does not pause on the alternate screen.

## Limitations

- **History trickles at about one window per round trip** (≈ 512 rows/RTT)
  for a v2 viewport, so a flood longer than the ring at that rate loses its
  oldest rows as a forward jump — silent until Stage 4 draws it. (A paced v1
  viewport keeps Stage 2's cliff: above ≈ 5 × `PACED_ACK_WAIT_MS` RTT its
  base is lost and v1 history is withheld until the flood ends.) Part B of
  Task 3.3 measures both.
- **A never-acking v2 viewport is re-sent at most one window (512 rows) per
  resend floor**, backing off to one per 8 × the floor. This is bandwidth,
  not backlog: at most one body is ever queued. (A paced v1 viewport that
  never acks is still re-sent up to one ring of history every
  `PACED_ACK_WAIT_MS`.)
- **A history body lost on the wire while a later body is in flight is
  accepted by the viewport as a forward jump** and cannot be repaired by the
  daemon's resend-from-ack (RFC 0009 has no eviction-vs-loss marker); with
  two bodies in flight, a body lost under a flood becomes a hole Stage 4
  will draw as "not received". `HISTORY_WINDOW_ROWS = SB2_ROWS_PER_BODY`
  (one body in flight) would remove the loss hole at half the throughput —
  an operator decision.
- **A reader slower than the flood loses the rows the ring evicts before
  their turn**; for a v2 viewport each loss is a forward jump of exactly that
  span, and the reader ends holding the whole retained ring. The live screen
  still ends on the last screen.
- **A mismatched-geometry viewport** (wider, narrower or shorter than the
  session), or any viewport while an application has switched column mode
  (DECCOLM), still gets ring-sized frames (`dump_vt`'s fallback) — but one at
  a time.
- **History is still lost across a reconnect** (a later stage).
- **A viewport that is dropped is not told why** (posh#226); with pacing the
  drop is now a backstop, not the flood outcome.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| `PACED_FRAME_FLOOR_MS` (`session/daemon.rs`) | 20 ms | `server_loop`'s send-interval floor (`SEND_INTERVAL_MIN`); ≤ 50 frames/s of encode work per viewport | prompt acks: 26 visible frames for 512 chunks, peak backlog one frame pair (below); revisit if a measured flood shows encode cost or a frame rate the eye notices |
| `PACED_ACK_WAIT_MS` (`session/daemon.rs`) | 250 ms | `server_loop`'s send-interval ceiling (`SEND_INTERVAL_MAX`); bounds the stall a lost ack can cause | never-acked: one visible frame per wait; it sets a v1 viewport's history RTT cliff (≈ 5 × this, Limitations) — raise it if field RTTs approach the cliff; it is also the v2 resend floor's minimum |
| `HISTORY_WINDOW_ROWS` (`session/daemon.rs`) | 512 rows (2 × `SB2_ROWS_PER_BODY`) | v2 rows in flight before the ack: about two bodies per round trip, `server_loop`'s SRTT/2 send interval as a static window; Task 3.4 makes it dynamic | measurement: Task 3.3 Part B |
| `HISTORY_RESEND_INITIAL_MS` (`session/daemon.rs`) | 1000 ms (4 × `PACED_ACK_WAIT_MS`) | the v2 resend floor before any ack latency is measured: TCP's initial RTO; afterwards `max(PACED_ACK_WAIT_MS, 2 × srtt)` | measurement: Task 3.3 Part B |
| `HISTORY_RESEND_MAX_DOUBLINGS` (`session/daemon.rs`) | 3 | the resend floor doubles per resend without ack progress, so a never-acking viewport is re-sent one window per 1, 2, 4, 8, 8, … × the floor | measurement: Task 3.3 Part B |
| `SB2_ROWS_PER_BODY` (`remote/history.rs`, shared with `server_loop`) | 256 rows | RFC 0009 §2's per-body cap: bounds one body, and with it the backlog ahead of a screen | measurement: Task 3.3 Part B |

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

Unpaced streams are byte-identical to before this feature, including beside a
paced viewport on the same session.

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
  (`HistoryCursor`, shared with `server_loop`), `session::parse_paced_gate`,
  `remote::client::outgoing_caps`, the M2 bridge's Init in `remote::server`.
- Implementation plan: `docs/plans/2026-10-05-flood-delivery.md`.
