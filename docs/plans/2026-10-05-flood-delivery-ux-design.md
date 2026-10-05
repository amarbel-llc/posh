# Flood delivery UX: what a viewport sees when a session out-produces it

**Date:** 2026-10-05
**Status:** design approved, implementation plan in
`2026-10-05-flood-delivery.md`
**Tracks:** posh#225 (viewports die under a high output rate).
**Follow-ups this settles or constrains:** posh#227 (drop policy — settled
by decision 6), posh#226 (a dropped viewport is never told why — still its
own work, required by decision 6's backstop).
**Builds on:** FDR 0005 (client-side scrollback), RFC 0002 / RFC 0009
(scrollback sync v1 / v2), RFC 0008 (session frame transport), RFC 0015
(session resume cursor), FDR 0014 / RFC 0011 (mux endpoint).

## Problem

A session that prints faster than a viewport can receive it — `nix gc`
deleting a few hundred thousand store paths is the field case — kills the
viewport. On the mux session-channel path the user sees only:

    posh: [client exited]
    posh: session <host>:<session> lost (mux channel closed)

The session survives; the viewport does not.

### What was established (2026-10-05)

- **Field log.** The remote session daemon dropped the mux bridge as a
  "slow client" at `MAX_CLIENT_BACKLOG` (16 MiB) twice in one session,
  after ~85 s and ~101 s attached, having already delivered 382 MB and
  775 MB to it. Every high-water line showed the reader draining
  (`last_drain_age_ms` 0–30, `drained_total` rising): a healthy reader,
  out-produced. No panic, no coredump.
- **Mechanism (code + in-process measurement).** Each visible frame is
  built from `dump_vt()`, which replays the whole scrollback ring before
  the grid. The codec sends a prefix/suffix diff only when that is smaller
  than the dump. Once the 10,000-row ring is full, every new line evicts
  the oldest row, the common prefix collapses, and the frame falls back to
  a full dump of ring plus screen — about 1 MiB at ~100-byte rows. The
  daemon emits one such frame per PTY read (≤ 4 KiB), appends rather than
  supersedes them for a lossy client, and writes to each client once per
  loop iteration (≤ ~219 KiB per write measured). With a full ring the
  backlog crossed 16 MiB after 74–98 KiB of output regardless of ack
  cadence, even against an ideal reader.
- **A second, smaller amplifier.** Scrollback frames re-carry every row
  since the client's last *acknowledged* total and are never coalesced.
  Real, but under 5% of the bytes once the ring is full.
