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
  any scrollback frame already queued ahead of it (up to a ring, ~1 MB, until
  Stage 3). The program in the session is never slowed: the PTY is read and
  the terminal fed exactly as before.
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
   (posh#181 threading), so a scrollback frame is never queued per read.
5. **A RESYNC releases the ack wait** — the viewport rejected what was
   outstanding, so its ack is not coming. Nothing else does.
6. **A regeometry frame waits for the next opportunity** and does not release
   the ack wait: frames in flight for the old geometry are still valid input,
   so the new-geometry screen lands within one ack (≤ `PACED_ACK_WAIT_MS`).
7. **The session's last screen is flushed before `Exit`**, whatever the
   viewport's pacing.

## Limitations

- **History during a flood needs RTT below ≈ 5 × `PACED_ACK_WAIT_MS`**
  (≈ 1.25 s). The frame producer's 8-frame outstanding window holds four
  visible+scrollback pairs, and a paced viewport is sent one pair per
  `PACED_ACK_WAIT_MS` while acks are in flight, so an RTT above about
  5 × `PACED_ACK_WAIT_MS` less one pace interval evicts a frame before its
  ack lands: the v1 base is lost, every visible frame is a `Full`, and v1
  history is withheld until the flood ends (measured: 0 rows acked at
  1500 ms RTT, against 17,730 of 20,511 at 300 ms). The viewport stays
  attached and the live screen keeps updating. Stage 3's addressed history
  removes the dependence on the base.
- **v1 history re-carry.** A paced viewport that never acks is re-sent up to
  one ring of history every `PACED_ACK_WAIT_MS` (measured: a 1,045,177-byte
  scrollback frame carrying the whole 10,000-row ring). This is bandwidth,
  not backlog — at most one visible frame and its scrollback frame are ever
  queued, so the cap is not at risk. Stage 3 replaces v1 history for paced
  viewports.
- **A slow reader loses rows to ring eviction.** While a ring-sized
  scrollback frame drains to a slow reader, the ring keeps scrolling, and
  rows evicted before the next scrollback frame are never shipped (measured
  at 1 KiB/ms: ~4,300 of 20,511 rows). The live screen still ends on the last
  screen. This is the existing eviction design; Stage 3's per-body cap
  bounds the scrollback frame and with it the loss.
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
| `PACED_ACK_WAIT_MS` (`session/daemon.rs`) | 250 ms | `server_loop`'s send-interval ceiling (`SEND_INTERVAL_MAX`); bounds the stall a lost ack can cause | never-acked: one visible frame per wait; it sets the history RTT cliff (≈ 5 × this, Limitations) — raise it if field RTTs approach the cliff |

Both start at `server_loop`'s send-interval clamp, are tuned independently of
it, and change only with a measurement recorded here.

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
  `flush_paced_frames`), `session::parse_paced_gate`,
  `remote::client::outgoing_caps`, the M2 bridge's Init in `remote::server`.
- Implementation plan: `docs/plans/2026-10-05-flood-delivery.md`.
