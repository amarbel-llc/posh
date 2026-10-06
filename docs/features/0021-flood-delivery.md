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
  Read once per attach. When on, the viewport advertises `CAP_PACED` (RFC 0001
  id 23) and the M2 bridge carries it into the daemon's Init.
- **What the user sees:** during a flood the live screen jumps to the latest
  state rather than replaying every intermediate one, and at most one screen
  is in flight to the viewport. What the user sees and what their keystrokes
  act on stay in step — Ctrl-C mid-flood takes visible effect at once. The
  program in the session is never slowed: the PTY is read and the terminal fed
  exactly as before.
- **Not paced:** a local `posh attach` (it does not advertise the capability
  yet — a later stage), and a viewport reached through the relay
  (`POSH_MUX_SESSIONS=0`): the relay does not forward `CAP_PACED` (ADR 0007).
  Both get today's per-read delivery.

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
   output only marks the viewport dirty. Screens produced in between are
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

- **The attach replay, RESYNC keyframe, regeometry frame, activity answer and
  source-swap frames are still built when their event happens**, outside the
  pacing — one frame per event, not per read, so they cannot accumulate
  during a flood of output. They join the paced path in a follow-up
  (posh#225).
- **v1 history re-carry.** A paced viewport that never acks is re-sent up to
  one ring of history every `PACED_ACK_WAIT_MS`. The cap is not at risk (at
  most one visible frame and its scrollback frame are ever queued), but the
  bandwidth is spent. A later stage replaces v1 history for paced viewports.
- **A mismatched-geometry viewport** (wider, narrower or shorter than the
  session) still gets ring-sized frames (`dump_vt`'s fallback) — but one at a
  time.
- **History is still lost across a reconnect** (a later stage).
- **A viewport that is dropped is not told why** (posh#226); with pacing the
  drop is now a backstop, not the flood outcome.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| `PACED_FRAME_FLOOR_MS` (`session/daemon.rs`) | 20 ms | `server_loop`'s send-interval floor (`SEND_INTERVAL_MIN`); ≤ 50 frames/s of encode work per viewport | measurement pending (posh#225 flood measurement, to be recorded here) |
| `PACED_ACK_WAIT_MS` (`session/daemon.rs`) | 250 ms | `server_loop`'s send-interval ceiling (`SEND_INTERVAL_MAX`); bounds the stall a lost ack can cause | measurement pending, as above |

Both are tuned independently of `server_loop`'s clamp and change only with a
measurement recorded in this table.

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
  `owe_paced_frame`, `send_paced_frames`, `paced_poll_timeout`,
  `flush_paced_frames`), `session::parse_paced_gate`,
  `remote::client::outgoing_caps`, the M2 bridge's Init in `remote::server`.
- Implementation plan: `docs/plans/2026-10-05-flood-delivery.md`.