- **The drop reason is lost at the first hop.** The daemon closes the
  socket without a word; the bridge forwards a bare EOF as an empty-payload
  close; the viewport prints the generic notice (posh#226).

The measurement tests are `posh225_flood_backlog_measurement` and
`posh225_flood_backlog_ideal_reader_measurement` in
`crates/posh/src/session/daemon.rs` (`#[ignore]`, assertion-free).

## Principle

**A session never slows the program it runs because of a viewport.**
Viewports are observers. A slow, stalled or absent viewport pays with its
own screen smoothness or a hole in its own scrollback — never with
backpressure onto the PTY, not even as an opt-in. A session outlives and
ignores its viewports.

## Decisions

Recorded from the design conversation, in the order they were settled.

1. **History is delivered up to what the session still holds.** After a
   flood, a viewport's scrollback catches up to every row still in the
   session's ring, however far behind it fell. Rows evicted before they
   could be delivered become a *marked hole* stating how many lines were
   not received. A silent seam is never acceptable.
2. **The live screen jumps to latest.** A viewport is sent the newest
   screen; intermediate screens are skipped, not queued. What the user
   sees and what their keystrokes act on stay in step (Ctrl-C mid-flood
   takes visible effect at once).
3. **Live screen first, plus a history trickle.** When the link cannot
   carry both, the newest screen always goes out and history uses a share
   of what is left. A lever sets the trickle's ceiling; `0` degrades to
   pure live-first. Below the ceiling the share is dynamic: it shrinks
   toward zero as that viewport's backpressure grows and recovers with
   headroom.
4. **Trailing history is announced in the scroll view.** While rows are
   still arriving, the existing scrollback top bar adds a "lines still
   arriving" count that counts down and disappears when caught up.
5. **The program is never slowed.** See Principle. No throttle, no
   opt-in throttle.
6. **A viewport that stops reading stays attached.** Per-viewport debt is
   bounded by decisions 1–2 (one unsent screen plus at most one ring of
   pending history), so output volume can no longer reach the byte cap.
   `MAX_CLIENT_BACKLOG` remains only as a backstop for clients that still
   take a raw output stream and for bugs; if it fires, the viewport is
   told why (posh#226). Accepted cost: a stalled viewport stays in
   smallest-wins size arbitration until it detaches, as any idle attached
   viewport does today.
7. **History rows are addressed and filled oldest first.** Each row
   carries its position in the session's history, so delivery is
   repeatable and order-free: a duplicate overwrites itself and a row can
   be placed wherever it belongs. This is the same shape as screen sync —
   decide what to send when the socket can take it, from the session's
   current state and what the viewport has acknowledged — so floods,
   wake-ups and reconnects share **one catch-up path**. Oldest-first keeps
   a viewport's history in one contiguous block and delivers the rows
   nearest eviction first, losing the fewest.
8. **Holes are drawn by the viewport.** Every hole is a single collapsed
   row at its true position, labelled *arriving* (`··· 3,200 lines
   arriving ···`) or *not received* (`··· 12,340 lines not received ···`).
   An arriving hole shrinks as rows land and disappears when filled. The
   top-bar count from decision 4 stays as the summary. The rows the user
   is reading must not move when a hole fills: the view stays anchored to
   content. A hole row is a view decoration, not history. Viewports on
   older builds get no marker (today's behaviour); the daemon does not
   insert one for them.
9. **History position survives a reconnect.** The viewport's history
   position rides the resume cursor. Rows produced during an outage fill
   in by the ordinary catch-up path; only rows already evicted become a
   *not received* hole. Catch-up is paced by the same backpressure as
   everything else and never arrives as a burst. **Stampede guard:** when
   several viewports sharing one mux wire reconnect together, their
   combined catch-up is bounded so it cannot swamp live screens or the
   link, and there is a test for that case.
10. **Fresh attaches stay forward-only, for now.** Back-filling a new
    viewport from the session's existing ring is out of scope (it is FDR
    0005's deferred extension and has its own decisions: per-attach cost,
    newest-first fill order, what a fresh attach means). Constraint on this
    work: fill order and starting position are per-viewport choices, never
    fixed assumptions, so back-fill needs no further protocol change.
11. **Local attach gets the same experience, second.** Remote (mux) path
    first, local `posh attach` as a later stage of the same plan — not a
    someday-issue. The local client should adopt the remote client's
    recovery behaviour (base guard, graceful re-ack, resync); the two
    converge on shared logic over time.
12. **No notice on the live screen, for now.** Skipped screens and
    trailing history are normal operation and are explained where the user
    looks for them (the scroll view). To be revisited after hands-on use.
13. **Rollout is split by path, and the switch belongs to the viewport.**
    The remote path ships on by default with an off switch; the local path
    ships opt-in first and flips once it has run without a wedge. The
    viewport advertises that it understands the new delivery and the
    daemon falls back to today's behaviour for any viewport that does not,
    so: turning it off takes effect on the next attach without restarting
    the session; older viewports keep working against a new daemon; two
    viewports on one session can run different modes side by side.
    Retiring the old delivery mode is an explicit final step. The man
    pages document the rollout state, every environment variable, and the
    steps to move between states and promote a default.

## Consequences that need no separate decision

- Full-screen apps that redraw rapidly have no scrollback, so decision 2
  alone covers them — including streamed build logs behind a TUI.
- Each viewport's experience depends only on its own link.
- Resize keeps its rule: the scroll view is discarded and history
  re-accumulates at the new width (the addressed row space is scoped to an
  epoch, as RFC 0009 §1.1 already specifies).
- Text selection in the scroll view is out of scope.

## Open, deliberately

- **Visual treatment of hole rows** — a look-alike transition or another
  way to set them apart from real output.
- **The trickle lever** — its name, default ceiling, and exactly which
  signal "backpressure" reads.
- **A live-screen notice** — decision 12 is "for now".

## Definition of done, from the user's seat

`nix gc` in a remote mux-attached session runs to completion with the
viewport staying attached; Ctrl-C takes visible effect immediately
mid-flood; and wheeling up afterwards shows the output in one piece, with a
labelled hole only if more than a ring went by undelivered.
