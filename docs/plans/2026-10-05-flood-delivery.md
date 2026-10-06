# Flood Delivery Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use eng:subagent-driven-development to implement this plan task-by-task.

**Goal:** A session that out-produces a viewport (posh#225, `nix gc`) no
longer kills it: the live screen jumps to latest, history trails and
catches up, and whatever could not be delivered is a labelled hole.

**Architecture:** Two layers. *Shrink the frame:* visible frames stop
replaying the whole scrollback ring, so a frame is screen-sized instead of
ring-sized — wire-compatible with every deployed viewport. *Bound the
backlog:* the session daemon stops queueing a frame per PTY read and
instead decides what to send each viewport at send time, from the
session's current state and what that viewport has acknowledged — the
discipline the single-peer roaming server (`server_loop`) already has,
including addressed (RFC 0009 v2) history. Holes, reconnect continuity and
local-attach parity build on that.

**Tech Stack:** Rust — `crates/posh-term` (serializer), `crates/posh-proto`
(codecs, capability table), `crates/posh` (session daemon, mux bridge,
remote + local clients), `crates/poshterity` (frame harness); scdoc man
pages under `doc/`.

**Rollback:** Stage 1 is a serializer bound with unchanged rendering;
rollback is a revert (see "Rollout exception" below). Stages 2–6 are gated
on a capability the *viewport* advertises: a viewport that does not
advertise it gets today's delivery from the same daemon, so
`POSH_PACED=0` on the viewport takes effect on the next attach without
restarting the session. Stage 7 retires the old mode and is the one
irreversible step; land it last and alone.

**Records:** design in `2026-10-05-flood-delivery-ux-design.md` (the
thirteen decisions; do not re-litigate). New FDR *Flood delivery* (levers,
rollout states, rollback) — `proposed` until Stage 2 lands, then
`experimental`. RFC 0001 (capability registry, edited in place), RFC 0008
§3 (send discipline for session-socket clients), RFC 0009 (v2 history on
the session-socket path; server-reported availability), RFC 0015 (history
on the resume cursor). FDR 0005 gains the hole rows and the "arriving"
count. Each record's status moves in the commit that moves the code
(`docs/README.md`).

---

## Stage order, and why it differs from the conversation

The conversation ordered the layers "bound the backlog, then shrink the
frame". Research reversed that, and the plan follows the research:

- **Shrinking the frame is small, needs no negotiation, and fixes deployed
  viewports too.** A `Full` body is "bytes a `Terminal` can process"; a
  dump that replays fewer scrollback rows is still that. No capability, no
  client change. It removes the ~1 MiB-per-PTY-read amplification that
  drove the field failure, so it goes first.
- **Bounding the backlog is the structural fix** and carries everything
  the UX decisions ask for, but it is larger and negotiated. It goes
  second, on a daemon that is no longer drowning.

After Stage 1 alone: viewports stop being dropped and the live screen is
right; history during a flood is still today's (v1, lossy under ack lag,
silent seam). Stages 2–5 deliver the decided history behaviour.

| Stage | Delivers | Decisions |
|---|---|---|
| 0 | Failing regression tests pinned; measurements committed | — |
| 1 | Screen-sized visible frames (all viewports) | fixes the drop |
| 2 | Send-time, paced screen delivery for capable viewports | 2, 5, 6, 13 |
| 3 | Addressed history on the daemon path + backpressure trickle | 1, 3, 7 |
| 4 | Viewport-drawn hole rows + "arriving" count | 4, 8 |
| 5 | History across reconnect + stampede guard | 9 |
| 6 | Local attach on the same path (opt-in) | 11 |
| 7 | Man pages, promotion steps, retire the old mode | 13 |

Stages 0–1 are written as bite-sized steps with code. Stages 2–7 are
written at task level — files, the tests that define done, the facts they
rest on — and **each is expanded into bite-sized steps when it starts**,
because its exact code depends on what the previous stage settles. That is
deliberate, not an omission: writing Stage 5's code today would be
guessing at Stage 3's types.

### Rollout exception (needs the operator's nod)

Decision 13 puts the switch on the viewport. Stage 1 has no viewport-
visible behaviour to switch: it changes how many scrollback rows a frame
replays, and the rendering is unchanged except at the session's own size,
where two pre-existing `dump_vt` bugs (posh#228, posh#229) no longer show.
This plan ships Stage 1 **ungated**, rollback by revert; the operator
accepted that on 2026-10-05. If a lever is wanted
anyway, the cheapest is a daemon-side `POSH_FRAME_TAIL=full` read at
session start — but it cannot take effect without a new session, which is
why it is not proposed.

---

## Facts the plan relies on (verified 2026-10-05, worktree HEAD `ee87e23`)

These describe the code at `ee87e23`, before Stage 1.

Read in the code unless marked *measured* or *field*.

- **Why frames are ring-sized.** `dump_vt()` replays every scrollback row
  before the grid (`crates/posh-term/src/dump.rs:343-347`). `DumpDiff`
  sends `make_diff` (common prefix/suffix only, `posh-proto/src/frame.rs:20`)
  when smaller, else `Full` (`framesync/dumpdiff.rs:25-39`). With a full,
  evicting ring the first row changes every chunk, so every frame is a
  `Full` of ring + screen. *Measured:* ~1.0 MiB at ~102-byte rows, 512 of
  512 frames `Full`.
- **Why they pile up.** The daemon polls with no timeout, reads ≤ 4096
  bytes of PTY per iteration (`session/daemon.rs:1517`), calls
  `broadcast_output` immediately (`:1567`), appends for every client that
  is not `coalescing()` (`:504-515`), and does one `write` per client per
  iteration (`:1800-1801`). *Measured:* one write moved ≤ 219,264 bytes;
  *field:* drain steps are multiples of exactly that.
- **Who needs the ring in a dump.** `Tag::History` (`posh history` in VT
  form) sends `term.dump_vt()` (`session/daemon.rs:1745`) — it must keep
  the full replay. Frame consumers build a mirror with ring depth 0 and
  discard replayed scrollback (FDR 0005 §"state-model"), **except** that a
  target *taller* than the session shows `(target_rows − session_rows)`
  scrollback rows above the grid, because the flow lands at the target's
  bottom (`dump.rs:322-342`; `dump_vt` doc `:236-251`).
- **The frame harness cannot see the ring.** `poshterity`'s
  `ServerSide::new` builds `Terminal::with_scrollback(rows, cols, 0)`
  (`framereplay.rs:57`), so `a_taller_client_mirrors_a_scrolled_session`
  never exercises the ring-replay branch despite its comment, and its
  content assertion is anchor-agnostic. Stage 1 adds a ring-backed variant.
- **The target design already exists in `server_loop`.** Send-time
  decisions paced by `conn.send_interval()` (`remote/server.rs:1978-1982`),
  at most one fresh body per opportunity with screen/history alternating
  on `last_was_sb` (`:2001-2022`), v2 history from an acked cursor with a
  per-body cap `SB2_ROWS_PER_BODY = 256` (`:54`, `:2172-2205`), eviction as
  a forward jump (`:2174-2176`), RTO resend re-anchored at the ack
  (`:2013-2014`), epoch bump on resize (`:1880-1888`). The daemon has none
  of it and emits only v1 (`session/daemon.rs:644`).
- **The remote viewport already speaks v2.** It always advertises
  `CAP_SCROLLBACK2` (`remote/client.rs:3810-3818`), appends in order,
  drops duplicates, and *accepts a forward jump as permanently lost*
  (`:3212-3245`) — silently. Its history is a `ScrollbackRing`
  (`VecDeque<Vec<u8>>`, `remote/sync.rs:551-601`), posh's own type, so
  hole bookkeeping is not constrained by posh-term's frozen API. The
  scroll view reads rows by index at one seam (`remote/scrollview.rs:118-130`)
  and already keeps the view anchored as rows arrive (`client.rs:3239-3241`).
- **The local viewport does not.** `session/client.rs` handles only v1
  (`:743-764`); its `Scrollback2` arm records stats and nothing else
  (`:723`).
- **What the bridge forwards.** Init content caps are `MORPH`,
  `SCROLLBACK`, `BASE_SUM` + `LOSSY` (`remote/relay.rs:246-251`,
  `:296-305`) — not `SCROLLBACK2`. Per-message forwarding to the daemon is
  an allow-list (`relay::forwarded_client_caps`, `:230-244`) plus the M2
  bridge's own additions in `bridge_client_message`
  (`remote/server.rs:1136-1186`); the bridge sends the frame ack via
  `forward_ack`. Frames go daemon → viewport verbatim apart from the
  header (`relay::rewrap`, `:311-325`). ADR 0007: additions go in the M2
  bridge, never the relay.
- **Reconnect loses history today.** A reconnect is a fresh attach; the
  daemon anchors `sb_floor` at the current total (`session/daemon.rs:1689`).
  `SessionResume` carries `frame`, `input`, `echo` only
  (`remote/resume.rs:20-36`), and has no blanket `Default` precisely so a
  new durable stream forces every construction site.
- **No fairness between session channels** on a shared mux wire; RFC 0011
  §4.1's ordered drain puts session frames before agent traffic
  (`remote/server.rs:883-892`) and congestion control covers only the
  agent path.
- **Capability ids 0–22 are taken** (`posh-proto/src/caps.rs`), plus
  224/225. RFC 0012 (`CAP_SESSION_SIZE`, proposed) may have reserved one:
  read RFC 0001's table before allocating.
- **Not verified:** that the remote host that produced the field logs runs
  this commit; that the local attach has the same oversized frames (same
  `dump_vt`, unmeasured).

### Tooling for every task

- Dev loop: `just debug-cargo test -p <crate> <filter>` (in-worktree, not
  hermetic). Add `-- --nocapture` for printed tables, `--release` for the
  flood tests.
- Do **not** run bare `just` before merging; the pre-merge hook is the CI
  lane.
- `nix build` sees only git-tracked files: `git add` new files first.
- No ad-hoc scripts; a repeated pipeline becomes a `[group("debug")]`
  recipe with its comment block.
- Commit after every task. Do not create branches.

---

## Stage 1 as built (2026-10-05)

Stages 0 and 1 are implemented on branch `quiet-willow`. The Stage 0 and
Stage 1 task text below is kept as the historical plan and is **superseded
where it differs** from this section.

- **API.** `posh-term` has a third frame-transport entry point,
  `Terminal::dump_vt_mirror(mirror_rows, mirror_cols)`, plus
  `Terminal::dump_vt_mirror_is_bounded(mirror_rows, mirror_cols)`. There is
  no public `dump_vt_tail(max_scrollback_rows)` and no per-client "tail rows"
  count; review replaced them. `dump_vt()` is unchanged and still serves
  `posh history`; `dump_vt_flat()` is unchanged.
- **Geometry rule.** Same width and same height: no scrollback replayed (the
  grid is drawn from home, as when the ring is empty). Same width and taller:
  the newest `2 * mirror_rows` scrollback rows, over-provisioned so the
  replay overfills the mirror and lands at its bottom as the full one does.
  Any other geometry (wider, narrower, shorter, or 0 rows/cols): exactly
  `dump_vt`'s bytes. The fallback exists because a row count cannot
  reproduce the full replay there: on a wider mirror soft-wrapped rows
  rejoin and free lines the full replay fills from older rows; on a shorter
  mirror the cursor anchors differ. `dump_vt_mirror_is_bounded` names the
  rule; a bounded dump is shaped for the mirror size it was built for, the
  fallback renders on a mirror of any size.
- **Daemon.** `queue_frame_from` and `broadcast_output`
  (`session/daemon.rs`) build each client's visible frame with
  `dump_vt_mirror(client_rows, client_cols)`. `broadcast_output` builds one
  dump per distinct bounded geometry and ONE shared full dump for all
  fallback geometries. `Tag::History` keeps `dump_vt()`.
- **Regeometry frame.** Because a bounded dump fits one mirror size, when a
  frame client's own reported size differs from the size its last visible
  dump was shaped for (`ClientConn::visible_shaped_for`, `Some(size)` for a
  bounded dump, `None` for the full fallback), the daemon queues a fresh
  frame for the new geometry, reusing the attach replay: a `Diff` against
  the old dump for DumpDiff clients, a forced `Full` for MorphDelta clients
  (kept forced until the client acks it). A client whose last frame was the
  full fallback is owed nothing new: as before posh#225 it is repainted by
  the next output, and a frame per resize event would be ring-sized.
- **Roaming server.** `server_loop` (`remote/server.rs`) builds its visible
  frame with `dump_vt_mirror(client_size)`. It needs no regeometry frame: a
  peer resize resizes its own terminal, which marks it dirty.
- **Harness.** `poshterity`'s frame harness can run a ring-backed server
  (`FrameHarness::with_ring`) and builds frames per client geometry.
- **Measured** (socket-accurate in-process harness, 50x200, full 10,000-row
  ring, 4 KiB chunks, an ack per chunk): largest visible frame 944,001 bytes
  to 5,145 bytes. Before, the backlog crossed 16 MiB after 106,496 bytes of
  output; after, it never crossed across 2 MiB and peaked at 9,369 bytes.
- **Known limitations.**
  - A viewport wider, narrower or shorter than the session (for example the
    larger of two differently-sized viewports on one session) still gets
    ring-sized frames and can still be dropped under a flood. Stage 2's
    paced, send-time delivery bounds its backlog regardless of frame size;
    session geometry on the frame (RFC 0012) would make every mirror
    session-sized and retire the fallback.
  - The same fallback applies when the SESSION's width differs from the
    viewport's for a reason other than another viewport: an application
    that switches column mode (DECCOLM, `CSI ? 3 h` under `CSI ? 40 h`)
    resizes the terminal without any viewport resizing
    (`posh-term/src/terminal.rs`, `set_deccolm`), so every viewport is on
    full dumps until it switches back. Correct output, ring-sized frames.
  - Frames in flight across a viewport's own resize are shaped for its old
    size. A full `dump_vt` rendered correctly on a mirror of any size; a
    bounded dump does not, so for about a round trip (plus pacing) after
    growing, a viewport shows the screen at the top over blank rows before
    the frame for its new size lands, and after shrinking its cursor can sit
    on the wrong row. Transient and self-correcting in both producers; it is
    the price of not replaying the ring, and RFC 0012 removes it.
  - No client repaints the dump it already holds when it resizes itself, so
    a viewport on the full dump is still not repainted after its own resize
    until the next output (unchanged from before posh#225; a same-width
    viewport now gets a frame at once).
  - History during a flood is still v1: with acks withheld it re-carries
    every un-acked row per chunk, and a remote viewport under a flood is in
    the lost-base regime where history frames are not sent at all. Stages
    2–3 fix that.
  - Two pre-existing `dump_vt` bugs were found and filed, not fixed: posh#228
    (the relative cursor anchor is moved by modes replayed after the flow)
    and posh#229 (a soft-wrapped row followed by an empty row loses a line;
    the mirror draws rows one row low). A viewport of exactly the session's
    size no longer goes through that branch, so it stops seeing both; other
    geometries and `posh history` (VT form) still do. Ignored tests in
    `posh-term/src/dump.rs` wait for the fixes.
- **Rendering** is otherwise unchanged; the exception above is an
  improvement at the session's own size.
- **Platform.** One test site depends on the host's unix-socket send buffer
  and skips its capacity-dependent bounds at runtime where the host grants
  less than 96 KiB per write (`flood_socket_supports_bounds`, in the
  posh#225 test block of `session/daemon.rs`); it carries a
  `macOS gap (posh#214):` comment.

---

## Stage 2 as built (2026-10-06)

Stage 2 is implemented on branch `quiet-willow` (`776b86d..db14c5c`). The
Stage 2 task text below is kept as the historical plan and is **superseded
where it differs** from this section. The user-facing record is FDR 0021.

- **2.1 + 2.2 — `CAP_PACED` and the viewport** (`776b86d`, `ad7ab3c`; one
  implementer took both). `CAP_PACED` is id 23 with its RFC 0001 row. The
  roaming viewport advertises it on every message while `POSH_PACED` is
  unset or on (`session::parse_paced_gate` / `paced_selected`, read once per
  attach; `0`/`false`/`off`/`no` turn it off). The M2 bridge carries the
  viewport's entry into the daemon Init (`bridge_init_content`); the relay
  does not (ADR 0007), so a relayed viewport is unpaced, and the local
  `posh attach` is unpaced until Stage 6. Deviation: the palette About view
  lists `POSH_PACED` from the attach's own state (`st.paced`), not by
  re-reading the environment.
- **2.3 — the paced core** (`9a705b2`). A paced viewport gets at most ONE
  fresh visible frame (plus its v1 scrollback frame behind it) per send
  opportunity: `write_buf` empty AND (last fresh frame acked →
  `PACED_FRAME_FLOOR_MS` = 20 ms after it was queued; else
  `PACED_ACK_WAIT_MS` = 250 ms), built from the terminal as it is then. The
  poll timeout is the nearest opportunity. Deviations: the per-read path
  asks one helper, `ClientConn::takes_per_read_frames`, rather than testing
  pacing at each site; the flush of a dirty paced viewport before `Exit`
  moved into `daemon_loop`, ahead of `close_overlay`, so the last screen is
  queued before the overlay is torn down.
- **2.4 — every frame-owing event** (`602e949`). Attach replay, regeometry,
  RESYNC, activity answer and source swap mark the viewport dirty and are
  served at the next opportunity; only a RESYNC releases the ack wait.
- **2.5 — flood harness, tests, measurement** (`1ca8b8b`, `db14c5c`; the
  justfile's `debug-cargo-flake` FILTER is `2535deb`). Deviations from the
  task text: the frame accounting is a `FloodLedger` rather than a shared
  closure; a `FloodDrain::Trickle(n)` slow-reader drain was added; the
  paced `Lagged` cadence is time-based (an RTT in ms on the fake clock)
  rather than a chunk count, since chunk counts mean nothing once frames are
  not per chunk; the idle tail is event-driven (it advances to the next
  opportunity) rather than stepping a fixed `ms`. The `Lagged(5)` question
  the task text left open is answered with an RTT sweep (below), and the
  lost-base threshold is pinned at 5 × `PACED_ACK_WAIT_MS`.
- **2.6 — the backstop says `paced=`** (`4d42b71`), as specified.
- **2.7 — `session_frames=`** (`efab3c3`): the mux peer's SIGUSR2 line
  counts visible frames forwarded, as specified.
- **2.8 — records**: FDR 0021 (Tuning Levers, Limitations, Interface),
  `POSH_PACED` in `posh-client(1)` and `posh(1)`, this section.
- **Measured** (ideal-reader harness, 50x200, socket buffers pinned to
  128 KiB, one write per chunk, 2 MiB flood, fake clock 1 ms per chunk,
  `--release`; re-run with `just debug-cargo test --release -p posh --bin
  posh posh225_flood_backlog_ideal_reader_measurement -- --ignored
  --nocapture`; the full table is in FDR 0021):
  - Unpaced is byte-identical to before (measured, including beside a paced
    viewport). 4 KiB chunks, prompt acks: 512 visible frames for 512 chunks,
    peak 9,369 B. Never acked: crosses 16 MiB after 655,360 B fed (empty
    ring) / 651,264 B (full ring).
  - Paced, 4 KiB chunks, prompt acks: 26 visible frames for 512 chunks,
    peak 88,719 B (empty ring) / 92,352 B (full ring) — one 5,145 B visible
    frame plus one ~800-row scrollback frame; at most one visible frame
    queued; all 20,511 / 20,560 scrolled rows acked; ends on the last
    screen.
  - Paced, never acked: never crosses the cap; 3 visible frames in 512 ms
    (4 KiB) / 10 in 2048 ms (1 KiB) — the first at the floor, then one per
    ack wait; peak 1,045,143–1,045,177 B, one scrollback frame carrying the
    whole 10,000-row ring; ends on the last screen.
  - Paced RTT sweep (1 KiB chunks, 2048 ms flood): 50 ms → 20,129 / 20,511
    rows acked, peak 58,405 B; 300 ms → 17,730 / 20,511, peak 527,217 B;
    1500 ms → 0 acked, 7 scrollback frames, peak 1,045,177 B. The cliff: the
    producer's 8-frame outstanding window holds four visible+scrollback
    pairs at one pair per `PACED_ACK_WAIT_MS`, so an RTT above
    5 × `PACED_ACK_WAIT_MS` less one pace (≈ 1.25 s) evicts a frame before
    its ack lands, the base is lost, and v1 history stops for the flood
    (every visible frame a `Full`). The viewport is never dropped. The task
    text's expectation that a remote viewport leaves the lost-base regime
    holds below the cliff only.
  - Slow reader (1 KiB/ms): at most one visible frame queued, ends on the
    last screen; ~4,300 of 20,511 rows never shipped — evicted from the ring
    while a ~1 MB scrollback frame drained.
- **Known limitations** (FDR 0021 Limitations): the RTT cliff above (Stage 3's
  addressed history removes it); a never-acking viewport is re-sent up to a
  ring of history per ack wait — bandwidth, not backlog (Stage 3); a slow
  reader loses rows to ring eviction while a ring-sized scrollback frame
  drains (Stage 3's per-body cap); a mismatched-geometry or DECCOLM viewport
  still gets ring-sized visible frames, now one at a time (Stage 1's
  limitation); history across a reconnect (Stage 5); a dropped viewport is
  not told why (posh#226).
- **posh#227** closes with this stage (decision 6, implemented here).
- **Post-review cleanup.** The whole-stage review found that the daemon
  loop reset every framed client's scrollback floor on ANY resize event (a
  client attaching or leaving, a height-only resize, a mux reconnect), which
  skipped the rows a paced viewport still held unshipped since its last
  paced pair — a silent hole in its ring. The floor now resets only when the
  session WIDTH changes (`reset_scrollback_floors_on_reflow`; RFC 0002 §4's
  reflow trigger), pinned by `a_same_width_attach_does_not_skip_a_paced_clients_unshipped_history`.
  A side effect for every framed viewport: rows a session-height SHRINK
  pushes into scrollback now ship (the reset used to skip them). Not
  addressed: a height GROW pulls rows back out of the ring without lowering
  the monotonic total, so those rows ship again when they re-scroll — true
  before this change too — and a paced viewport holding unshipped rows
  across a grow may be sent rows it already holds. Refactors with no behaviour change: one entry point,
  `ClientConn::request_frame_from`, for every frame-owing event (paced:
  mark dirty; framed: build now), over the mechanism `build_frame_from`,
  with a RESYNC's release of the ack wait moved into `apply_frame_ack`; the
  morph regeometry keyframe's number is recorded by `queue_frame` when the
  keyframe is built rather than predicted at prepare time
  (`RegeometryKeyframe`); `Pacing.last_fresh` keeps only the send time, the
  outstanding test being the producer's `acked_num() < last_visible_num()`.
- **Exit check: NOT yet done.** The field run — a `nix gc`-shaped flood on a
  remote session, SIGUSR2 to the mux peer twice and `session_frames=`
  compared, Ctrl-C mid-flood — needs this build deployed to the remote host.

---

## Stage 3 as built (2026-10-06)

Tasks 3.0–3.3 and 3.5 are implemented on branch `quiet-willow`
(`0ca9f72..c950f8e`, plus this records commit); **Task 3.4 is HELD**. The
Stage 3 task text below is kept as the historical plan and is **superseded
where it differs** from this section. The user-facing record is FDR 0021;
the wire contract is RFC 0009 §5 (new) and RFC 0008 §3.2 (amended).

- **3.0 — ack-latency observability** (`0ca9f72`). `Pacing` keeps a send
  log of `(frame number, queued at)` for its paced visible frames (16
  entries, cleared on RESYNC); an ack that ADVANCES `acked_num` samples
  `now − queued_at` of the newest logged frame at or below the acked number
  and drops every entry at or below it, so each visible frame is sampled at
  most once and an ack naming a scrollback slot N+1 times visible frame N.
  A repeated or RESYNC ack is no sample; every ack stamps `last_ack_at`.
  `AckLatency` keeps last / srtt (7/8) / min / max / count. Surfaces: the
  backlog log lines and the `client disconnected` line carry `ack_ms=… ack_n=
  ack_age_ms=`, and a `paced ack latency … new=` line is written at most
  every `ACK_LOG_INTERVAL_MS` (10 s) while new samples arrive. Unpaced
  clients record nothing. No stream changed (the ideal-reader table was
  identical before and after). Deviations: the sample rule (the task text's
  "the ack confirming the NEWEST frame, timed from `last_fresh`" never fires
  under continuous output once the RTT exceeds `PACED_ACK_WAIT_MS` — the
  regime the signal exists for; corrected in "Design choices"); so `Pacing`
  is no longer `Copy`; the `Lagged(300)` flood test uses the 2 MiB flood
  (256 KiB ends before one 300 ms ack can land) and measures srtt 301 ms.
- **3.1 — `HistoryCursor`** (`53ccdd0`). `crates/posh/src/remote/history.rs`
  holds the RFC 0009 send cursor `server_loop` kept as seven locals, with
  `SB2_ROWS_PER_BODY`. `HistoryStart::{Fresh, Continue { epoch, rows }}` and
  `anchor_abs`/`anchor_rel` let the daemon continue a viewport's epoch at
  its count and never offer rows below that anchor. Exactness: `server_loop`
  only ever opens `Fresh`, for which the extraction is character-for-character
  the old logic (reviewed site by site), including the inactive-until-
  advertised shape; ten unit tests. The `wedge_repro` witness keeps its
  shape, but its `sb2_rows=` varies run to run with the induced loss (11,697
  vs 11,504 on one tree), so it is not a byte witness. The daemon's v1
  "keep in sync" comment lost its stale line reference.
- **3.2 — the M2 bridge** (`35f080a`), as planned: `bridge_init_content`
  adds the viewport's `CAP_SCROLLBACK2` entry beside `CAP_PACED` (never the
  relay, ADR 0007); `bridge_client_message` forwards the entry as
  `Tag::ClientCaps` only when its payload changed
  (`SessionBridge::sb2_forwarded`) and keeps `content`'s copy current;
  `rehome_bridge` clears the dedupe memory. Six tests.
- **3.3 Part A — the daemon core** (`18c2785`). A paced viewport whose Init
  carried a well-formed `CAP_SCROLLBACK2` gets a `HistoryCursor` in `Pacing`
  (`open_history`, from the Init arm beside `sb_floor`) and v2 bodies from
  `send_history_body`; v1 frames are unchanged for everyone else. One body
  per send opportunity (`send_paced_frames`' coin over `paced_send_at` and
  `history_send_at`); the 512-row window; resend from the ack after
  `max(PACED_ACK_WAIT_MS, 2 × srtt)` (1 s before a sample), doubling per
  resend without progress up to 8×; history pauses under the escape
  overlay; the exit flush is screen-only. A body rides the newest visible
  frame number (the producer does not advance) and is never a latency
  sample. Deviations:
  - `queue_frame`'s `activity_cap` became `frame_caps`, which also carries
    the server `SCROLLBACK2` ack on every visible frame to a v2 viewport.
  - **A session width change RE-ANCHORS the other viewports instead of
    bumping their epochs** (review finding I2; `HistoryCursor::reanchor`):
    a bump would wipe a desktop viewport's ring whenever a narrower one
    attached under smallest-wins. Only a viewport's OWN size change bumps
    its epoch. The test is
    `a_viewports_own_resize_bumps_its_epoch_and_a_width_change_reanchors_the_others`
    (renamed from the task text's "…bumps_all"). The post-review cleanup
    (below) changed WHERE the re-anchor lands and added the height-grow
    trigger.
  - **The viewport appends the tail of a partial overlap** instead of
    discarding the body (`remote/client.rs`,
    `scrollback2_partial_overlap_appends_only_the_tail`): a resend from a
    lagging ack produces exactly that, which RFC 0009 §3 had said never
    happens. RFC 0009 §3/§4 are amended (Task 3.5).
  - The epoch byte skips 0 on wrap (255 → 1): a viewport advertises 0 to
    mean it holds none (`bump_epoch`).
  - Test adjustments: every `send_paced_frames` / `paced_poll_timeout` test
    call gained its history argument; the source-swap keyframe test
    (`a_source_swap_marks_a_paced_client_dirty_and_its_next_frame_is_full`)
    drives the pass with `None` history, the overlay being up.
- **3.3 Part B — the flood, addressed** (`c950f8e`). `FloodCase::v2`,
  `paced_v2_flood_case`, a reference of every scrolled row, the
  `FloodViewport` model, frames handed to it by `FloodLedger` at the step
  they arrive, v2 acks on the cadence through `absorb_client_caps`, and an
  event-driven tail (never-acked: a fixed 20 s; the whole tail bounded at
  120 s). Six regression tests (`posh225_v2_*` and
  `non_paced_and_v1_paced_streams_are_identical_beside_a_v2_client`).
  Deviations:
  - `FloodViewport` applies a partial overlap as the viewport now does: the
    tail is appended, only the prefix counts as repeated (the task text
    counted the whole body).
  - `FloodRun::baseless_frames` (visible frames built with no acked base)
    is the lost-base witness, not `full_frames`: under a flood `DumpDiff`
    sends a `Full` whenever the diff is no net win, so every paced flood
    frame is `Full` even at prompt acks.
  - `print_flood_row` gained `nobase` and `v2 hbody max_hb uniq rep jmp
    jumped mism` (the task text named five v2 columns).
  - **History has no frame floor** (operator decision, 2026-10-06; see the
    corrected "Design choices"). The first cut gated fresh bodies on
    `PACED_FRAME_FLOOR_MS`, which capped history at 256 rows per 20 ms ≈
    12,800 rows/s and lost 4,111 of 20,511 rows of a 40,000 rows/s flood to
    eviction with PROMPT acks, where paced v1 lost none. Now history is
    limited by the window and socket backpressure only; a fresh body is
    capped at the room left in the window, so at most 512 rows are ever in
    flight (the window is exact); and the first screen/history tie goes to
    the screen (UX decision 3 — after the cleanup the coin bit is
    `Pacing.last_was_screen`, whose derived `false` makes the first tie the
    screen's). The floor stays for the screen. After: prompt acks deliver every row at
    1 KiB and 4 KiB chunks, peak backlog 5–8 KB, bodies of about one chunk's
    rows, one per loop iteration.
  - The `Lagged(2500)` row cannot reach the visible-base cliff the "Facts"
    predicted (≈ 2 s): the 2 MiB flood ends before a 2,500 ms ack lands. The
    FDR says so rather than claiming no cliff exists.
  - The commit narrows posh#240 rather than closing it (below).
- **3.5 — records** (this commit): RFC 0009 §5 (the session-socket path;
  §3's partial-overlap rule and §4's anchoring note amended; Covered
  Requirements extended; posh#243 noted as open); RFC 0008 §3.2 (history
  bodies at their own opportunities for a v2 client); FDR 0021
  (Limitations, Diagnostics, code pointers, decision 12's seam); this
  section. No man page: no lever exists until Task 3.4.
- **Measured** (ideal-reader harness as Stage 2's, 2 MiB flood, 20,511
  rows scrolled into a 10,000-row ring; the full v2 table is in FDR 0021;
  every non-v2 row is identical to the Part A baseline):
  - Prompt acks: every row delivered once at 1 KiB and 4 KiB chunks, peak
    backlog 5,149 B / 8,476 B.
  - RTT 50 ms at 4 KiB: 14,127 of 20,511 delivered; RTT 300 ms at 1 KiB:
    12,400 — the window (≈ 512 rows per RTT) is the limiter Task 3.4 makes
    dynamic. Every lost row is inside a forward jump; none is repeated or
    differs from the session's.
  - RTT 1,500 ms (256 KiB regression flood): every row delivered and acked,
    one window re-sent before the first latency sample, no frame built
    without an acked base (Stage 2's paced v1 acked 0 rows here).
  - Never acked: the viewport holds exactly 512 rows; one window re-sent at
    1, 2, 4 and 8 s (2,560 rows total), against Stage 2's one ring per
    250 ms.
  - Slow reader (1 KiB/ms): 14,111 delivered, 6,400 lost to eviction as
    exact forward jumps; one ≤ 26.7 KB body queued at a time.
- **Known limitations** (FDR 0021 Limitations): history throughput is one
  static window per RTT until Task 3.4; a body lost under a later one is an
  unrepairable forward jump indistinguishable from eviction (posh#243, with
  Stage 4); rows scrolled while detached are not delivered (Stage 5), and
  rows a width change or height grow left unsent become a forward jump at
  the resize (labelled by Stage 4); unpaced and relayed viewports stay on
  v1 until Stage 7; the slow-reader loss is larger than v1's ~4,300 and
  not yet analysed; ring rows a height grow pops back onto the grid
  re-enter the ring under new numbers when they re-scroll, so a viewport
  already holding them gets them twice (pre-existing, v1 too; untracked).
- **Post-review cleanup** (the commit after 3.5's records; whole-stage
  review + simplify): (a) a session HEIGHT GROW also re-anchors other
  viewports — posh-term pops ring rows back onto the grid without lowering
  the total, so the cursor's ring-index mapping would otherwise offer older
  rows under newer numbers (test by row content:
  `a_session_height_grow_reanchors_the_other_viewports`); a shrink pushes
  rows through the total like a scroll and needs nothing; (b)
  **`reanchor` lands at the viewport's row COUNT as of the resize**, not at
  its send cursor, so rows scrolled-but-unsent and sent-but-lost become one
  forward jump instead of a silent seam (UX decision 1;
  `reanchor_keeps_the_epoch_and_jumps_to_the_count_at_the_resize`); (c) a
  fresh body starts at `max(sent_upto, acked_rows)` — a late ack past a
  rewound resend no longer re-sends held rows
  (`a_late_ack_past_a_rewound_resend_is_not_sent_again`); (d) the cursor
  owns the window (`next_due`/`next_body(.., window)` cap a fresh body at
  the room left; `server_loop` passes an unbounded window) and the resend
  count (zeroed by an advancing ack, a bump or a re-anchor), `on_client_size`
  reports whether it bumped, `next_body` returns the epoch, `ClientConn`
  gains `history()`/`history_mut()`; (e) **screen priority was tried and
  reverted**: without the coin, a reader whose drain exceeds the 20 ms
  floor under continuous output gets no history until the output stops,
  which contradicts decision 3's "a share of what is left" (the lever's
  `0` is the only live-only mode); the coin stays as
  `Pacing.last_was_screen`; (f) `AckLatency::take_log_line` checks and
  stamps in one step; a comment says why `datagram::RttEstimator` is not
  reused (it drops ≥ 5 s samples, the stalled-viewport regime this exists
  to see).
- **posh#240 narrowed, not closed, by `c950f8e`:** a v2 viewport (paced,
  the default remote path) gets each row at most once by address; an
  unpaced or relayed viewport stays on v1 until Stage 7, and v2 still loses
  rows to eviction when a flood outruns one window per RTT for longer than
  the ring. Queue row 2 (posh#227) was already closed by the Stage 2 merge.
- **Task 3.4 is HELD.** Its gate: the field `paced ack latency` series from
  the session log — an idle session, a `nix gc` flood with this stage's
  static window, and the same flood with history held off (Task 3.4's
  "What the field data must show") — from which the operator answers open
  question 3.
- **Exit check: NOT yet done.** The Stage 3 exit check (end of Task 3.5)
  needs this build on the remote host; its step (1) is also Task 3.4's
  input.

---

## Stage 0 — pin the failure

### Task 0.1: Commit the measurement tests

**Promotion criteria:** N/A.

**Files:**
- Modify (already edited, uncommitted): `crates/posh/src/session/daemon.rs`
  — the `posh#225` block at the end of `mod tests` (`FloodAcks`,
  `FloodDrain`, `FloodRun`, `FloodCase`, `newline_flood`, `measure_flood`,
  and the two `#[ignore]` tests).
- Add: `docs/plans/2026-10-05-flood-delivery-ux-design.md`,
  `docs/plans/2026-10-05-flood-delivery.md`.

**Step 1: Confirm the block compiles and clippy is clean**

Run: `just debug-cargo clippy -p posh --all-targets -- -D warnings`
Expected: no warnings.

**Step 2: Commit**

```bash
git add crates/posh/src/session/daemon.rs docs/plans/2026-10-05-flood-delivery-ux-design.md docs/plans/2026-10-05-flood-delivery.md
git commit -m "posh#225: flood backlog measurements and the delivery plan"
```

### Task 0.2: A failing regression test for the drop

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/session/daemon.rs` (same test block, after
  `posh225_flood_backlog_ideal_reader_measurement`).

**Step 1: Write the failing test**

It reuses `measure_flood` with the socket-accurate drain and a full ring —
the field shape — and asserts what Stage 1 must make true.

```rust
    /// posh#225 regression: a newline flood into a session whose ring is
    /// already full must not push a healthy lossy client toward
    /// `MAX_CLIENT_BACKLOG`. The visible frame must stay screen-sized — it
    /// may not carry the scrollback ring — whatever the ack cadence.
    ///
    /// Scope: this pins the VISIBLE frame. With acks withheld entirely the
    /// v1 scrollback frame still re-carries every un-acked row (the second
    /// amplifier); that case is bounded by the v2 send cursor, not here.
    #[test]
    fn posh225_full_ring_flood_keeps_visible_frames_screen_sized() {
        const KIB: usize = 1024;
        let flood = newline_flood(256 * KIB);
        for acks in [FloodAcks::EveryNewest(1), FloodAcks::Lagged(5), FloodAcks::Never] {
            let case = FloodCase {
                chunk: 4 * KIB,
                acks,
                drain: FloodDrain::OneWritePerChunk,
                prefill_rows: SCROLLBACK + 200,
            };
            let r = measure_flood(&flood, case);
            assert!(
                r.largest_visible < 64 * KIB,
                "acks={}: a visible frame was {} bytes — it is carrying the scrollback ring",
                acks.label(),
                r.largest_visible,
            );
            if !matches!(acks, FloodAcks::Never) {
                assert_eq!(
                    r.crossed_backlog_at,
                    None,
                    "acks={}: a draining client crossed MAX_CLIENT_BACKLOG after {} bytes",
                    acks.label(),
                    r.fed,
                );
                assert!(
                    r.peak_write_buf < KIB * KIB,
                    "acks={}: backlog peaked at {} bytes against an ideal reader",
                    acks.label(),
                    r.peak_write_buf,
                );
            }
        }
    }
```

**Step 2: Run it and watch it fail**

Run: `just debug-cargo test --release -p posh --bin posh posh225_full_ring_flood -- --nocapture`
Expected: FAIL on the first cadence with
`a visible frame was 9xxxxx bytes — it is carrying the scrollback ring`.

**Step 3: Mark it expected-to-fail until Stage 1, and commit**

Add `#[ignore = "posh#225: fails until visible frames stop replaying the ring (Task 1.3)"]`
above `#[test]` so the merge gate stays green, then:

```bash
git add crates/posh/src/session/daemon.rs
git commit -m "posh#225: failing regression — visible frames carry the scrollback ring"
```

---

## Stage 1 — screen-sized visible frames

### Task 1.1: `Terminal::dump_vt_tail` in posh-term

**Promotion criteria:** N/A — additive. `dump_vt()` keeps its signature and
its bytes (frozen API); `posh history` keeps using it.

**Files:**
- Modify: `crates/posh-term/src/dump.rs:252-254` (`dump_vt`), `:269-273`
  (`dump_vt_flat`), `:275` (`dump_vt_impl` signature), `:331-358` (the
  primary-screen branch), and the `tests` module.
- Modify: `crates/posh-term/src/lib.rs:27-30` (the frozen-API list — add
  the new item; never change the existing lines).

**Step 1: Write the failing tests**

In `dump.rs`'s `mod tests`. They use only public API; "same visible state"
is compared through `dump_vt_flat()` of the two mirrors, which serializes
the active grid, modes and cursor.

```rust
    /// Mirror `dump` into a fresh ring-less terminal — what every frame
    /// consumer does (FDR 0005) — and return its visible state.
    fn mirror_flat(rows: u16, cols: u16, dump: &[u8]) -> Vec<u8> {
        let mut t = Terminal::with_scrollback(rows, cols, 0);
        t.process(dump);
        t.dump_vt_flat()
    }

    fn scrolled(rows: u16, cols: u16, ring: usize, lines: usize) -> Terminal {
        let mut t = Terminal::with_scrollback(rows, cols, ring);
        for i in 0..lines {
            t.process(format!("line {i:05} of scrolled output\r\n").as_bytes());
        }
        t.process(b"prompt$ ");
        t
    }

    /// The whole ring is `usize::MAX` rows of tail: byte-identical to
    /// `dump_vt`, which `posh history` and the frozen API depend on.
    #[test]
    fn tail_dump_with_no_bound_is_dump_vt() {
        let t = scrolled(24, 80, 1000, 500);
        assert_eq!(t.dump_vt_tail(usize::MAX), t.dump_vt());
    }

    /// A same-height mirror needs no scrollback at all: the zero-tail dump
    /// renders exactly what the full dump renders.
    #[test]
    fn zero_tail_dump_renders_like_the_full_dump_at_the_same_height() {
        let t = scrolled(24, 80, 1000, 500);
        assert_eq!(
            mirror_flat(24, 80, &t.dump_vt_tail(0)),
            mirror_flat(24, 80, &t.dump_vt()),
        );
    }

    /// A mirror TALLER than the source shows `extra` scrollback rows above
    /// the grid (the flow lands at the target's bottom). A tail of exactly
    /// `extra` rows reproduces that; this is the multi-client case.
    #[test]
    fn tail_dump_renders_like_the_full_dump_on_a_taller_mirror() {
        let t = scrolled(24, 80, 1000, 500);
        for target_rows in [25u16, 32, 50] {
            let extra = usize::from(target_rows - 24);
            assert_eq!(
                mirror_flat(target_rows, 80, &t.dump_vt_tail(extra)),
                mirror_flat(target_rows, 80, &t.dump_vt()),
                "target_rows={target_rows}",
            );
        }
    }

    /// The point of the bound: the dump's size follows the screen, not the
    /// ring. (posh#225: a 10,000-row ring made every frame ~1 MiB.)
    #[test]
    fn tail_dump_size_is_independent_of_ring_depth() {
        let t = scrolled(50, 200, 10_000, 20_000);
        assert!(t.dump_vt().len() > 300_000, "premise: the full dump carries the ring");
        assert!(
            t.dump_vt_tail(0).len() < 16_384,
            "zero-tail dump was {} bytes",
            t.dump_vt_tail(0).len(),
        );
    }

    /// Fewer scrollback rows than the requested tail: replay what exists.
    #[test]
    fn tail_dump_clamps_to_the_rows_the_ring_holds() {
        let t = scrolled(24, 80, 1000, 3); // nothing has scrolled off yet
        assert_eq!(t.dump_vt_tail(10), t.dump_vt());
    }
```

**Step 2: Run them and watch them fail**

Run: `just debug-cargo test -p posh-term tail_dump`
Expected: compile error — `no method named dump_vt_tail`.

**Step 3: Implement**

`dump_vt` and `dump_vt_flat` pass the bound through; only the primary
branch changes. Replace the bodies at `dump.rs:252-254` and `:269-273`:

```rust
    pub fn dump_vt(&self) -> Vec<u8> {
        self.dump_vt_impl(false, usize::MAX)
    }

    /// [`Terminal::dump_vt`] with the scrollback replay bounded to the
    /// NEWEST `max_scrollback_rows` rows of the ring.
    ///
    /// For frame transport (posh#225). A frame consumer mirrors the dump
    /// into a ring-less terminal, so replayed scrollback is discarded —
    /// except by a mirror taller than this terminal, which shows
    /// `target_rows - self.rows()` of those rows above the grid because the
    /// flow lands at its bottom. Pass that difference (0 for a same-height
    /// mirror) and the mirror renders exactly as it would from `dump_vt`,
    /// from a dump whose size follows the screen instead of the ring.
    /// `usize::MAX` is `dump_vt`.
    ///
    /// The same contract as `dump_vt` otherwise: the target may be larger,
    /// so nothing here derives a position from an assumed height — a zero
    /// tail takes the homed `draw_grid` branch that a terminal with no
    /// scrollback has always taken.
    pub fn dump_vt_tail(&self, max_scrollback_rows: usize) -> Vec<u8> {
        self.dump_vt_impl(false, max_scrollback_rows)
    }
```

```rust
    pub fn dump_vt_flat(&self) -> Vec<u8> {
        let mut out = Vec::from(DRAWABLE_STATE_RESET);
        out.extend_from_slice(&self.dump_vt_impl(true, 0));
        out
    }
```

Change the signature at `:275` to
`fn dump_vt_impl(&self, flat: bool, max_scrollback_rows: usize) -> Vec<u8>`
and the primary branch at `:331-358` to replay only the tail (`flat`
returns before this point, so its `0` is never read):

```rust
        let sb_len = self.primary.scrollback_len();
        let replay = sb_len.min(max_scrollback_rows);
        // (comment above `cursor_anchor` stays; the condition keys on the
        // rows actually replayed, not on the ring being non-empty.)
        let cursor_anchor = if replay > 0 && !self.alt_active {
            CursorAnchor::Relative
        } else {
            CursorAnchor::Absolute
        };
        if replay > 0 {
            for i in (sb_len - replay)..sb_len {
                let row = self.primary.scrollback_row(i).unwrap();
                self.emit_terminated_row(&mut out, row, &mut st);
            }
            for r in 0..self.primary.rows() {
                let row = self.primary.row(r).unwrap();
                self.emit_row(&mut out, row, &mut st);
                if !row.wrapped() && r + 1 < self.primary.rows() {
                    self.reset_pen(&mut out, &mut st);
                    out.push_str("\r\n");
                }
            }
        } else {
            self.draw_grid(&mut out, &self.primary, &mut st);
        }
```

Add to the frozen-API list in `lib.rs` after the `dump_vt` item:

```rust
//! - `Terminal::dump_vt_tail(&self, max_scrollback_rows: usize) -> Vec<u8>`
//!   (`dump_vt` with the scrollback replay bounded to the newest N rows;
//!   the frame-transport serializer, posh#225)
```

**Step 4: Run the new tests and the whole crate**

Run: `just debug-cargo test -p posh-term`
Expected: PASS, including every existing `taller_replay_*` test (they call
`dump_vt`, whose bytes are unchanged).

If `zero_tail_dump_renders_like_the_full_dump_at_the_same_height` fails,
the homed `draw_grid` branch and the bottom-landing flow disagree at equal
height. Stop: that is a real rendering difference, not a test to loosen.
Report it — the fallback is `dump_vt_tail(1)` as the minimum, which keeps
the flow branch.

**Step 5: Commit**

```bash
git add crates/posh-term/src/dump.rs crates/posh-term/src/lib.rs
git commit -m "posh-term: dump_vt_tail — bound the scrollback replay in a dump (posh#225)"
```

### Task 1.2: A ring-backed frame harness in poshterity

The harness is the only place a *multi-client* frame stream is asserted,
and today its server has no ring.

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/poshterity/src/framereplay.rs:50-96` (`ServerSide`),
  the `FrameHarness` constructor(s), and the tests near `:484-560`.

**Step 1: Read `FrameHarness::new`, `add_client`, `assert_mirrors_content`
and `assert_converged`** (same file) before editing; the snippet below
assumes `add_client(rows, cols) -> ClientId` and that each `ClientLane`
knows its client's rows. If the lane does not carry the client size, add a
`rows: u16` field set in `add_client`.

**Step 2: Write the failing test**

```rust
    /// The ring-replay branch, finally exercised: the server keeps real
    /// scrollback, so a taller client's frames carry a tail. Both clients
    /// must mirror the session, and no frame may carry the whole ring.
    #[test]
    fn a_taller_client_mirrors_a_scrolled_session_with_a_real_ring() {
        let mut h = FrameHarness::with_ring(24, 80, FrameSync::DumpDiff, 10_000);
        let tall = h.add_client(50, 80);
        for i in 0..2_000u16 {
            h.feed(format!("line {i:04}\r\n").as_bytes());
        }
        h.feed(b"prompt$ ");
        h.deliver_all();
        h.assert_mirrors_content(ClientId::PRIMARY);
        h.assert_mirrors_content(tall);
        h.assert_converged();
        assert!(
            h.largest_frame_bytes() < 16_384,
            "a frame was {} bytes — it carried the ring",
            h.largest_frame_bytes(),
        );
    }
```

**Step 3: Run it and watch it fail**

Run: `just debug-cargo test -p poshterity a_taller_client_mirrors_a_scrolled_session_with_a_real_ring`
Expected: compile error (`with_ring`, `largest_frame_bytes`).

**Step 4: Implement**

- `ServerSide::new(rows, cols, ring)`; `FrameHarness::new` passes `0`
  (unchanged behaviour), `FrameHarness::with_ring` passes the depth.
- In `ServerSide::baseline_now` and `encode_for`, take the lane and use
  `self.term.dump_vt_tail(tail_rows(lane.rows, self.term.rows()))` in place
  of `dump_vt()`, with

  ```rust
  /// Scrollback rows a frame must replay for a client `client_rows` tall
  /// mirroring a `session_rows` session — mirror of the daemon's rule.
  fn tail_rows(client_rows: u16, session_rows: u16) -> usize {
      usize::from(client_rows).saturating_sub(usize::from(session_rows))
  }
  ```
- Track the largest encoded `ServerFrame` in `ClientLane::send_frame`;
  expose `FrameHarness::largest_frame_bytes()`.

**Step 5: Run the crate**

Run: `just debug-cargo test -p poshterity`
Expected: PASS — the new test and every existing harness test.

**Step 6: Commit**

```bash
git add crates/poshterity/src/framereplay.rs
git commit -m "poshterity: ring-backed frame harness; frames replay a per-client tail"
```

### Task 1.3: The session daemon frames with a per-client tail

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/session/daemon.rs:414-421` (`queue_frame_from`),
  `:688-712` (`broadcast_output`), and the `#[ignore]` on the Task 0.2 test.

**Step 1: Un-ignore the Task 0.2 test** (delete its `#[ignore]` line) and
re-run it to confirm it still fails for the right reason.

Run: `just debug-cargo test --release -p posh --bin posh posh225_full_ring_flood`
Expected: FAIL — `it is carrying the scrollback ring`.

**Step 2: Implement**

Add to `impl ClientConn`, beside `queue_frame_from`:

```rust
    /// Scrollback rows this client's visible frames must replay
    /// (`Terminal::dump_vt_tail`): the rows by which its terminal is taller
    /// than `src`, which is all a ring-less mirror can show. A same-height
    /// client — the only client of most sessions, and every mux bridge —
    /// replays none, so its frames are screen-sized (posh#225).
    ///
    /// A client that has not reported a size yet gets the whole ring: the
    /// pre-posh#225 dump, correct at any height.
    fn frame_tail_rows(&self, src: &Terminal) -> usize {
        if self.rows == 0 {
            return usize::MAX;
        }
        usize::from(self.rows).saturating_sub(usize::from(src.rows()))
    }
```

`queue_frame_from` uses it:

```rust
    fn queue_frame_from(&mut self, src: &Terminal) -> bool {
        self.queue_frame(
            src.dump_vt_tail(self.frame_tail_rows(src)),
            Snapshot::from_term(src),
            src.is_alt_screen(),
            (src.rows(), src.cols()),
        )
    }
```

`broadcast_output` keeps deriving the shared inputs once, and derives the
dump once per *distinct tail* (one for most sessions):

```rust
fn broadcast_output(clients: &mut [ClientConn], term: &Terminal, bcast: &[u8]) {
    let frame_inputs = clients.iter().any(|c| c.producer.is_some()).then(|| {
        (
            Snapshot::from_term(term),
            term.is_alt_screen(),
            (term.rows(), term.cols()),
        )
    });
    // One dump per distinct tail: clients of the same height share it.
    let mut dumps: Vec<(usize, Vec<u8>)> = Vec::new();
    for c in clients.iter_mut() {
        let produced = match &frame_inputs {
            Some((snap, alt, dims)) if c.producer.is_some() => {
                let tail = c.frame_tail_rows(term);
                let dump = match dumps.iter().find(|(t, _)| *t == tail) {
                    Some((_, d)) => d.clone(),
                    None => {
                        let d = term.dump_vt_tail(tail);
                        dumps.push((tail, d.clone()));
                        d
                    }
                };
                c.queue_frame(dump, snap.clone(), *alt, *dims)
            }
            _ => false,
        };
        if !produced {
            c.queue(Tag::Output, bcast);
        } else {
            c.maybe_queue_scrollback(term);
        }
    }
}
```

(Keep the two existing comments in `broadcast_output`; the doc comment
above it gains one sentence: frames replay a per-client scrollback tail,
not the ring.)

**Step 3: Run the regression test, then the daemon's tests**

Run: `just debug-cargo test --release -p posh --bin posh posh225_full_ring_flood -- --nocapture`
Expected: PASS.

Run: `just debug-cargo test -p posh --bin posh session::daemon`
Expected: PASS. Existing tests call `c.queue_frame(term.dump_vt(), …)`
directly with a full dump; they stay valid (a full dump is a legal frame
input) and are not rewritten here.

**Step 4: Re-run the measurements and record the new numbers**

Run: `just debug-cargo test --release -p posh --bin posh posh225_flood_backlog -- --ignored --nocapture`
Expected: the "FULL ring" rows no longer cross; `max_vis` in the low
kilobytes. Paste the before/after rows for `4 KiB, newest/1, full ring`
into the commit message.

**Step 5: Commit**

```bash
git add crates/posh/src/session/daemon.rs
git commit -m "daemon: visible frames replay a per-client tail, not the scrollback ring

Fixes the posh#225 drop for every viewport, deployed ones included: a
Full body is still bytes a Terminal can process.

<before/after measurement rows>"
```

Do **not** write `Closes #225` here — the issue's definition of done
includes history, which Stage 3 delivers.

### Task 1.4: The roaming server and the remaining frame producers

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/remote/server.rs:2116` (`server_loop`'s
  `src.dump_vt()`).
- Check, do not change: `session/daemon.rs:1745` (`Tag::History` — must
  stay `dump_vt()`), `:1611`, `:1954`, `:1978` (`dump_vt_flat`, real-tty
  paths).

**Step 1: Write the failing test.** Find the nearest existing
`server_loop` test that reads frames off the wire (the file's `mod tests`,
e.g. the scrollback flood test near `remote/client.rs:5513` that drives a
real server) and add an assertion that, after scrolling more than a
screenful with a deep ring, no `FrameBody::Full` exceeds 16 KiB. If no
existing test drives `server_loop` with a deep ring, add one modelled on
that flood test and say so in the commit.

**Step 2: Implement.** `server_loop` is single-peer and sizes its terminal
to that peer, so the tail is the same difference:

```rust
                    stats.time_dump_vt(|| {
                        src.dump_vt_tail(
                            usize::from(client_size.0).saturating_sub(usize::from(src.rows())),
                        )
                    }),
```

(`client_size` is the `(rows, cols)` the loop already tracks; confirm the
name at the top of `server_loop` before editing.)

**Step 3: Run** `just debug-cargo test -p posh --bin posh remote::` —
expected PASS.

**Step 4: Commit**

```bash
git add crates/posh/src/remote/server.rs
git commit -m "server_loop: bounded-tail visible dump (posh#225 parity with the daemon)"
```

### Task 1.5: Records for Stage 1

**Files:**
- Modify: `CLAUDE.md` — the "Two serializers, two contracts" trap becomes
  three: `dump_vt()` (a full replica incl. history: `posh history`),
  `dump_vt_tail(n)` (frames: a ring-less mirror, tail = rows the mirror is
  taller by), `dump_vt_flat()` (a real tty). Keep it one bullet; this file
  is a router with a character cap.
- Modify: `docs/rfcs/0008-*.md` — one paragraph in the frame-body section:
  a visible `Full`/`Diff` is computed over a bounded-tail dump; receivers
  are unaffected.
- Comment on posh#225 with the mechanism, the measurements and what Stage 1
  did and did not fix (scrub hostnames and home paths).

**Step 1:** `just lint-fmt` — expected clean (the `agents-md` cap is a
merge-gate check; if the `CLAUDE.md` edit trips it, shorten the bullet,
do not raise the cap).

**Step 2: Commit**, then merge the stage: `merge-this-session` (its
pre-merge hook is the CI lane).

**Stage 1 exit check (from the user's seat):** on a remote mux attach to a
host running the new build, `nix gc` (or `just debug-posh-backlog-repro
distinct` adapted to a frame client) runs to completion with the viewport
attached, and the session log shows no `client backlog high-water` lines.

---

## Stage 2 — send-time, paced screen delivery

Expanded 2026-10-06 against HEAD `be8ccbb`.

Decisions 2, 5, 6, 13. After this stage the daemon holds at most one
unsent screen per capable viewport, whatever the frame size.

**The model, mirrored from `server_loop`:** per viewport, a `dirty` flag
and a *send opportunity*. An opportunity exists when the viewport's
`write_buf` is empty **and** the previous fresh frame is acknowledged or a
pacing interval has elapsed. `broadcast_output` stops encoding for these
viewports; it only marks them dirty. Encoding happens at the opportunity,
from the terminal as it is then — so screens produced in between are never
built, let alone queued. The PTY is read exactly as today (decision 5).

**Why ack-or-interval:** the daemon has no RTT for a viewport, but a lossy
viewport's `FrameAck` is end-to-end (viewport → bridge → daemon), so
waiting for it is real backpressure from the real link; the interval is
the floor that keeps a lost ack from stalling the screen. Start with
`server_loop`'s clamp (20 ms floor, 250 ms ceiling) and treat the exact
numbers as a tuning task with a recorded measurement.

### Facts re-verified at `be8ccbb` (Stage 2 rests on these)

Corrections to the task-level text that stood here before are marked
**corrected**.

- **Capability id.** `caps.rs` allocates 0–22 (`CAP_PUSH_CMD_REQUEST = 22`,
  `caps.rs:178`) plus 224/225; RFC 0001's table (`0001-*.md:231-257`)
  lists `23–223` unassigned (`:256`). RFC 0012 does **not** reserve a
  number — it says the id "MUST be allocated in RFC 0001's capability
  registry" and fixes none (`0012-*.md:51-54`). `CAP_PACED = 23`.
- **Payload shape to copy.** The forward-compatible decoder in the file is
  `decode_push_cmd_request` (`caps.rs:500-503`): reads its prefix, ignores
  trailing bytes. Exact-length decoders (`decode_scrollback2_client`
  `:538-547`, `decode_session_kind` `:474-479`) are the shape *not* to
  copy for a payload that must grow. Tests sit at the end of `caps.rs`'s
  `mod tests` (`push_cmd_ids_are_the_next_free_pair` `:1411`, the id-pin
  pattern).
- **The remote viewport advertises on every message.** `outgoing_caps`
  (`remote/client.rs:3793`) builds the table for every `ClientMessage`
  (the protocol is connectionless); `CAP_SCROLLBACK2` at `:3810-3818`,
  `CAP_MORPH` behind its gate at `:3826-3834`. Codec levers are parsed
  once into `ClientState` at construction (`framesync` at `:1775`, field
  `:1631`); there are two `ClientState` literals (`:1783`, `test_state`
  `:5369`). Default-on gates use `util::parse_default_on_gate`
  (`util.rs:99-104`; e.g. `mux::mux_sessions_selected`, `mux.rs:111-113`),
  and the About view lists gates (`client.rs:720-728`).
- **Where the daemon Init is formed — corrected.** The plan put the cap in
  `relay::content_caps` (`relay.rs:246-251`). ADR 0007 ("No new features on
  the relay or Architecture A … Anything new lands on M2", `0007-*.md:66-67`)
  forbids that. The M2 bridge forms the daemon Init from the **first**
  `ClientMessage` on an `Awaiting` channel at `remote/server.rs:986-995`
  (`content_caps(&msg.caps)` plus the bridge's own `CLIENT_IDENT`) and
  keeps it as `SessionBridge::content` (`:346`), which an FDR 0012 re-home
  reuses (`rehome_bridge`, `:1122-1135`). `CAP_PACED` is added there, so a
  relayed viewport stays unpaced — the same split RFC 0016 made for
  push-cmd (`the_relay_never_forwards_push_cmd`, `server.rs:3066`). It is
  Init-only: `bridge_client_message`'s per-message `Tag::ClientCaps`
  forward (`:1153-1170`) does not carry it, and the daemon reads it only on
  Init.
- **Bridge tests — corrected line numbers.** `:4724`/`:5080` were stale.
  The Init-table assertions are in `mux_peer_opens_daemonlink_per_session_channel`
  (`server.rs:4886`, Init decode `:4909-4913`) and
  `mux_peer_switch_rehomes_channel_with_frame_and_input_continuity`
  (`:5203`, `:5245-5258`); the cheap seams are `test_bridge` (`:2802`) and
  `rehome_bridge_seeds_frame_offset_and_reinits` (`:2915`).
- **The bridge drains the daemon socket as fast as it reads.** Every daemon
  `Tag::Frame` is rewrapped and sent on the wire at once
  (`server.rs:698-728`), holding only the newest for retransmit. So for a
  bridged viewport the daemon's `write_buf` is empty almost always; the
  end-to-end `FrameAck` (`forward_ack`, `relay.rs:197-217`) is the only
  backpressure that reflects the viewport's link. This is why the
  opportunity waits on the ack.
- **Daemon structure — corrected line numbers.** `ClientConn`
  `daemon.rs:150-275` (Stage 1's `visible_shaped_for` `:268`,
  `regeometry_keyframe` `:274`); `apply_init` `:350-378` (sets `lossy` /
  `coalesce` from the Init table — the shape `pacing` copies);
  `maybe_enable_frames` `:486-490`; `queue_frame_from` `:508-516`;
  `queue_frame` `:535-629`; `apply_frame_ack` `:647-693`;
  `maybe_queue_scrollback` `:718-795`; `broadcast_output` `:816-874`;
  `handle_frame_ack` `:889-893`; `broadcast_source_swap` `:902-909`;
  teardown (`ExitCause`/`Exit` to every client) `:1361-1371`;
  `daemon_loop` `:1427`; loop-top `now` `:1482`; high-water line
  `:1483-1498`; `clients.retain` drop `:1506-1522`; **`util::poll(&mut fds,
  -1)` at `:1553`** (the plan said `:1391`); PTY read + `broadcast_output`
  `:1680-1743` (call at `:1731`); overlay broadcast `:1776-1781`; client
  section `:1785-2159` — Init `:1836-1861`, `FrameAck` `:1944-1948`,
  **the one write per client `:1962-1991`** (the plan said `:1800-1829`),
  regeometry `:1999-2001`, replay `:2104-2128`; end-of-iteration
  activity-answer pass `:2161-2167`.
- **`POLLOUT` is armed only for a non-empty `write_buf`** (`:1527-1533`).
  A frame queued into an empty buffer at the end of an iteration is
  written on the next one. The paced send pass therefore belongs at the
  end of the iteration, beside the activity-answer pass.
- **The daemon has no generation tracking** (no `generation()` call in
  `daemon.rs`), and its broadcast source switches between two terminals
  (session and escape overlay, `active_source`) whose generation counters
  are unrelated. **Corrected:** `dirty` is an event flag set where the
  daemon already knows output reached the source (`broadcast_output`) and
  where an event owes a frame, not a generation comparison.
- **Every site that queues a visible frame today**, each of which must
  respect pacing for a paced client: `broadcast_output` (per PTY read and
  overlay read), the attach/regeometry replay (`:2124`), the RESYNC
  keyframe (`handle_frame_ack`), the activity-answer pass (`:2165` — it
  fires on every title change, so left alone it would bypass pacing during
  a flood that sets the title), and `broadcast_source_swap` (via
  `broadcast_output`).
- **Teardown sends `Exit` without a final frame** (`:1366-1370`). Today
  every screen was already queued; with pacing a dirty client's last
  screen would be lost. A flush is added.
- **`server_loop`'s clamp** lives in `remote/datagram.rs:29-30`
  (`SEND_INTERVAL_MIN = 20`, `SEND_INTERVAL_MAX = 250`, private);
  `send_interval()` is `datagram.rs:316-318` (SRTT/2 clamped). `server_loop`
  gates fresh frames on it at `server.rs:1982-1983` and sets its poll
  deadline from it at `:1434-1436`. The #117 quiescence nudge
  (`:1946-1969`, `visible_frame_leapt`) guards a v1-scrollback ack that
  leaps a visible frame; the daemon's scrollback threads off the visible
  frame queued just before it (`advance_scrollback_after_visible`,
  posh#181), so the daemon has no leap to guard. What the daemon must
  guarantee instead is that a dirty paced viewport always has a deadline
  (no stale screen at quiescence).
- **`FrameProducer`** (`posh-proto/src/framesync/producer.rs`):
  `acked_num()` `:132`, `last_visible_num()` `:378`, `acked_dump()` `:154`
  (the acked frame's dump — the test seam for "which screen did it get"),
  outstanding window 8 (`:262`). `ack` sets `acked_num` even when the
  acked frame was evicted (`:314`), so "the fresh frame is acked" is
  `acked_num() >= fresh_num` in every regime.
- **The flood harness drives `broadcast_output` directly**, not
  `daemon_loop` (`measure_flood`, `daemon.rs:5273-5406`). The fake clock is
  therefore a `now: u64` parameter on the new free functions, which
  `daemon_loop` calls with `util::now_ms()`. `FloodCase` (`:5190`) is a
  `Copy` struct literal at each call site; eight `ClientConn` literals
  exist (`:1640`, `:2606`, `:2729`, `:2772`, `:3378`, `:3663`, `:4299`,
  `:4508` — posh#230), so new state goes in **one** field.
- **v1 history under pacing — corrected expectation.** The plan asked for
  `peak_write_buf` < 64 KiB for every cadence. With v1 scrollback riding
  behind each paced visible frame, one scrollback frame carries every row
  scrolled since the viewport's ack — at 20 ms between frames and ~4 MB/s
  of output, ~800 rows (~90 KB) with prompt acks, up to a ring with none.
  Stage 2's bound is structural instead: **at most one visible frame and
  the one scrollback frame behind it are ever queued**, so the backlog
  cannot accumulate toward `MAX_CLIENT_BACKLOG`. The per-body cap is Stage
  3's (`SB2_ROWS_PER_BODY`).

### Design choices this expansion makes (the task-level text left them open)

- **v1 history for a paced viewport rides the same opportunity**, queued
  right behind the paced visible frame by the same call
  (`maybe_queue_scrollback`, unchanged). Reason: it is the least change
  that keeps the backlog bounded — emitting it per PTY read (as today)
  would reintroduce one queued frame per read — and it keeps posh#181's
  threading (a scrollback frame names the visible frame queued just before
  it). Stage 3 replaces it with v2 for paced viewports.
- **Stage 1's "never-acked viewports still cross the cap via v1 re-carry"
  is closed for paced viewports** as far as the *cap* goes: a scrollback
  frame is queued only at an opportunity, which needs an empty
  `write_buf`, so at most one ring (~1 MiB) is ever queued. It is **not**
  closed as *bandwidth*: a never-acking paced viewport is re-sent up to a
  ring of history every `PACED_ACK_WAIT_MS`. Stage 3 closes that.
  Measured in Task 2.5: with far fewer frames in flight per round trip, a
  remote viewport's acks land inside the producer's 8-frame window again,
  so it leaves the lost-base regime in which v1 history stops entirely —
  below the RTT cliff (≈ 5 × `PACED_ACK_WAIT_MS`), not above it.
- **Two constants, both used.** `PACED_FRAME_FLOOR_MS = 20` — the least
  time between two fresh frames however promptly the viewport acks (caps
  encode work during a flood at ≤ 50 frames/s); `PACED_ACK_WAIT_MS = 250`
  — the longest a dirty viewport waits for an ack. Both start at
  `server_loop`'s clamp; each is one `const` in `daemon.rs` with a comment
  naming it a tuning value that changes only with a measurement recorded
  in the FDR. They are not shared with `datagram.rs` because they will be
  tuned independently.
- **All paced state is one field**, `pacing: Option<Pacing>`: `Some`
  exactly when the Init table carried a well-formed `CAP_PACED`, so "not
  paced" is visibly `None` and each of the eight literals gains one line.
- **One predicate** (`ClientConn::paced_send_at`) feeds both the send pass
  and the poll timeout, so the two cannot disagree (a disagreement is
  either a busy loop or a stale screen).
- **Event sites use one verb.** `ClientConn::owe_paced_frame(release_ack_wait)`
  marks a paced client dirty and returns `true`, so every site reads
  `if !c.owe_paced_frame(..) { /* today's path */ }` and a non-paced
  client provably takes today's path. A RESYNC releases the ack wait (the
  viewport gave up on what was outstanding, so waiting for its ack would
  stall the recovery for `PACED_ACK_WAIT_MS`); nothing else does.
- **The regeometry frame under pacing.** `prepare_regeometry_frame`
  decides the debt exactly as for any client and, for a MorphDelta client,
  drops the base and sets `regeometry_keyframe = current_num() + 1`. The
  replay site turns the debt into `owe_paced_frame(false)`. The next paced
  frame is built by `queue_frame_from`, whose `note_visible_dump_shape`
  re-records `visible_shaped_for` and so ends the debt; on the paced path
  the next frame produced is always that visible frame (its scrollback
  frame follows it), so `current_num() + 1` still names the keyframe. It
  does not release the ack wait: frames in flight for the old geometry are
  still valid input. The new-geometry frame therefore lands within one ack
  (≤ `PACED_ACK_WAIT_MS`) — Stage 1's "about a round trip (plus pacing)"
  transient, now with a stated bound.
- **A self-acked paced client** (the reliable local socket, Stage 6) is
  always "acked", so its opportunity is "`write_buf` empty and the floor
  elapsed" — socket backpressure plus the floor. No special case.
- **The gate function is default-on only.** `parse_paced_gate` is
  `util::parse_default_on_gate`, matching the remote rollout. Stage 6's
  local attach defaults **off** (decision 13): Task 6.2 extends the one
  function with the path's default rather than adding a second parser.
  Not added now, because an unused `Local` variant fails `clippy -D
  warnings`.
- **Observability for the exit check is missing and is added (Task 2.7).**
  The mux peer's SIGUSR2 line reports only `session_channels=`
  (`server.rs:425-441`); nothing counts frames forwarded.

### Task 2.1: Allocate `CAP_PACED` (id 23)

**Promotion criteria:** N/A — an id nothing sends yet.

**Files:**
- Modify: `crates/posh-proto/src/caps.rs` — the constant after
  `CAP_PUSH_CMD_REQUEST` (`:173-178`); helpers after
  `decode_push_cmd_request` (`:500-503`); tests at the end of `mod tests`
  (after `push_cmd_offer_is_an_empty_entry`, `:1435`).
- Modify: `docs/rfcs/0001-target-grammar-and-capability-table.md` — a row
  for 23 after `:255`; `23–223` at `:256` becomes `24–223`.

**Step 1: Write the failing tests** (`caps.rs` `mod tests`):

```rust
    /// posh#225 Stage 2 takes the next free id after push-cmd's pair.
    /// Pinned so a later allocation cannot silently collide with it.
    #[test]
    fn paced_id_is_the_next_free_after_push_cmd_request() {
        assert_eq!(CAP_PACED, 23);
    }

    /// RFC 0008 §3.2: a version byte, nothing else in v1.
    #[test]
    fn paced_entry_carries_its_version() {
        let cap = encode_paced();
        assert_eq!((cap.id, cap.payload.clone()), (CAP_PACED, vec![PACED_VERSION]));
        assert_eq!(decode_paced(&cap.payload), Some(PACED_VERSION));
    }

    /// Room to grow (Stage 3 appends the history ceiling): a reader takes
    /// the version byte and ignores what follows; an empty payload or
    /// version 0 is malformed, and a malformed entry means "not paced".
    #[test]
    fn paced_entry_tolerates_appended_fields_and_rejects_an_empty_one() {
        assert_eq!(decode_paced(&[2, 0xaa, 0xbb]), Some(2));
        assert_eq!(decode_paced(&[]), None);
        assert_eq!(decode_paced(&[0]), None);
    }
```

**Step 2: Run** `just debug-cargo test -p posh-proto paced` — expected:
compile error, `cannot find value CAP_PACED`.

**Step 3: Implement.**

```rust
/// Paced delivery (posh#225; RFC 0008 §3.2, FDR 0021). Client entry: "decide
/// what to send me at send time". A session daemon then builds at most one
/// fresh visible frame for this client per send opportunity — its outgoing
/// buffer empty and its last fresh frame acked or a wait elapsed — from the
/// terminal as it is then, instead of one per PTY read. Payload: a version
/// byte ([`PACED_VERSION`]); later versions append fields a v1 reader
/// ignores. Init-only on the session socket; an M2 bridge carries the
/// viewport's entry into the daemon Init, a relay does not (ADR 0007), so a
/// relayed viewport is unpaced. A server never sends it.
pub const CAP_PACED: u8 = 23;
/// The [`CAP_PACED`] payload version this build writes.
pub const PACED_VERSION: u8 = 1;
```

```rust
/// This build's [`CAP_PACED`] entry.
pub fn encode_paced() -> Cap {
    Cap { id: CAP_PACED, payload: vec![PACED_VERSION] }
}

/// The version of a [`CAP_PACED`] payload: `None` when it is empty or names
/// version 0 (malformed: the entry is ignored and the client is unpaced).
/// Bytes after the version byte belong to later versions and are ignored.
pub fn decode_paced(payload: &[u8]) -> Option<u8> {
    payload.first().copied().filter(|v| *v != 0)
}
```

**Step 4: Run** `just debug-cargo test -p posh-proto caps` — expected PASS.

**Step 5: RFC 0001 row.** Insert after the id 22 row:

```
| 23 | `PACED` | client | ≥ 1 byte | Paced delivery (RFC 0008 §3.2, allocated 2026-10-06): a version byte (`1`); later versions append fields a reader ignores. Advertised on `Tag::Init` by a viewport that asks the session daemon to decide what to send at send time — at most one fresh visible frame per send opportunity, built from the terminal as it is then. An M2 bridge carries the viewport's entry into the daemon Init; a relay does not (ADR 0007), so a relayed viewport is unpaced. A server MUST NOT send it. |
```

and change the `23–223` row to `24–223`.

**Step 6: Commit** — message:
`posh-proto: allocate CAP_PACED (id 23) for send-time delivery (posh#225 Stage 2)`

### Task 2.2: The viewport advertises it; the M2 bridge carries it

**Promotion criteria:** N/A — until Task 2.3 the daemon ignores id 23
(unknown ids are skipped), so this changes no behaviour.

**Files:**
- Modify: `crates/posh/src/session/mod.rs` — the gate (shared by both
  clients; Stage 6 reuses it), tests in `mod tests` (`:999`).
- Modify: `crates/posh/src/remote/client.rs` — `ClientState` field beside
  `framesync` (`:1631`), its two literals (`:1783`, `test_state` `:5369`),
  `outgoing_caps` (`:3793`, after the `CAP_MORPH` block `:3826-3834`),
  the About gate list (`:720-728`), tests near
  `outgoing_caps_advertises_v2_and_drops_v1_once_acked` (`:6477`).
- Modify: `crates/posh/src/remote/server.rs` — the `Awaiting` → `Linked`
  Init formation (`:986-995`), tests near `:2915` and `:3066`.
- Do **not** modify `crates/posh/src/remote/relay.rs` (ADR 0007).

**Step 1: The gate — failing test** (`session/mod.rs` `mod tests`):

```rust
    /// `POSH_PACED` is a default-on off-switch (decision 13: the remote path
    /// ships on): unset, empty, `1` and anything unrecognised advertise;
    /// the shared off spellings do not.
    #[test]
    fn paced_gate_is_on_unless_switched_off() {
        for on in [None, Some(""), Some("1"), Some("yes"), Some("bogus")] {
            assert!(parse_paced_gate(on), "{on:?}");
        }
        for off in ["0", "false", "off", "no", " OFF "] {
            assert!(!parse_paced_gate(Some(off)), "{off:?}");
        }
    }
```

Run `just debug-cargo test -p posh --bin posh paced_gate` — expected:
compile error (`parse_paced_gate`).

**Step 2: Implement** in `session/mod.rs`:

```rust
/// The paced-delivery gate (posh#225, FDR 0021): whether a viewport
/// advertises `CAP_PACED`. A default-on off-switch — the shared
/// [`util::parse_default_on_gate`] shape, `POSH_PACED=0` the rollback, read
/// by the viewport so it takes effect on its next attach without
/// restarting the session. Pure, so tests need not touch the environment.
pub fn parse_paced_gate(value: Option<&str>) -> bool {
    util::parse_default_on_gate(value)
}

/// [`parse_paced_gate`] of `$POSH_PACED`. Read once per attach.
pub fn paced_selected() -> bool {
    parse_paced_gate(std::env::var("POSH_PACED").ok().as_deref())
}
```

Run — expected PASS.

**Step 3: The viewport — failing tests** (`client.rs` tests):

```rust
    /// posh#225 Stage 2: a viewport advertises CAP_PACED on every message
    /// (the bridge forms the daemon Init from whichever arrives first).
    #[test]
    fn outgoing_caps_advertises_paced_by_default() {
        let mut st = test_state(5, 20);
        let caps = outgoing_caps(&mut st);
        let entry = caps::find(&caps, caps::CAP_PACED).expect("paced advertised");
        assert_eq!(caps::decode_paced(&entry.payload), Some(caps::PACED_VERSION));
    }

    /// `POSH_PACED=0`: today's delivery, nothing advertised.
    #[test]
    fn outgoing_caps_omits_paced_when_the_gate_is_off() {
        let mut st = test_state(5, 20);
        st.paced = false;
        assert!(caps::find(&outgoing_caps(&mut st), caps::CAP_PACED).is_none());
    }
```

Run `just debug-cargo test -p posh --bin posh outgoing_caps` — expected:
compile error (`no field paced`).

**Step 4: Implement.** `ClientState` gains, after `framesync`:

```rust
    /// Paced delivery (`POSH_PACED`, posh#225): whether every message
    /// advertises `CAP_PACED`. Read once at construction.
    paced: bool,
```

`run`'s literal (`:1783`) sets `paced: crate::session::paced_selected()`;
`test_state` sets `paced: true` (the default). In `outgoing_caps`, after
the `CAP_MORPH` block:

```rust
    // posh#225 Stage 2 (RFC 0008 §3.2): ask for send-time delivery. Every
    // message, like SCROLLBACK2: the M2 bridge forms the daemon Init from
    // the first message it sees on a channel.
    if st.paced {
        extra.push(caps::encode_paced());
    }
```

In the About gate list (`:720-728`) add
`gate("POSH_PACED", crate::session::paced_selected())` after
`POSH_MUX_SESSIONS` (one more `{}` in the format string) — it answers "is
this viewport paced?" from the palette until Stage 7's `posh status` field.

Run `just debug-cargo test -p posh --bin posh outgoing_caps` — expected
PASS, including `outgoing_caps_advertises_v2_and_drops_v1_once_acked`.

**Step 5: The bridge — failing tests** (`server.rs` tests, after
`the_relay_never_forwards_push_cmd`):

```rust
    /// posh#225 Stage 2, ADR 0007: the M2 bridge carries the viewport's
    /// CAP_PACED into the daemon Init iff the viewport advertised it.
    #[test]
    fn bridge_init_carries_paced_iff_the_viewport_advertised_it() {
        let with = bridge_init_content(&caps::own_table(&[caps::encode_paced()]));
        assert_eq!(caps::find(&with, caps::CAP_PACED), Some(&caps::encode_paced()));
        let without = bridge_init_content(&caps::own_table(&[]));
        assert!(caps::find(&without, caps::CAP_PACED).is_none());
    }

    /// The relay never forwards it: a relayed viewport stays unpaced.
    #[test]
    fn the_relay_never_forwards_paced() {
        let table = vec![caps::encode_paced()];
        assert!(crate::remote::relay::content_caps(&table).is_empty());
        assert!(crate::remote::relay::forwarded_client_caps(&table).is_empty());
    }

    /// FDR 0012 re-home re-Inits the new daemon with the same negotiation:
    /// a paced viewport stays paced across a switch.
    #[test]
    fn rehome_bridge_keeps_paced_in_the_reinit() {
        let (mut b, _peer) = test_bridge();
        b.content = bridge_init_content(&[caps::encode_paced()]);
        let mut connect = |_: &str| {
            let (a, other) = std::os::unix::net::UnixStream::pair().unwrap();
            std::mem::forget(other);
            Ok(a)
        };
        rehome_bridge(&mut b, "work/s-1", &mut connect).unwrap();
        let mut fb = crate::session::ipc::FrameBuffer::new();
        fb.feed(&b.daemon.link.write);
        let init = fb.next().unwrap().unwrap();
        assert_eq!(init.tag, crate::session::ipc::Tag::Init);
        let (table, _) = caps::decode_table(&init.payload[4..]).unwrap();
        assert!(caps::find(&table, caps::CAP_PACED).is_some());
    }
```

Run `just debug-cargo test -p posh --bin posh paced` — expected: compile
error (`bridge_init_content`).

**Step 6: Implement** beside `bridge_client_message`:

```rust
/// The client half of the daemon `Tag::Init` this bridge sends for a
/// channel: the relay's content caps (RFC 0008 §4) plus what only the M2
/// bridge carries (ADR 0007) — the viewport's `CAP_PACED` entry
/// (posh#225, RFC 0008 §3.2), verbatim. The caller appends its own
/// identity. Retained as `SessionBridge::content`, so a re-home re-Inits
/// with the same negotiation.
fn bridge_init_content(client_caps: &[caps::Cap]) -> Vec<caps::Cap> {
    let mut content = crate::remote::relay::content_caps(client_caps);
    content.extend(caps::find(client_caps, caps::CAP_PACED).cloned());
    content
}
```

and at `:987` replace `crate::remote::relay::content_caps(&msg.caps)` with
`bridge_init_content(&msg.caps)`.

**Step 7: Run** `just debug-cargo test -p posh --bin posh remote::` —
expected PASS (`mux_peer_opens_daemonlink_per_session_channel` still sees
its lossy Init; its `cm()` advertises no `CAP_PACED`, which is fine).
Then `just debug-cargo clippy -p posh --all-targets -- -D warnings` —
clean.

**Step 8: Commit** — message:
`viewport advertises CAP_PACED (POSH_PACED, default on); the M2 bridge carries it into the daemon Init (posh#225 Stage 2)`

### Task 2.3: Daemon — paced delivery core

The commit that makes a paced viewport paced. FDR 0021 is created at
`experimental` and RFC 0008 gains §3.2 **in this commit**.

**Promotion criteria:** N/A — opt-out on the viewport (`POSH_PACED=0`,
next attach).

**Files:**
- Modify: `crates/posh/src/session/daemon.rs` — constants beside
  `MAX_CLIENT_BACKLOG` (`:62`); a `Pacing` struct after `ClientConn`
  (`:275`); the `pacing` field on `ClientConn` and all eight literals
  (`:1640`, `:2606`, `:2729`, `:2772`, `:3378`, `:3663`, `:4299`, `:4508`
  — `pacing: None`); `apply_init` (`:350-378`); new `ClientConn` methods
  after `maybe_enable_frames` (`:486-490`); `broadcast_output`
  (`:816-874`); new free functions after `broadcast_source_swap`
  (`:902-909`); `daemon_loop`'s poll (`:1553`) and end of iteration
  (`:2161-2167`); `daemon_main`'s teardown (`:1361-1371`).
- Create: `docs/features/0021-flood-delivery.md`.
- Modify: `docs/rfcs/0008-unified-session-frame-transport.md` — new
  `#### 3.2` after §3.1 (`:153-181`, before `### 4.` at `:183`).

**Step 1: Record the non-paced witness BEFORE touching code.** Run
`just debug-cargo test --release -p posh --bin posh posh225_flood_backlog_ideal_reader_measurement -- --ignored --nocapture`
and keep the printed table (it goes in this task's commit message). Step 12
re-runs it; every row must be identical — the client in that harness is
not paced, so its stream must not move.

**Step 2: Failing tests — state and predicate** (new block at the end of
`daemon.rs`'s `mod tests`, headed `// ---- posh#225 Stage 2: paced
delivery ----`). A helper first:

```rust
    /// A lossy client shaped like an M2 bridge's daemon Init with the
    /// viewport's CAP_PACED, plus any `extra` content caps.
    fn paced_conn(rows: u16, cols: u16, extra: &[caps::Cap]) -> (ClientConn, UnixStream) {
        let mut table = vec![caps::encode_paced()];
        table.extend_from_slice(extra);
        lossy_conn(rows, cols, &table)
    }
```

Tests (one line each on what they assert):

- `paced_cap_on_init_makes_a_paced_client_and_a_bare_reinit_keeps_it` —
  `paced_conn` → `c.pacing == Some(Pacing::default())`; after setting
  `dirty = true`, a bare 4-byte re-`Init` (`apply_init(&encode_resize(..))`)
  leaves `pacing` (and `dirty`) intact; `lossy_conn(..., &[])` →
  `pacing == None`; an Init whose `CAP_PACED` payload is empty → `None`.
- `broadcast_output_only_marks_a_paced_client_dirty` — feed a line,
  `broadcast_output(slice, &term, b"x")`: the paced client's `write_buf`
  is empty, `pacing.dirty` is true, `visible_shaped_for` is unchanged,
  and the producer's `current_num()` did not move.
- `paced_send_at_waits_for_an_empty_buffer_then_an_ack_or_the_wait` —
  with a fake `now`: not dirty → `None`; dirty and never sent → `Some(0)`;
  after `send_paced_frames(.., now = 100)`: not dirty → `None`; dirty
  again with the frame unacked and `write_buf` cleared →
  `Some(100 + PACED_ACK_WAIT_MS)`; after `apply_frame_ack` of
  `last_visible_num()` → `Some(100 + PACED_FRAME_FLOOR_MS)`; with
  `write_buf` non-empty → `None`.
- `paced_poll_timeout_is_the_nearest_deadline_and_never_negative` — no
  clients / no paced clients / a clean paced client → `-1`; two dirty
  paced clients due at 120 and 350 with `now = 100` → `20`; an overdue one
  → `0`; a non-paced client with a full `write_buf` changes nothing.
- `send_paced_frames_builds_one_frame_from_the_terminal_as_it_is_then` —
  dirty a paced client, feed three more distinct lines WITHOUT calling the
  pass, then `send_paced_frames(.., now)`: exactly one `Tag::Frame` with a
  visible body was queued; ack it; `producer.acked_dump()` equals
  `term.dump_vt_mirror(rows, cols)` (the latest screen, not an
  intermediate one); `pacing == Some(Pacing { dirty: false, last_fresh:
  Some((last_visible_num, now)) })`; a second pass at the same `now`
  queues nothing.
- `send_paced_frames_carries_scrollback_right_behind_the_visible_frame` —
  `paced_conn(.., &[CAP_SCROLLBACK])`, attach keyframe acked, scroll ten
  rows, pass: two frames, visible then `FrameBody::Scrollback` whose
  `base` is the visible frame's number (posh#181), carrying the ten rows.
- `flush_paced_frames_sends_a_dirty_clients_last_screen` — a paced client
  dirty with an unacked frame outstanding and `now` before its deadline:
  `flush_paced_frames` queues a frame anyway; a clean paced client and a
  non-paced client get nothing.

Run `just debug-cargo test -p posh --bin posh paced` — expected: compile
errors (`Pacing`, `send_paced_frames`, …).

**Step 3: Implement the state.**

```rust
/// Paced delivery (posh#225, RFC 0008 §3.2): the least time between two
/// fresh visible frames to one paced viewport, however promptly it acks.
/// Starts at `server_loop`'s send-interval floor (`datagram::SEND_INTERVAL_MIN`).
/// A tuning value: change it only with a measurement recorded in FDR 0021.
const PACED_FRAME_FLOOR_MS: u64 = 20;
/// The longest a dirty paced viewport waits for the ack of its last fresh
/// frame before it is sent the next anyway — what keeps a lost ack from
/// stalling its screen. Starts at `server_loop`'s send-interval ceiling
/// (`SEND_INTERVAL_MAX`). A tuning value, as above.
const PACED_ACK_WAIT_MS: u64 = 250;
```

```rust
/// A paced client's send-time state (posh#225, RFC 0008 §3.2).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Pacing {
    /// A visible frame is owed: output reached the broadcast source since
    /// the last paced frame, or an event (attach replay, regeometry,
    /// resync, activity answer, source swap) asked for one.
    dirty: bool,
    /// The newest paced visible frame's number and when it was queued (the
    /// caller's clock). `None` before the first, and after a RESYNC (the
    /// client gave up on what was outstanding): the next frame then waits
    /// only for an empty `write_buf`.
    last_fresh: Option<(u64, u64)>,
}
```

`ClientConn` gains, after `regeometry_keyframe`:

```rust
    /// Paced delivery (posh#225): `Some` exactly when this client's Init
    /// carried a well-formed `CAP_PACED`. Its visible frames are not built
    /// per PTY read: `broadcast_output` and every other frame-owing site
    /// mark it dirty (`owe_paced_frame`), and `send_paced_frames` builds at
    /// most one per send opportunity (`paced_send_at`).
    pacing: Option<Pacing>,
```

In `apply_init`, beside `lossy`/`coalesce` (preserved across a bare
re-Init exactly as they are):

```rust
                    // posh#225 (RFC 0008 §3.2): a viewport that asked for
                    // send-time delivery. A repeated Init keeps the state.
                    let paced = caps::find(&advertised, caps::CAP_PACED)
                        .is_some_and(|c| caps::decode_paced(&c.payload).is_some());
                    self.pacing = paced.then(|| self.pacing.unwrap_or_default());
```

**Step 4: Implement the predicate and the verbs** (`impl ClientConn`):

```rust
    /// Paced AND framed: a paced client without a producer (not frame
    /// capable) takes the raw `Tag::Output` path like any other.
    fn is_paced(&self) -> bool {
        self.pacing.is_some() && self.producer.is_some()
    }

    /// When this client may next be sent a fresh visible frame, on the
    /// caller's clock: `None` when it is not paced, owes nothing, or still
    /// has bytes queued (`POLLOUT` will wake the loop for those). The ONE
    /// predicate behind `send_paced_frames` and `paced_poll_timeout`, so
    /// the two cannot disagree — a disagreement is a busy loop or a stale
    /// screen.
    fn paced_send_at(&self) -> Option<u64> {
        let pacing = self.pacing.filter(|p| p.dirty)?;
        let producer = self.producer.as_ref()?;
        if !self.write_buf.is_empty() {
            return None;
        }
        Some(match pacing.last_fresh {
            None => 0,
            Some((num, at)) if producer.acked_num() >= num => at + PACED_FRAME_FLOOR_MS,
            Some((_, at)) => at + PACED_ACK_WAIT_MS,
        })
    }

    /// For a paced client, record that a visible frame is owed and return
    /// `true`: the caller must not build one now — `send_paced_frames` will.
    /// `release_ack_wait` also forgets the outstanding frame (a RESYNC: the
    /// client rejected it, so its ack is not coming). Any other client:
    /// `false`, and the caller takes today's path.
    fn owe_paced_frame(&mut self, release_ack_wait: bool) -> bool {
        if !self.is_paced() {
            return false;
        }
        if let Some(p) = self.pacing.as_mut() {
            p.dirty = true;
            if release_ack_wait {
                p.last_fresh = None;
            }
        }
        true
    }

    /// Build this paced client's fresh visible frame from `src` now — for
    /// its current geometry (`queue_frame_from`, which also records
    /// `visible_shaped_for`), with any v1 scrollback right behind it
    /// (posh#181 threading) — and record it as the newest fresh frame.
    fn send_paced_frame(&mut self, src: &Terminal, now: u64) {
        if !self.queue_frame_from(src) {
            return;
        }
        self.maybe_queue_scrollback(src);
        let num = self.producer.as_ref().map_or(0, FrameProducer::last_visible_num);
        if let Some(p) = self.pacing.as_mut() {
            *p = Pacing { dirty: false, last_fresh: Some((num, now)) };
        }
    }
```

**Step 5: Implement the pass, the timeout and the flush** (free functions
after `broadcast_source_swap`):

```rust
/// The paced send pass (posh#225, RFC 0008 §3.2): every paced client with a
/// send opportunity at `now` gets ONE fresh visible frame built from `src`
/// as it is now. Screens produced since its last frame were never built.
/// Runs at the end of a loop iteration; `now` is a parameter so tests
/// drive a fake clock.
fn send_paced_frames(clients: &mut [ClientConn], src: &Terminal, now: u64) {
    for c in clients.iter_mut() {
        if c.paced_send_at().is_some_and(|at| now >= at) {
            c.send_paced_frame(src, now);
        }
    }
}

/// The daemon's poll timeout: milliseconds until the nearest paced send
/// opportunity, `0` when one is already due, `-1` (block) when no paced
/// client owes a frame — never a busy-wait.
fn paced_poll_timeout(clients: &[ClientConn], now: u64) -> i32 {
    clients
        .iter()
        .filter_map(ClientConn::paced_send_at)
        .map(|at| at.saturating_sub(now))
        .min()
        .map_or(-1, |ms| i32::try_from(ms).unwrap_or(i32::MAX))
}

/// Before `Exit`: every paced client that still owes a frame gets it now,
/// whatever its pacing — the session's last screen must not be lost.
fn flush_paced_frames(clients: &mut [ClientConn], src: &Terminal, now: u64) {
    for c in clients.iter_mut().filter(|c| c.pacing.is_some_and(|p| p.dirty)) {
        c.send_paced_frame(src, now);
    }
}
```

**Step 6: `broadcast_output` skips paced clients.** Three edits, so a
paced client takes no dump, no cache key and no frame:

- `producers` counts `c.producer.is_some() && !c.is_paced()`;
- `keys` maps `(c.producer.is_some() && !c.is_paced()).then(|| c.note_visible_dump_shape(term))`
  (a paced client's `visible_shaped_for` must keep naming the dump it
  actually holds);
- the loop's first statement:
  `if c.owe_paced_frame(false) { continue; }` with the comment "A paced
  client is only marked dirty: its frame is built at its next send
  opportunity (`send_paced_frames`), from the terminal as it is then."

The doc comment above `broadcast_output` gains one sentence saying so.

**Step 7: Wire `daemon_loop` and the teardown.**
- `:1553`: `util::poll(&mut fds, paced_poll_timeout(clients, now))` (`now`
  is the loop-top `util::now_ms()` at `:1482`).
- After the activity-answer pass (`:2161-2167`, which reuses its `src`):
  `send_paced_frames(clients, src, util::now_ms());` with a comment that
  it runs last so the frame reflects everything this iteration fed the
  terminal, and that a frame queued here is written next iteration
  (`POLLOUT`, `:1527-1533`).
- `daemon_main` teardown, before the `ExitCause` loop (`:1366`):
  `flush_paced_frames(&mut clients, &term, util::now_ms());` (the overlay
  is already closed there, so `term` is the source).

**Step 8: Run** `just debug-cargo test -p posh --bin posh paced` —
expected PASS. Then `just debug-cargo test -p posh --bin posh session::daemon`
— expected PASS: no existing test constructs a paced client.

**Step 9: FDR 0021.** Create `docs/features/0021-flood-delivery.md`
(`status: experimental`, `date:` the commit date), modelled on FDR 0020's
sections (Problem Statement, Interface, Examples, Decisions, Limitations,
Rollback, More Information) plus FDR 0019's Tuning Levers:
- *Problem*: posh#225 — a session that out-produces a viewport dropped it;
  Stage 1 shrank the frame, Stage 2 bounds the queue.
- *Interface*: `POSH_PACED` on the viewport (default on for remote/M2
  attach; `0`/`false`/`off`/`no` off; read per attach); what the user sees
  (the live screen jumps to latest; at most one screen in flight); local
  attach unpaced until Stage 6; a relayed viewport (`POSH_MUX_SESSIONS=0`)
  unpaced (ADR 0007).
- *Decisions*: link the UX design doc's decisions 2, 5, 6, 13 (do not
  restate them) and this stage's choices: ack-or-wait opportunity, the
  floor, v1 history riding the opportunity, RESYNC releases the wait,
  regeometry waits for the next opportunity, the exit flush.
- *Limitations*: v1 history (≤ one ring re-carried per `PACED_ACK_WAIT_MS`
  to a never-acking viewport; Stage 3); a mismatched-geometry viewport
  still gets ring-sized frames, but one at a time; history still lost
  across reconnect (Stage 5); posh#226 (a drop is unexplained).
- *Tuning Levers*: `PACED_FRAME_FLOOR_MS`, `PACED_ACK_WAIT_MS` — values,
  origin (`server_loop`'s clamp), "measurement: Task 2.5" placeholder that
  Task 2.8 fills.
- *Rollback*: `POSH_PACED=0` on the viewport; next attach; no session
  restart; two viewports on one session may differ.

**Step 10: RFC 0008 §3.2** — "Paced delivery (posh#225)". Normative:
- A client advertising `CAP_PACED` on `Tag::Init` (§1.1) asks the daemon to
  decide at send time. The daemon MUST NOT queue a visible frame for it per
  PTY read; it MUST hold at most one unsent visible frame for it, plus the
  history body that rides behind that frame.
- A *send opportunity* exists when the client's outgoing buffer is empty
  AND its last fresh visible frame is acknowledged (a `FrameAck` at or
  beyond it, or the §2 self-ack) or an implementation-defined wait has
  elapsed since it was queued. Implementations SHOULD also space fresh
  frames by a floor. At an opportunity the daemon builds the frame from
  the terminal as it is then, for the client's current geometry (§2).
- Events that owe a frame (attach, the §2 regeometry frame, a RESYNC,
  an activity answer, a source swap) mark it owed and are served at the
  next opportunity; a RESYNC releases the wait for an ack.
- A daemon MUST deliver an owed frame before it sends `Exit`.
- Pacing MUST NOT apply backpressure to the PTY: output is read and fed to
  the terminal as for any client (UX design decision 5).
- A client that does not advertise `CAP_PACED` receives §2/§3 delivery
  unchanged. An M2 bridge forwards its viewport's entry into the daemon
  Init; a relay does not (ADR 0007).
- The intervals are implementation values (FDR 0021), not protocol.

**Step 11: Lint.** `just lint-fmt` — clean;
`just debug-cargo clippy -p posh --all-targets -- -D warnings` — clean.

**Step 12: Re-run the Step 1 measurement** — expected: byte-identical
table (the harness client is not paced).

**Step 13: Commit** — message (paste Step 1's table under the body):
`daemon: send-time, paced visible frames for CAP_PACED viewports (posh#225 Stage 2)`
with a body naming: at most one visible frame (+ its scrollback) queued
per paced viewport; ack-or-wait opportunity with a floor; poll timeout =
nearest opportunity; exit flush; non-paced clients byte-identical
(measurement table unchanged); FDR 0021 experimental; RFC 0008 §3.2.

### Task 2.4: Daemon — every frame-owing event respects pacing

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/session/daemon.rs` — `handle_frame_ack`
  (`:889-893`); the replay block (`:2104-2128`) extracted into a helper;
  the activity-answer pass (`:2161-2167`) extracted into a helper; tests in
  the Stage 2 block.

**Step 1: Failing tests:**

- `a_paced_attach_replay_is_built_by_the_send_pass` — a fresh
  `paced_conn`, `queue_replay(&mut c, &term)`: nothing queued, `dirty`;
  `send_paced_frames(.., now = 0)`: one `Full` visible frame (the fresh
  producer has no base).
- `a_paced_resync_releases_the_ack_wait` — paced client with an unacked
  fresh frame sent at `now = 100`, then `handle_frame_ack(&mut c,
  &encode_frame_ack(n, FRAME_ACK_RESYNC), &term)`: nothing queued at once;
  `paced_send_at() == Some(0)`; the pass at `now = 101` queues a `Full`.
- `a_paced_regeometry_frame_is_the_next_paced_frame` — paced client at
  24x80, frame sent and acked; `apply_resize` to 30x80;
  `prepare_regeometry_frame()` is true; `queue_replay` queues nothing and
  sets `dirty`; after the opportunity, the frame is queued,
  `visible_shaped_for == Some((30, 80))` and `owes_regeometry_frame()` is
  false.
- `a_paced_morph_regeometry_frame_is_a_full` — the same with
  `paced_conn(.., &[CAP_MORPH])`: the paced frame is a `Full` and
  `regeometry_keyframe` names its number.
- `a_paced_activity_answer_rides_the_next_paced_frame` — paced client that
  wants activity; title changes twice with no opportunity in between
  (`queue_due_answers` called after each): nothing queued; the next pass
  queues ONE frame carrying the second label.
- `a_source_swap_marks_a_paced_client_dirty_and_its_next_frame_is_full` —
  `broadcast_source_swap` queues nothing for the paced client; the pass
  queues a `Full`.

Run `just debug-cargo test -p posh --bin posh a_paced` — expected:
compile errors (`queue_replay`, `queue_due_answers`).

**Step 2: Implement.**

```rust
fn handle_frame_ack(c: &mut ClientConn, payload: &[u8], src: &Terminal) {
    // A paced client's recovering Full waits only for an empty buffer:
    // the RESYNC released its ack wait.
    if c.apply_frame_ack(payload) && !c.owe_paced_frame(true) {
        c.queue_frame_from(src);
    }
}
```

Extract the replay body (`:2118-2127`) as

```rust
/// The attach / regeometry replay for one client (github #16; posh#225):
/// a paced client is marked as owing a frame, a framed one is queued a
/// frame for its geometry now, a baseline one the flat dump.
fn queue_replay(c: &mut ClientConn, src: &Terminal) {
    let produced = c.owe_paced_frame(false) || (c.producer.is_some() && c.queue_frame_from(src));
    if !produced {
        c.queue(Tag::Output, &src.dump_vt_flat());
    }
}
```

keeping the existing comments at the call site, and the activity pass as

```rust
/// RFC 0013 §5.2 / RFC 0016 §2: a due answer that did not ride a frame
/// this iteration rides one of its own — for a paced client, its next
/// paced frame (a title that changes every line would otherwise bypass
/// pacing).
fn queue_due_answers(clients: &mut [ClientConn], src: &Terminal) {
    for c in clients.iter_mut().filter(|c| c.producer.is_some() && c.answer_due()) {
        if !c.owe_paced_frame(false) {
            c.queue_frame_from(src);
        }
    }
}
```

`broadcast_source_swap` needs no change: it drops every base, then
`broadcast_output` marks the paced client dirty, so its next paced frame is
a `Full`. The regeometry path needs no change beyond `queue_replay`: see
"The regeometry frame under pacing" above.

**Step 3: Run** `just debug-cargo test -p posh --bin posh session::daemon`
— expected PASS (the regeometry, resync and activity tests of Stage 1 and
earlier exercise the non-paced branch of the same helpers).

**Step 4: Commit** — message:
`daemon: replay, resync, regeometry and activity frames respect pacing (posh#225 Stage 2)`

### Task 2.5: The flood, paced — harness, regression tests, measurement

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/session/daemon.rs` — the posh#225 test block:
  `FloodCase` (`:5190`), `FloodRun` (`:5154`), `measure_flood`
  (`:5273-5406`), every `FloodCase` literal (`pace: None`), the
  ideal-reader measurement (`:5501`), new tests after `:5654`.

**Step 1: Extend the harness** (no new assertions yet).
- `FloodCase` gains `pace: Option<u64>` — `Some(ms)` runs a PACED client
  on a fake clock that advances `ms` per chunk (`1` models ~4 MB/s, the
  `nix gc` shape); `None` is today's client, byte-for-byte today's loop.
- `FloodRun` gains `visible_frames: usize`, `visible_frames_during_flood:
  usize`, `max_visible_queued: usize` (visible frames in `write_buf` after
  any queueing step), and `last_screen_delivered: Option<bool>` (paced
  runs only).
- In `measure_flood`, when `case.pace` is `Some(ms)`:
  - build the client with `paced_conn(rows, cols, &[SCROLLBACK, BASE_SUM])`;
  - the attach keyframe goes through the real path:
    `c.owe_paced_frame(false); send_paced_frames(slice, &term, 0);`, then
    ack frame 1 and clear `write_buf` as today;
  - per chunk, in production order: `term.process` + `broadcast_output`
    (marks dirty, queues nothing); the drain step (unchanged); the ack
    step (unchanged); then `send_paced_frames(slice, &term, now)`, parse
    the bytes it queued with the existing frame-accounting loop (moved into
    a closure so both modes share it), push `newest`; then `now += ms`;
  - after the last chunk, an idle tail: repeat drain → ack (per cadence) →
    `now += ms` → send pass until `!dirty && write_buf.is_empty()`;
    assert it ends within `now ≤ flood_end + 4 * PACED_ACK_WAIT_MS`
    (otherwise the pacing has a stale-screen hole); then ack
    `last_visible_num()` and set `last_screen_delivered =
    Some(producer.acked_dump() == Some(&term.dump_vt_mirror(rows, cols)[..]))`.
- `print_flood_row` gains a `paced` column.

Run `just debug-cargo test -p posh --bin posh posh225` — expected PASS:
every existing case has `pace: None`.

**Step 2: Failing tests** (they fail on assertions only if pacing is
broken; write them, run them, and expect PASS — if one fails, the core is
wrong; stop and report rather than loosen it):

- `posh225_paced_flood_without_acks_sends_one_visible_frame_per_ack_wait`
  — 2 MiB flood, 4 KiB chunks, `pace: Some(1)`, `FloodAcks::Never`,
  `FloodDrain::Always` (isolates pacing from the socket): the flood spans
  `t = 0..=511`, so `visible_frames_during_flood == 1 + (511 -
  PACED_FRAME_FLOOR_MS) / PACED_ACK_WAIT_MS` (frames at 20 and 270 —
  computed from the constants, not hard-coded); `last_screen_delivered ==
  Some(true)`.
- `posh225_paced_flood_with_prompt_acks_never_queues_two_visible_frames`
  — 256 KiB, `EveryNewest(1)`, `OneWritePerChunk`, full ring:
  `max_visible_queued <= 1`; `visible_frames <= chunks /
  PACED_FRAME_FLOOR_MS as usize + 2`; `largest_visible < 64 KiB`.
- `posh225_paced_flood_backlog_is_one_frame_pair_for_every_cadence` — for
  `Never`, `EveryNewest(1)`, `Lagged(2)`, `Lagged(5)` × prefill `0` and
  `SCROLLBACK + 200`, `OneWritePerChunk`, the WHOLE 256 KiB flood (pacing
  removes the per-chunk quadratic that made `Never` too slow before):
  `crossed_backlog_at == None`; `max_visible_queued <= 1`;
  `peak_write_buf <= largest_visible + largest_scrollback` (at most one
  pair queued); `largest_visible < 64 KiB`. No 64 KiB bound on
  `peak_write_buf` — see the corrected expectation above; Task 3.3 adds it
  when v2 caps the body.
- `posh225_paced_flood_ends_on_the_last_screen` — every cadence above:
  `last_screen_delivered == Some(true)` (no stale screen at quiescence).
- `posh225_paced_flood_delivers_history_with_prompt_acks` — 256 KiB,
  `EveryNewest(1)`, `OneWritePerChunk`, full ring; guarded by
  `flood_socket_supports_bounds`: `rows_acked == rows_scrolled` after the
  tail. (If the host's write capacity is below a paced scrollback frame —
  ~90 KB at a 20 ms floor — the guard skips; that is the harness, not
  posh.)
- `non_paced_stream_is_identical_beside_a_paced_client` — the
  "existing path untouched" witness that stays in the suite: two
  identical 24x80, 1000-row terminals fed `newline_flood(64 KiB)` in 4 KiB
  chunks through `broadcast_output`; run A has one
  `scrollback_capable_conn(24, 80)`; run B has the same plus a
  `paced_conn(24, 80, &[])` of the SAME geometry (so the pre-Stage-2 dump
  cache would have shared a dump with it). Drain the non-paced client's
  `write_buf` into a `Vec` after each chunk; the two `Vec`s are equal.

Run `just debug-cargo test -p posh --bin posh posh225_paced` and
`just debug-cargo test -p posh --bin posh non_paced_stream` — expected
PASS. Release mode (`--release`) if debug is too slow, as for the existing
flood tests.

**Step 3: Measure.** Add paced rows (`pace: Some(1)`, the same cadences
and rings) to `posh225_flood_backlog_ideal_reader_measurement`, run it
(`just debug-cargo test --release -p posh --bin posh posh225_flood_backlog_ideal_reader_measurement -- --ignored --nocapture`),
and keep the paced rows for Task 2.8. In particular record, for `Lagged(5)`
paced, whether `sb_acked` is now non-zero — the row that tests the "remote
viewport leaves the lost-base regime" expectation above. (Measured: it holds
below the RTT cliff, ≈ 5 × `PACED_ACK_WAIT_MS`, not above; see "Stage 2 as
built".)

**Step 4: Commit** — message (measurement rows in the body):
`posh#225 Stage 2: paced flood regression tests and measurements`

### Task 2.6: The backstop stays, and says so

**Promotion criteria:** N/A.

`MAX_CLIENT_BACKLOG` remains for non-frame clients and bugs (decision 6).
A drop of a paced viewport must read as a bug.

**Files:**
- Modify: `crates/posh/src/session/daemon.rs` — the high-water line
  (`:1487-1495`) and the drop line (`:1508-1517`); a test in the Stage 2
  block.

**Step 1: Failing test** —
`backlog_log_fields_say_whether_the_client_is_paced`: for a `paced_conn`
and a `lossy_conn`, `backlog_log_fields(&c, now)` ends in `paced=1` and
`paced=0` respectively and still carries `fd=`, `backlog=`,
`drained_total=`, `last_drain_age_ms=`.

Run `just debug-cargo test -p posh --bin posh backlog_log_fields` —
expected: compile error.

**Step 2: Implement** — one formatter both lines use:

```rust
/// The fields both backlog log lines carry (posh#131 diagnosis): stalled vs
/// bursty, and `paced=` — a paced viewport holds at most one frame pair
/// (RFC 0008 §3.2), so a paced client at the high-water mark or dropped is
/// a posh bug, not a slow reader. Telling the viewport why it was dropped
/// is posh#226.
fn backlog_log_fields(c: &ClientConn, now: u64) -> String {
    format!(
        "fd={} backlog={} drained_total={} last_drain_age_ms={} paced={}",
        c.stream.as_raw_fd(),
        c.write_buf.len(),
        c.bytes_drained,
        now.saturating_sub(c.last_drain_ms),
        u8::from(c.is_paced()),
    )
}
```

The lines become `client backlog high-water {}` and `dropping slow client
{}` with `backlog_log_fields(c, now)`; the existing prefixes stay (they are
grep targets).

**Step 3: Run** `just debug-cargo test -p posh --bin posh session::daemon`
— expected PASS.

**Step 4: Commit** — message:
`daemon: backlog log lines say paced= — a dropped paced viewport is a bug (posh#225; reason to the viewport is posh#226)`

### Task 2.7: A frame counter on the mux peer (for the exit check)

**Promotion criteria:** N/A — diagnostic only.

Verified missing: the agent-only/mux peer's SIGUSR2 line
(`server.rs:425-441`) reports `session_channels=` and no frame count.

**Files:**
- Modify: `crates/posh/src/remote/server.rs` — `SessionBridge`
  (`:309-347`) gains `visible_forwarded: u64`; its literal at `:1017-1034`
  and `test_bridge` (`:2802`) set `0`; the `Tag::Frame` arm (`:698-728`)
  increments it when `!scrollback`; the SIGUSR2 line gains
  `session_frames={}`.
- Modify: `doc/posh-server.1.scd` — the SIGUSR2 paragraph's mux-peer
  sentence (`:226-230`) names the visible-frame count.

**Step 1: Failing test** — `session_frames_forwarded_sums_linked_channels`:
two `PeerChannel::Linked(test_bridge())` with counts 3 and 4 plus an
`Awaiting` channel → `session_frames_forwarded(&channels) == 7`.

**Step 2: Implement** `fn session_frames_forwarded(channels: &[PeerChannel]) -> u64`
and use it in the SIGUSR2 line. Run
`just debug-cargo test -p posh --bin posh session_frames_forwarded` —
expected PASS; `just lint-doc` — clean.

**Step 3: Commit** — message:
`mux peer: count visible frames forwarded in the SIGUSR2 line (posh#225 Stage 2 exit check)`

### Task 2.8: Records

**Files:**
- Modify: `docs/features/0021-flood-delivery.md` — Tuning Levers gets
  Task 2.5's measured paced rows (and how to re-run them); Limitations
  gets the Lagged-cadence finding either way.
- Modify: `doc/posh-client.1.scd` ENVIRONMENT (beside `POSH_FRAMESYNC`,
  `:187`) and `doc/posh.1.scd` ENVIRONMENT (beside `POSH_FRAMESYNC`,
  `:498`): `POSH_PACED` — default on for remote attach, `0` off, read per
  attach, takes effect on the next attach without restarting the session;
  local attach unaffected until Stage 6. (Task 7.1 is the checklist; the
  lever's entry lands with the lever.)
- Modify: this plan — a "Stage 2 as built" section in the style of
  "Stage 1 as built", superseding this stage's task text where it differs.

**Step 1:** `just lint-doc` and `just lint-fmt` — clean (scdoc: no line
starting with `[`, no `*` inside `_italic_`).

**Step 2: Commit** — message:
`docs: POSH_PACED in the man pages; FDR 0021 measurements; Stage 2 as built (posh#225)`
with `Closes #227` in the body (queue row 2: decision 6 settled it; this
stage implements it).

**Step 3:** merge the stage with `merge-this-session` (its pre-merge hook
is the CI lane; do not run `just` first).

**Stage 2 exit check:** a remote flood shows the bridge forwarding a
handful of visible frames per second instead of one per PTY read — send
SIGUSR2 to the mux peer twice a few seconds apart and compare
`session_frames=` in its per-client-host log (`<base>/agent/mux-<ID>.log`,
`posh-server(1)`) — and Ctrl-C mid-flood repaints within one pacing
interval. Unverified: whether `just debug-posh-dump <pid>` signals the mux
peer process; if it does not, `kill -USR2 <mux peer pid>` and read that log.

---

## Stage 3 — addressed history on the daemon path, with the trickle

Expanded 2026-10-06 against HEAD `5be221b`.

Decisions 1, 3, 7 (and 2, 5, 6 constrain it). After this stage a paced
remote viewport gets its history as RFC 0009 v2 bodies — every row
addressed in an epoch-scoped row space, acknowledged cumulatively, resent
from the acknowledgement, and skipped past (a forward jump) only when the
session's ring evicted it first — instead of v1 frames that re-carry every
un-acked row. Task 3.4 (the dynamic trickle) is expanded but **HELD**.

**The model, mirrored from `server_loop`:** one addressed history stream
per viewport beside its paced screen. At a send opportunity the daemon
sends ONE body: the screen when it is owed, a history body when rows are
pending, and — when both are — whichever kind did not go last (the
`last_was_sb` coin). History never occupies a visible frame slot and is
never acknowledged by `acked_frame` (RFC 0009 §4), so it cannot lose the
screen's diff base, and the screen's ack pacing cannot starve it.

### Facts re-verified at `5be221b` (Stage 3 rests on these)

Corrections to the task-level text that stood here before are marked
**corrected**.

- **`server_loop`'s v2 state — corrected line numbers.** `SB2_ROWS_PER_BODY
  = 256` at `remote/server.rs:52-55`. The state is seven loose locals at
  `:1328-1341` (`sb2_active`, `sb2_epoch` starting at 1, `sb2_epoch_base`,
  `sb2_acked_rows`, `sb2_sent_upto`, `sb2_last_send`, `sb2_size`). The
  per-message client entry opens the epoch on first sight at the current
  monotonic total and takes `max(acked, entry.acked_rows)` only when the
  entry's epoch matches (`:1892-1909`). A changed client size bumps the
  epoch (`wrapping_add`) and re-anchors only while active, but `sb2_size`
  tracks the size from the start (`:1910-1922`). Wanting a body:
  `avail > sent_upto || resend_due`, where `resend_due = acked < sent_upto
  && now - last_send >= conn.rto()` (`:2046-2054`); the coin is
  `make_sb2 = want_sb2 && (!want_visible || !last_was_sb)` (`:2055`). The
  body (`:2215-2249`) starts at `max(resend_due ? acked : sent_upto,
  floor_rel)` with `floor_rel = avail - min(ring_len, avail)` — eviction is
  a forward jump — maps relative row `r` to ring index `ring_len - (avail -
  r)`, caps at `SB2_ROWS_PER_BODY`, and sets `sent_upto`/`last_send`. It
  rides `frame_num = producer.current_num()` without advancing the producer
  (`:2187-2193`, `:2544`). The server entry `encode_scrollback2_ack(epoch)`
  rides every frame while active (`:2345-2350`). The plan's `:1301-1307`,
  `:1863-1888`, `:2012-2022`, `:2172-2205` were stale.
- **`server_loop`'s v2 path has one end-to-end test**, in the CLIENT's
  module: `remote::client::tests::wedge_repro_server_loop_with_loss_and_titles`
  (`client.rs:5487`), which asserts v2 engaged. `server.rs`'s own
  scrollback tests (`run_scrollback_session`, `:4205`) drive v1 only. So
  "the existing v2 tests pass untouched" means that one test plus the
  client-side apply tests (`scrollback2_apply_rules_never_touch_applied_num`
  `:6421`, `scrollback2_epoch_adoption_resets_ring_and_count` `:6465`).
- **The "keep in sync" comment is NOT removed by `HistoryCursor` —
  corrected.** `daemon.rs:905` ("mirror of server.rs:761-770 — keep in
  sync") sits in `maybe_queue_scrollback` and mirrors `server_loop`'s **v1**
  row-range computation (`server.rs:2267-2277` today), which both producers
  keep. Task 3.1 only repairs its stale line reference.
- **Where the v2 Init entry goes — corrected.** The plan put
  `CAP_SCROLLBACK2` in `relay::content_caps` (`relay.rs:246-251`). ADR 0007
  forbids additions to the relay (the same correction Stage 2 made for
  `CAP_PACED`). The M2 bridge forms the daemon Init in `bridge_init_content`
  (`server.rs:1158-1168`, called at `:1007` from the first `ClientMessage`
  on an `Awaiting` channel) and retains it as `SessionBridge::content`
  (`:346`), which `rehome_bridge` re-Inits with (`:1143-1156`). The
  per-message forward is `bridge_client_message` (`:1170-1220`): the relay's
  `forwarded_client_caps` (`relay.rs:230-244`: ident, state, upstream,
  activity) plus the bridge's own additions (push-cmd, `:1186-1196`), sent
  as one `Tag::ClientCaps` (`:1197-1203`), then `forward_ack`
  (`relay.rs:197-217`). Both v2 additions go in the bridge.
- **`SessionBridge::content` is frozen at the first message.** A re-home
  re-Inits with that first message's caps, so a `SCROLLBACK2` entry in it
  would carry the viewport's position as of the channel's first message
  (epoch 0, 0 rows). Task 3.2 refreshes the entry in `content` on every
  message.
- **The bridge holds Scrollback2 frames in its scrollback slot**
  (`relay::is_scrollback_frame`, `relay.rs:404-409`; `held.hold`,
  `server.rs:747`) and releases them on `acked_frame >= frame_num`
  (`relay.rs:364-370`). A v2 body rides the newest visible number, so once
  that visible frame is acked the bridge stops retransmitting a lost body:
  **the daemon's own resend from the v2 ack is required for correctness**,
  not an optimisation.
- **The viewport** (`remote/client.rs`): advertises `CAP_SCROLLBACK2` on
  every message carrying `{ring_depth 0, epoch (0 while unknown),
  acked_rows = T}` and keeps v1 `CAP_SCROLLBACK` only until it has adopted
  an epoch (`outgoing_caps`, `:3800-3832`); omits both for exactly one
  message after its own resize (`suppress_scrollback_once`). On ANY own
  size change — height included — it clears its ring, sets `sb2_epoch =
  None` and `T = 0` (`:2045-2094`). It adopts the server's epoch from the
  `SCROLLBACK2` ack on any frame — caps are read before the body
  (`:3137-3146`, then `apply_frame` at `:3174`) — clearing its ring on a
  change. A v2 body is handled before the stale gate (`:3219-3252`): wrong
  epoch → discard; fully covered → dup; partial overlap → discard; else
  append (a forward jump is appended silently) and `T = end`. The plan's
  `:3126-3139` and `:3212-3245` were stale. **It does not clear its ring on
  a reconnect or an FDR 0012 switch**, so today's v1 history continues
  across both with a silent seam.
- **`ScrollbackRing`** is `remote/sync.rs:550-580` (`new`, `append`,
  `clear`); the daemon's tests already import it (`daemon.rs:2983`).
- **Caps** (`posh-proto/src/caps.rs`): `CAP_SCROLLBACK2 = 10` (`:72-80`);
  `Scrollback2Client {ring_depth, epoch, acked_rows}`,
  `encode/decode_scrollback2_client` (`:542-574`, exactly 10 bytes),
  `encode/decode_scrollback2_ack` (`:576-592`, `{0x02, epoch}`).
  `CAP_PACED = 23`, `PACED_VERSION = 1` (`:179-190`); `decode_paced` reads
  the version byte and ignores the rest (`:528-530`) — Task 3.4's ceiling
  appends there. `FrameBody::Scrollback2 { epoch, row_offset, rows }` is
  `posh-proto/src/frame.rs:145-149`.
- **Daemon — what Stage 2 and posh#239 left.** `ClientConn` `daemon.rs:161-302`
  (`pacing: Option<Pacing>` `:294`, `init_applied` `:301`,
  `visible_shaped_for` `:281`, `regeometry_keyframe` `:288`); `Pacing
  { dirty, last_fresh: Option<u64> }` `:313-325` (derives `Default, Copy,
  PartialEq`); `absorb_client_caps` `:332-357` — **ignores
  `CAP_SCROLLBACK2` today**; `apply_init` `:400-436` (re-derives `pacing`
  from each table, preserving its state: `paced.then_some(self.pacing
  .unwrap_or_default())`); `paced_send_at` `:576-587`; `owes_paced_frame`
  `:591`; `request_frame_from` `:614`; `send_paced_frame` `:622-631`
  (visible frame, then v1 `maybe_queue_scrollback`); `build_frame_from`
  `:648`; `queue_frame` `:675-773` (frame caps are
  `own_table(&activity_cap)` at `:731`, the only place a server entry can
  ride a visible frame); `apply_frame_ack` `:792-843` (no clock
  parameter); `maybe_queue_scrollback` `:868-945` (v1, `want =
  frame_sb_total - max(acked_sb_total, sb_floor)`); `backlog_log_fields`
  `:961-970`; `broadcast_output` `:985-1050` (a paced client is skipped
  entirely, `:1027`); `handle_frame_ack` `:1069-1073`; `send_paced_frames`
  `:1117-1123`; `paced_poll_timeout` `:1128-1135`; `flush_paced_frames`
  `:1139-1143`; `reset_scrollback_floors_on_reflow` `:1465-1475` (early
  return unless the session WIDTH changed). In `daemon_loop`: the
  high-water / drop lines `:1745-1773`, the poll `:1806`, the Init arm
  `:2091-2119` (sets `sb_floor` for a freshly framed client at `:2107-2109`),
  `Tag::ClientCaps` `:2125-2134`, `Tag::FrameAck` `:2202-2206`, the
  disconnect line `:2316-2322`, the `resized` block `:2326-2343`, the
  replay `:2350-2366`, the end-of-iteration passes `:2399-2406`, the exit
  flush `:2413-2417`. Eight `ClientConn` literals (`:1925`, `:2891`,
  `:3022`, `:3067`, `:3687`, `:3974`, `:4612`, `:4823`; posh#230).
- **The session daemon has no SIGUSR2 dump and no ack timing.** No
  `SIGUSR2` in `session/` (`rg`); `paced_send_at` only polls
  `acked_num()`. Its diagnostic surfaces are (a) its log, which it always
  opens (`util::log_init(&cfg.log_path(name))`, `daemon.rs:1516`;
  `<base>/…/<session>.log`) — the high-water and drop lines
  (`backlog_log_fields`) and `client disconnected fd= remaining=`; and (b)
  the RFC 0014 status socket (`status_response`, `:1655-1678`; four
  callers, one in `server.rs:1547`), whose client lines are rendered from
  the CLIENT's own reported record (`introspect::render_client_line`) in a
  fixed key order (RFC 0014 §4.2). A paced viewport never reaches the
  high-water or drop line — its backlog is bounded — so on a healthy field
  run the only line that names it today is the disconnect line, which
  carries no fields.
- **The flood harness acks through `apply_frame_ack` directly**
  (`measure_flood`, `daemon.rs:5786`, `:5823`, `:5866`, `:5888`), not
  `handle_frame_ack` (`:4486`, `:4492`, `:5026`, `:5135`, `:6797` and the
  loop are its only callers). Its `FloodLedger::account` classifies a frame
  as visible unless it is a v1 `Scrollback` (`:6014`) — a `Scrollback2`
  would be miscounted as visible. `FloodCase` (`:5651-5666`) gains a field
  in every literal.
- **The Task 0.2 carve-out stays — corrected.**
  `posh225_full_ring_flood_keeps_visible_frames_screen_sized`
  (`daemon.rs:6262`) runs a NON-paced client (`pace: None`). Stage 3 keeps
  non-paced viewports on v1 byte-for-byte, so its `Never` carve-out stays
  until Stage 7 retires per-read delivery. The bound it defers is asserted
  for paced v2 viewports by Task 3.3's tests instead.
- **FDR 0021's slow-reader loss is not fully removable — corrected
  expectation.** At `Trickle(1 KiB)` the reader drains ~10 rows/ms against
  ~40 rows/ms scrolled; rows the ring evicts before their turn are gone
  whatever the body size. What Stage 3 changes: the backlog is one ≤ 256-row
  body (not a ring-sized frame), every lost row is a forward jump of
  exactly the evicted span (the viewport accounts for it; Stage 4 draws
  it), and the viewport ends holding the whole retained ring.
- **posh#240 is closed only for v2 viewports.** The issue
  (`v1 scrollback re-carry + retained-base apply can append rows twice`)
  also reaches unpaced lossy viewports — a relayed one
  (`POSH_MUX_SESSIONS=0`, ADR 0007) or `POSH_PACED=0` — which stay on v1
  until Stage 7. The issue text itself asks to be closed with a pointer
  when Stage 3 lands; the commit that closes it says what remains.
- **The visible-base RTT cliff moves.** Stage 2's cliff (≈ 5 ×
  `PACED_ACK_WAIT_MS`) came from v1 scrollback frames occupying half of
  `FrameProducer`'s 8-frame outstanding window (`producer.rs:262`). v2
  bodies occupy none, so a paced v2 viewport's visible base survives an RTT
  up to ≈ 8 × `PACED_ACK_WAIT_MS` less one pace (≈ 2 s). *Unverified until
  Task 3.3 Part B measures it* (a `Lagged(2500)` row is added for that).
- **poshterity's framereplay does not model scrollback**
  (`poshterity/src/framereplay.rs:24`, `:159-162`: "Scrollback production
  isn't modelled here yet (github #75 follow-up)").

### Design choices this expansion makes (the task-level text left them open)

- **Ack latency is sampled in `handle_frame_ack`, not `apply_frame_ack`.**
  The plan said "stamped in `apply_frame_ack`", which has 34 call sites and
  no clock; `handle_frame_ack` is the daemon loop's one entry and has five
  test callers, so it gains `now: u64`. **Sample rule, corrected while
  executing 3.0 (2026-10-06):** the expansion first said "the ack that
  confirms the NEWEST paced frame, timed from `last_fresh`". That never
  fires while output is continuous and the RTT exceeds
  `PACED_ACK_WAIT_MS` — a newer frame is always queued before the ack
  lands — which is exactly the slow-link regime the signal exists for
  (the `Lagged(300)` flood test produced zero samples). The rule is now:
  `Pacing` keeps a small send log of `(frame number, queued at)` for its
  paced visible frames (capped at 16, cleared on RESYNC); an ack that
  ADVANCES `acked_num` samples `now − queued_at` of the newest logged frame
  at or below the acked number (so an ack naming a scrollback slot N+1
  times visible frame N), and drops every entry at or below it, so each
  frame is sampled at most once. A repeated or RESYNC ack is no sample.
  Every ack stamps `last_ack_at`. Unpaced clients record nothing (the
  state lives in `Pacing`), so their streams and literals are untouched.
- **3.0's surfaces are log lines, not the status socket.** `posh status`
  client lines are the client's own record in RFC 0014 §4.2's fixed key
  order; adding daemon-measured keys means an RFC 0014 edit and four
  `status_response` callers — that is Task 7.1's "`posh status` shows each
  viewport's mode", where it belongs. 3.0 adds the latency to
  `backlog_log_fields`, gives the disconnect line those fields, and adds one
  throttled `paced ack latency` line per paced viewport (≤ one per
  `ACK_LOG_INTERVAL_MS` = 10 s, only when new samples arrived), so a field
  `nix gc` leaves a time series in the session log without detaching.
- **`HistoryCursor` lives in `crates/posh/src/remote/history.rs`, not
  posh-proto.** Both users (`server_loop`, the session daemon) are in
  `posh`; framereplay does not model scrollback, so nothing outside `posh`
  needs it. When the github #75 follow-up models v2 in poshterity, the
  move is mechanical (the type depends only on `posh_term::Terminal` and
  posh-proto types). `SB2_ROWS_PER_BODY` moves with it.
- **The cursor keeps `server_loop`'s inactive-until-advertised shape**
  (`active` inside, `size` tracked from construction) so the extraction is
  exact, including the corner where the first v2 message also changes size.
- **A new attachment CONTINUES the viewport's epoch at its count.** The
  daemon opens a cursor per attachment, while the viewport keeps its ring
  across a reconnect and a switch. Opening a fresh epoch would make the
  viewport clear its ring on every mux reconnect — a visible regression of
  today's (v1) behaviour. So the cursor opens from the Init's
  `SCROLLBACK2` entry: epoch 0 (the viewport holds none) → a fresh epoch 1
  at row 0; epoch `e`, count `T` → epoch `e`, the next scrolled row is row
  `T`. That reproduces today's forward-only seam exactly, and it is the
  input Stage 5 needs (`HistoryStart` takes a starting position; Stage 5
  passes an earlier absolute one from the resume cursor). Decision 10's
  "fill order and starting position are per-viewport inputs" is met by the
  same type. Consequence: an FDR 0012 switch continues the viewport's
  epoch into the new session's rows — today's v1 behaviour; Stage 5 gives
  a switch a fresh epoch.
- **The bridge refreshes the `SCROLLBACK2` entry in `content` and dedupes
  its forward.** Every viewport message carries the entry; the bridge
  forwards it as `Tag::ClientCaps` only when its payload differs from the
  last forwarded one (`SessionBridge::sb2_forwarded`; the socket is
  reliable, so dedupe loses nothing), and a re-home clears that memory.
- **v2 is gated on pacing, not just on the advertisement.** The bridge
  forwards what the viewport advertised; the daemon opens a cursor only
  for a paced, framed client. `POSH_PACED=0` therefore remains a complete
  rollback to Stage 1 delivery (v1 history), and a non-paced client's bytes
  do not change. The cursor lives inside `Pacing`, so "not paced" implies
  "no v2" by construction and no `ClientConn` literal changes.
- **One body per opportunity; screen and history keep separate clocks.**
  The visible opportunity is Stage 2's, unchanged (`paced_send_at`). History
  has its own (`history_send_at`): `write_buf` empty, and either fresh rows
  with room in the window → due now (**corrected 2026-10-06 after Part B's
  measurement:** the expansion first gated fresh bodies on
  `PACED_FRAME_FLOOR_MS` too, which capped history at 256 rows per 20 ms ≈
  12,800 rows/s and lost 20% of a 40,000 rows/s flood to eviction even with
  prompt acks — where v1 lost nothing; the operator chose window + socket
  backpressure as the only limiter), or rows in flight with no room / no
  fresh rows → `last history send + history_resend_after()`. When both are
  due in one pass, the kind that did
  not go last goes (`server_loop`'s coin); the other goes next iteration,
  after the drain. So the screen's cadence is Stage 2's, and the backlog is
  at most ONE body — a visible frame or a ≤ 256-row history body. The v1
  scrollback frame no longer rides behind a v2 viewport's visible frame.
- **History is ack-clocked by a fixed window: `HISTORY_WINDOW_ROWS = 2 ×
  SB2_ROWS_PER_BODY` (512 rows) in flight — and by nothing else.** The
  daemon has no RTT and the bridge drains its socket at once, so without a
  window history would flow as fast as the socket accepts bodies whatever
  the link — the swamping decision 3 forbids. With it, a body goes out
  whenever `write_buf` is empty and fewer than 512 rows are unacked:
  throughput ≈ 512 rows per RTT (on a 1 ms link that is far above any
  flood; on a 300 ms link ≈ 1,700 rows/s). `server_loop` gets the same
  effect from its SRTT/2 send interval (about two bodies per RTT). The
  window is the static stand-in for Task 3.4's dynamic share.
- **The resend floor is the measured ack latency, with a conservative
  start and backoff:** `history_resend_after() = base << min(resends, 3)`,
  `base = max(PACED_ACK_WAIT_MS, 2 × ack srtt)` once Task 3.0 has a
  sample, else `HISTORY_RESEND_INITIAL_MS = 4 × PACED_ACK_WAIT_MS` (1 s,
  TCP's initial RTO); `resends` counts resend bodies without ack progress
  and resets when the ack advances or the epoch bumps. Using
  `PACED_ACK_WAIT_MS` alone would resend spuriously at any RTT above
  250 ms; the backoff turns a never-acking viewport's re-send into at most
  one window per 1, 2, 4, 8, 8, … s.
- **Epoch bumps per client on its own size change AND on a session width
  change.** RFC 0009 §1.1 requires a bump when "the client's reported
  terminal size changes", and the viewport clears its ring on any own
  resize (height too). Without the per-client bump the next body would be a
  forward jump past rows the viewport itself discarded, which Stage 4 would
  draw as "not received". The session width change (RFC 0002 §4 reflow) is
  the existing trigger; both live in one function beside
  `reset_scrollback_floors_on_reflow`.
- **History comes from the session terminal and pauses while the overlay
  is up**, as `server_loop` (`:2050`); it does NOT pause on the alternate
  screen (`server_loop` `:2042-2045`).

### Task 3.0: Ack-latency observability (no behaviour change)

> **Superseded in part, 2026-10-06, while executing:** the sample rule in
> this task's Steps 2–4 (time the ack that confirms the NEWEST frame from
> `last_fresh`) never fires under continuous output at RTT >
> `PACED_ACK_WAIT_MS`. The rule as built is the per-frame send log in
> "Design choices" above (`Pacing.sent_frames`, each visible frame sampled
> at most once); `Pacing` is therefore no longer `Copy`. The test
> `an_ack_below_the_newest_frame_stamps_arrival_but_is_no_sample` became
> `an_ack_of_an_older_frame_samples_that_frames_own_round_trip`, a
> scrollback-slot test was added, and the flood test uses the 2 MiB flood
> (256 KiB is shorter than one 300 ms RTT, so no ack could land).

**Promotion criteria:** N/A — diagnostic only.

**Files:**
- Modify: `crates/posh/src/session/daemon.rs` — constants beside
  `PACED_ACK_WAIT_MS` (`:73`); `AckLatency` after `Pacing` (`:325`), the
  `acks` field on `Pacing`; `ClientConn::note_frame_ack` after
  `apply_frame_ack` (`:843`); `backlog_log_fields` (`:961-970`);
  `handle_frame_ack` (`:1069-1073`) and its callers (`:2202-2206`, `:4486`,
  `:4492`, `:5026`, `:5135`, `:6797`); the loop-top breadcrumb
  (`:1745-1755`); the disconnect line (`:2316-2322`); the flood harness's
  cadence acks (`:5823`, `:5866`); tests after the Stage 2 block
  (`:7198`).
- Modify: `docs/features/0021-flood-delivery.md` — a *Diagnostics*
  paragraph at the end of Interface.

**Step 1: Record the witness BEFORE touching code.** Run
`just debug-cargo test --release -p posh --bin posh posh225_flood_backlog_ideal_reader_measurement -- --ignored --nocapture`
and keep the table (scratchpad). Step 8 re-runs it; every row must be
identical — this task changes no stream.

**Step 2: Failing tests** (new block `// ---- posh#225 Stage 3: ack latency
----` after `non_paced_stream_is_identical_beside_a_paced_client`):

- `a_paced_clients_ack_of_its_newest_frame_is_a_latency_sample` —
  `paced_conn(24, 80, &[])`, `broadcast_output` a line, `send_paced_frames`
  at `now = 100`; `handle_frame_ack(&mut c, &encode_frame_ack(last_visible,
  0), &term, 340)`: `acks.last_ms == Some(240)`, `srtt_ms == Some(240)`,
  `min_ms == Some(240)`, `max_ms == 240`, `samples == 1`, `last_ack_at ==
  Some(340)`.
- `ack_latency_smooths_and_keeps_min_and_max` — samples 240 then 40:
  `srtt_ms == Some((7 * 240 + 40) / 8)`, `min_ms == Some(40)`, `max_ms ==
  240`, `samples == 2`.
- `an_ack_below_the_newest_frame_stamps_arrival_but_is_no_sample` — two
  paced frames sent unacked (at 100, then at `100 + PACED_ACK_WAIT_MS`
  after re-dirtying and clearing `write_buf`); ack the FIRST at 400:
  `last_ack_at == Some(400)`, `samples == 0`.
- `a_repeated_or_resync_ack_is_no_sample` — after one sample, the same ack
  again → `samples` unchanged, `last_ack_at` moves; a
  `FRAME_ACK_RESYNC` ack of a new frame → no sample.
- `an_unpaced_client_records_no_ack_latency` — `lossy_conn`;
  `handle_frame_ack` → `pacing == None` still.
- `backlog_log_fields_carry_the_ack_latency` — a paced client with no
  sample ends `… paced=1 ack_ms=none ack_n=0 ack_age_ms=none`; after the
  240 ms sample at 340, at `now = 400`: `ack_ms=240/240/240/240 ack_n=1
  ack_age_ms=60`; a `lossy_conn` ends `paced=0 ack_ms=none ack_n=0
  ack_age_ms=none`. The Stage 2 test
  `backlog_log_fields_say_whether_the_client_is_paced` still passes
  (it checks the leading fields).
- `the_ack_latency_line_is_due_once_per_interval_with_new_samples` —
  `ack_latency_log_line(&mut c, now)`: `None` with no samples; `Some` at the
  first sample (starts `paced ack latency fd=` and ends `new=1`); `None`
  again at `now + 1` with no new sample, `None` with a new sample before
  `ACK_LOG_INTERVAL_MS` has passed, `Some(.. new=1)` once it has.
- `posh225_paced_flood_measures_the_round_trip_as_ack_latency` — 256 KiB,
  `paced_flood_case(FloodAcks::Lagged(300), 0)`; `measure_flood` returns the
  client's `AckLatency` in a new `FloodRun::ack_latency` field: `min_ms >=
  Some(300)` and `srtt_ms` in `300..=310` (expected ≈ 301: the RTT plus the
  one pace step before the frame reaches the reader). If it is outside,
  stop and report — the harness's ack timing, not the threshold, is wrong.

Run `just debug-cargo test -p posh --bin posh ack_latency` — expected:
compile errors (`AckLatency`, the `handle_frame_ack` arity).

**Step 3: Implement the state.**

```rust
/// How often a paced viewport's `paced ack latency` log line may repeat
/// (posh#225 Stage 3.0): the field series Task 3.4 is tuned from.
const ACK_LOG_INTERVAL_MS: u64 = 10_000;
```

```rust
/// A paced viewport's frame-ack timing (posh#225 Stage 3.0): the round trip
/// of its newest fresh visible frame — queued, through the bridge and the
/// link, applied, acked — which is the only RTT the daemon can see. Read by
/// the history resend floor (Task 3.3) and the trickle (Task 3.4).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct AckLatency {
    /// When the newest `Tag::FrameAck` arrived, advancing or not.
    last_ack_at: Option<u64>,
    last_ms: Option<u64>,
    /// Smoothed like TCP's SRTT: the first sample, then 7/8 old + 1/8 new.
    srtt_ms: Option<u64>,
    min_ms: Option<u64>,
    max_ms: u64,
    samples: u64,
    /// `samples` at the last `paced ack latency` line, and when it was written.
    logged_samples: u64,
    logged_at: Option<u64>,
}

impl AckLatency {
    fn record(&mut self, ms: u64) {
        self.last_ms = Some(ms);
        self.srtt_ms = Some(self.srtt_ms.map_or(ms, |s| (7 * s + ms) / 8));
        self.min_ms = Some(self.min_ms.map_or(ms, |m| m.min(ms)));
        self.max_ms = self.max_ms.max(ms);
        self.samples += 1;
    }

    /// `ack_ms=<last>/<srtt>/<min>/<max> ack_n=<samples> ack_age_ms=<ms>`,
    /// `none` where there is nothing yet.
    fn log_fields(&self, now: u64) -> String { /* … */ }
}
```

`Pacing` gains `acks: AckLatency` (its `Default` keeps
`paced_cap_on_init_makes_a_paced_client_and_a_bare_reinit_keeps_it`'s
`Some(Pacing::default())` true).

**Step 4: Implement the sampling.** `impl ClientConn`:

```rust
    /// Record a frame ack's arrival for a paced client (posh#225 Stage 3.0):
    /// `acked_before` is the producer's `acked_num()` before the ack was
    /// applied. A sample when this ack is the one that confirmed the newest
    /// paced visible frame, measured from when that frame was queued.
    fn note_frame_ack(&mut self, acked_before: Option<u64>, now: u64) {
        let Some(producer) = self.producer.as_ref() else { return };
        let (acked, newest) = (producer.acked_num(), producer.last_visible_num());
        let Some(p) = self.pacing.as_mut() else { return };
        p.acks.last_ack_at = Some(now);
        if let (Some(before), Some(sent)) = (acked_before, p.last_fresh) {
            if before < newest && acked >= newest {
                p.acks.record(now.saturating_sub(sent));
            }
        }
    }
```

```rust
fn handle_frame_ack(c: &mut ClientConn, payload: &[u8], src: &Terminal, now: u64) {
    let acked_before = c.producer.as_ref().map(FrameProducer::acked_num);
    let resync = c.apply_frame_ack(payload);
    c.note_frame_ack(acked_before, now);
    if resync {
        c.request_frame_from(src);
    }
}
```

(A RESYNC clears `last_fresh` inside `apply_frame_ack`, so it is never a
sample.) Update the loop arm to pass `util::now_ms()` and the five test
callers to pass `0` (or the test's clock). In `measure_flood`, the two
cadence acks (`:5823`, `:5866`) become `handle_frame_ack(&mut c, &…,
&term, now)`; the attach ack (`:5786`) and the final ack (`:5888`) stay
`apply_frame_ack`, so the harness's artificial t = 0 keyframe is not a
sample. For an unpaced run `handle_frame_ack` is `apply_frame_ack` plus a
no-op, so the stream is unchanged. `FloodRun` gains `ack_latency:
Option<AckLatency>` (the client's `pacing.acks` at the end of the run;
`None` unpaced) for the flood test above.

**Step 5: Implement the surfaces.**
- `backlog_log_fields` appends `" {}", c.pacing.map_or_else(||
  AckLatency::default().log_fields(now), |p| p.acks.log_fields(now))` after
  `paced=`.
- `fn ack_latency_log_line(c: &mut ClientConn, now: u64) -> Option<String>`
  — due when `samples > logged_samples` and `logged_at` is `None` or at
  least `ACK_LOG_INTERVAL_MS` old; returns `format!("paced ack latency {}
  new={}", backlog_log_fields(c, now), samples - logged_samples)` and
  records `logged_samples`/`logged_at`.
- Loop top, in the breadcrumb loop (`:1746`): `if let Some(line) =
  ack_latency_log_line(c, now) { util::log_write("info", &line); }`.
- Disconnect line (`:2317-2322`): compute `backlog_log_fields(&clients[i],
  util::now_ms())` before `clients.remove(i)` and log `client disconnected
  {fields} remaining={}` (the prefix and `fd=` are unchanged grep targets).

**Step 6: Run** `just debug-cargo test -p posh --bin posh ack_latency` and
`just debug-cargo test -p posh --bin posh backlog_log_fields` — expected
PASS. Then `just debug-cargo test -p posh --bin posh session::daemon` —
expected PASS.

**Step 7: FDR 0021 Diagnostics.** One paragraph at the end of Interface:
the session daemon's log (`<base>/…/<session>.log`) carries, per paced
viewport, `paced ack latency … ack_ms=<last>/<srtt>/<min>/<max> ack_n=
ack_age_ms= new=` at most every 10 s while frames are acked, and the same
fields on `client disconnected`; `ack_ms` is the round trip of the newest
paced screen frame (daemon → bridge → link → viewport → back). Status
unchanged (`experimental`).

**Step 8: Lint and witness.** `just lint-fmt`;
`just debug-cargo clippy -p posh --all-targets -- -D warnings` — clean.
Re-run Step 1's measurement — expected: identical rows.

**Step 9: Commit** — message:
`daemon: record and log each paced viewport's ack latency (posh#225 Stage 3.0)`
with a body naming the sample rule, the three log surfaces, and "no stream
changes (measurement table identical)".

**Field step (operator, after 3.0 merges and deploys):** run a `nix gc` in
a remote mux-attached session, then read the remote session log for
`paced ack latency` lines (idle before, during, after). These numbers gate
Task 3.4.

### Task 3.1: Extract `server_loop`'s v2 cursor into `HistoryCursor` (pure refactor)

**Promotion criteria:** N/A — no behaviour change.

**Files:**
- Create: `crates/posh/src/remote/history.rs`.
- Modify: `crates/posh/src/remote/mod.rs` — `pub mod history;` beside
  `pub mod framesync` (`:40`).
- Modify: `crates/posh/src/remote/server.rs` — remove `SB2_ROWS_PER_BODY`
  (`:52-55`); replace the locals (`:1328-1341`), the entry/size blocks
  (`:1892-1922`), `now_wants` (`:1923-1924`), `want_sb2` (`:2039-2054`),
  the body (`:2215-2249`), the ack entry (`:2345-2350`).
- Modify: `crates/posh/src/session/daemon.rs:905` — the stale "mirror of
  server.rs:761-770" becomes "mirror of `server_loop`'s v1 scrollback body
  (`remote/server.rs`) — keep in sync" (no line numbers).

**Step 1: Witness.** Run
`just debug-cargo test -p posh --bin posh wedge_repro_server_loop_with_loss_and_titles -- --nocapture`
and `just debug-cargo test -p posh --bin posh scrollback2` — expected PASS;
keep the printed `sb2_rows=` line.

**Step 2: Failing tests** (`history.rs` `mod tests`; a 5x20 terminal with a
50-row ring, `scroll(term, n)` feeding `n` distinct `"{i:04}\r\n"` lines
after filling the screen):

- `a_cursor_is_inert_until_activated` — `HistoryCursor::new((5, 20))`:
  `epoch() == None`, `wants(..) == false`; `on_client_size((6, 20), total)`
  records the size without bumping (after `activate`, `epoch() ==
  Some(1)`).
- `a_fresh_cursor_numbers_rows_from_its_opening_total` — activate `Fresh`
  at total `t0`, scroll 10: `avail == 10`; `next_body(.., cap 4)` →
  `Scrollback2 { epoch: 1, row_offset: 0, rows }` whose rows equal
  `term.dump_scrollback_row(ring_len - 10 + k)`; the next body starts at 4.
- `a_body_carries_at_most_the_cap` — scroll 600 (ring 50): bodies of
  `cap` rows until caught up.
- `an_ack_moves_only_forward_and_only_in_its_epoch` — `on_ack(1, 5)` →
  acked 5; `on_ack(1, 3)` → 5; `on_ack(2, 9)` → 5 (wrong epoch).
- `a_resend_starts_at_the_ack_and_waits_for_the_rto` — send 0..8 at
  `now = 0`, `on_ack(1, 3)`: `resend_due(10, 100) == false` and `wants(..,
  10, 100) == false` (caught up); at `now = 100`: `resend_due` true and
  `next_body` starts at 3.
- `evicted_rows_become_one_forward_jump` — send 0..10, scroll 100 more
  (ring 50): `next_body` starts at `avail - 50` (> 10: a jump of
  `avail - 50 - 10`), and bodies after it are contiguous.
- `a_size_change_bumps_the_epoch_and_reanchors` — active, scroll 10, send:
  `on_client_size((6, 20), total)` → `epoch() == Some(2)`, `avail == 0`,
  `acked_rows() == 0`, `sent_upto() == 0`; the same size again → no bump;
  epoch 255 bumps to 0 (`wrapping_add`, as today).
- `bump_epoch_reanchors_unconditionally_when_active`.
- `a_continued_cursor_resumes_the_viewports_count` — `activate(Continue {
  epoch: 7, rows: 40 }, total)`, scroll 3: `epoch() == Some(7)`, `avail ==
  43`, `acked_rows() == 40`; `next_body` → `row_offset 40`, the 3 newest
  ring rows. (Used by Task 3.3; `server_loop` never continues.)
- `a_continued_cursor_never_offers_rows_from_before_its_anchor` — continue
  at `rows: 40` over a full ring, scroll 5, send them, let the RTO pass
  with no ack: the resend body starts at 40, never below it, though the
  ring holds older rows (the floor is `max(anchor_rel, avail - ring_len)`).

Run `just debug-cargo test -p posh --bin posh remote::history` — expected:
compile error (module missing).

**Step 3: Implement** `history.rs`:

```rust
//! RFC 0009 v2 history: the send cursor of the addressed, cumulatively
//! acknowledged scrollback stream. Shared by the single-peer `server_loop`
//! and the session daemon's paced viewports (posh#225 Stage 3).

use posh_term::Terminal;

use crate::remote::caps::Scrollback2Client;
use crate::remote::sync::FrameBody;

/// Max rows per v2 body (RFC 0009 §2): chunks a long-disconnect resend into
/// fragmentation-friendly frames; the cumulative repeat loop carries the
/// rest forward as acks advance.
pub(crate) const SB2_ROWS_PER_BODY: u64 = 256;

/// Where a cursor's row space starts (decision 10: a per-viewport input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryStart {
    /// The cursor's own epoch; row 0 is the next row scrolled.
    Fresh,
    /// The viewport's epoch, continued: row `rows` is the next row scrolled.
    /// Forward-only — rows scrolled while it was elsewhere are not this
    /// cursor's (Stage 5 passes an earlier position from the resume cursor).
    Continue { epoch: u8, rows: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistoryCursor {
    active: bool,
    epoch: u8,
    /// The monotonic primary-scrollback total at which relative row
    /// `anchor_rel` begins (relative row r is absolute row
    /// `anchor_abs + r - anchor_rel`).
    anchor_abs: u64,
    anchor_rel: u64,
    acked_rows: u64,
    sent_upto: u64,
    last_send: u64,
    size: (u16, u16),
}

impl HistoryCursor {
    pub(crate) fn new(size: (u16, u16)) -> Self;          // inactive, epoch 1
    pub(crate) fn activate(&mut self, start: HistoryStart, total: u64); // no-op when active
    /// `server_loop`'s per-message entry: open on first sight, then ack.
    pub(crate) fn on_client_entry(&mut self, entry: &Scrollback2Client, total: u64);
    /// Cumulative ack in `epoch`; returns whether it advanced.
    pub(crate) fn on_ack(&mut self, epoch: u8, acked_rows: u64) -> bool;
    /// Records `size`; bumps when active and it changed (RFC 0009 §1.1).
    pub(crate) fn on_client_size(&mut self, size: (u16, u16), total: u64);
    pub(crate) fn bump_epoch(&mut self, total: u64);       // no-op when inactive
    pub(crate) fn epoch(&self) -> Option<u8>;               // None when inactive
    pub(crate) fn avail(&self, total: u64) -> u64 {
        self.anchor_rel + total.saturating_sub(self.anchor_abs)
    }
    pub(crate) fn resend_due(&self, now: u64, rto: u64) -> bool {
        self.acked_rows < self.sent_upto && now.saturating_sub(self.last_send) >= rto
    }
    pub(crate) fn wants(&self, total: u64, now: u64, rto: u64) -> bool {
        self.active && (self.avail(total) > self.sent_upto || self.resend_due(now, rto))
    }
    pub(crate) fn in_flight(&self) -> u64 { self.sent_upto.saturating_sub(self.acked_rows) }
    pub(crate) fn acked_rows(&self) -> u64;
    pub(crate) fn sent_upto(&self) -> u64;
    pub(crate) fn last_send(&self) -> u64;
    /// The next body, exactly `server_loop`'s: from the ack when a resend is
    /// due, else from the send cursor; never below the eviction floor (a
    /// forward jump) nor below `anchor_rel`; at most `cap` rows.
    pub(crate) fn next_body(&mut self, term: &Terminal, now: u64, rto: u64, cap: u64) -> FrameBody {
        let total = term.primary_scrollback_total();
        let ring_len = term.primary_scrollback_len() as u64;
        let avail = self.avail(total);
        let floor_rel = self.anchor_rel.max(avail.saturating_sub(ring_len));
        let cursor = if self.resend_due(now, rto) { self.acked_rows } else { self.sent_upto };
        let start = cursor.max(floor_rel);
        let count = (avail - start).min(cap) as usize;
        let rows = (0..count)
            .map(|k| {
                let r = start + k as u64;
                term.dump_scrollback_row((ring_len - (avail - r)) as usize).unwrap_or_default()
            })
            .collect();
        self.sent_upto = start + count as u64;
        self.last_send = now;
        FrameBody::Scrollback2 { epoch: self.epoch, row_offset: start, rows }
    }
}
```

With `anchor_rel = 0`, `floor_rel` is `server_loop`'s `avail - min(ring_len,
avail)` exactly. `Fresh` sets `anchor_abs = total`, `anchor_rel = acked =
sent = 0` and keeps `epoch`; `Continue` sets `epoch`, `anchor_rel = acked =
sent = rows`. `bump_epoch` is `server_loop`'s re-anchor (`wrapping_add(1)`,
`anchor_abs = total`, everything else 0).

Run `just debug-cargo test -p posh --bin posh remote::history` — expected
PASS.

**Step 4: Switch `server_loop`.**
- `:1328-1341` → `let mut sb2 = HistoryCursor::new((rows, cols));` (one
  comment carrying the old block's explanation).
- `:1892-1909` → `if let Some(c) = caps::find(..).and_then(|cap|
  caps::decode_scrollback2_client(&cap.payload).ok()) {
  sb2.on_client_entry(&c, term.primary_scrollback_total()); }`.
- `:1910-1922` → `sb2.on_client_size(client_size, term.primary_scrollback_total());`.
- `:1923-1924` → `let now_wants = sb2.epoch().is_none() && …`.
- `:2046-2054` → `let want_sb2 = overlay.is_none() && !force_frame &&
  !shutdown && paced && sb2.wants(cur_sb_total, now, conn.rto());` (the
  `sb2_avail`/`sb2_resend_due` locals go).
- `:2215-2249` → `sb2.next_body(&term, now, conn.rto(), SB2_ROWS_PER_BODY)`
  (the comment stays).
- `:2348-2350` → `if let Some(epoch) = sb2.epoch() {
  extras.push(caps::encode_scrollback2_ack(epoch)); }`.
- `use crate::remote::history::{HistoryCursor, SB2_ROWS_PER_BODY};`.

**Step 5: Run** Step 1's two commands — expected PASS, the
`wedge_repro` line unchanged in shape (its numbers vary run to run with
induced loss). Then `just debug-cargo test -p posh --bin posh remote::` —
expected PASS; `just debug-cargo clippy -p posh --all-targets -- -D
warnings` — clean.

**Step 6: Commit** — message:
`remote: extract server_loop's v2 history cursor into HistoryCursor (posh#225 Stage 3; no behaviour change)`

### Task 3.2: The M2 bridge carries the viewport's `SCROLLBACK2` entry and its ack

**Promotion criteria:** N/A — until Task 3.3 the daemon ignores the entry
(`absorb_client_caps` and `apply_init` do not read id 10), so no stream
changes.

**Files:**
- Modify: `crates/posh/src/remote/server.rs` — `SessionBridge`
  (`:309-350`) gains `sb2_forwarded: Option<Vec<u8>>`; its literals
  (`:1037-1055`, `test_bridge`'s at `:2856-2862`) set `None`;
  `bridge_init_content` (`:1158-1168`); `bridge_client_message`
  (`:1186-1203`); `rehome_bridge` (`:1143-1156`); tests after
  `rehome_bridge_keeps_paced_in_the_reinit` (`:3144`).
- Do **not** modify `crates/posh/src/remote/relay.rs` (ADR 0007).

**Step 1: Failing tests** (`server.rs` `mod tests`; a helper
`fn sb2(epoch: u8, rows: u64) -> caps::Cap` = `encode_scrollback2_client`
with `ring_depth: 0`, and `fn client_caps_records(b: &SessionBridge) ->
Vec<Vec<caps::Cap>>` decoding every `Tag::ClientCaps` in
`b.daemon.link.write`):

- `bridge_init_carries_the_viewports_scrollback2_entry` —
  `bridge_init_content(&[sb2(0, 0)])` contains it verbatim; without it, no
  id 10.
- `the_relay_never_carries_scrollback2` — `relay::content_caps(&[sb2(1,
  5)])` and `relay::forwarded_client_caps(&[sb2(1, 5)])` are empty.
- `the_bridge_forwards_a_changed_scrollback2_ack_once` — `test_bridge()`;
  a `ClientMessage` (rows/cols = `b.client_size`, no input) with
  `caps: vec![sb2(1, 5)]` → one `ClientCaps` record holding exactly
  `sb2(1, 5)`; the same message again → no new record; `sb2(1, 9)` → a
  record with it.
- `the_bridge_keeps_its_init_content_at_the_viewports_latest_entry` —
  after messages with `sb2(1, 5)` then `sb2(1, 9)`, `caps::find(&b.content,
  CAP_SCROLLBACK2) == Some(&sb2(1, 9))`.
- `rehome_bridge_reinits_with_the_latest_scrollback2_entry_and_forwards_afresh`
  — as `rehome_bridge_keeps_paced_in_the_reinit` (`:3144`), after a message
  with `sb2(3, 77)`: the re-Init's table decodes id 10 as `{epoch 3,
  acked_rows 77}`; the next message carrying the same `sb2(3, 77)` is
  forwarded (the dedupe memory was cleared).
- `a_message_without_scrollback2_forwards_nothing_for_it` — the
  resize-suppressed message (no entry) adds no `ClientCaps` record and
  leaves `content`'s entry alone.

Run `just debug-cargo test -p posh --bin posh scrollback2` — expected:
compile errors / assertion failures.

**Step 2: Implement.**

```rust
fn bridge_init_content(client_caps: &[caps::Cap]) -> Vec<caps::Cap> {
    let mut content = crate::remote::relay::content_caps(client_caps);
    content.extend(caps::find(client_caps, caps::CAP_PACED).cloned());
    // posh#225 Stage 3 (RFC 0009 on the session socket): the viewport's v2
    // entry — its epoch and count, from which the daemon opens the cursor.
    content.extend(caps::find(client_caps, caps::CAP_SCROLLBACK2).cloned());
    content
}
```

In `bridge_client_message`, after the push-cmd extension:

```rust
    // posh#225 Stage 3: the viewport's SCROLLBACK2 entry is its cumulative
    // v2 ack (RFC 0009 §3); forward it when it changed (the socket is
    // reliable), and keep `content`'s copy current so a re-home re-Inits
    // the new daemon at the viewport's position, not its first message's.
    if let Some(entry) = caps::find(&msg.caps, caps::CAP_SCROLLBACK2) {
        if b.sb2_forwarded.as_deref() != Some(&entry.payload[..]) {
            b.sb2_forwarded = Some(entry.payload.clone());
            forwarded.push(entry.clone());
        }
        match b.content.iter_mut().find(|c| c.id == caps::CAP_SCROLLBACK2) {
            Some(held) => *held = entry.clone(),
            None => b.content.push(entry.clone()),
        }
    }
```

`rehome_bridge` sets `b.sb2_forwarded = None` after the new leg links. The
doc comment on `bridge_init_content` names the entry beside `CAP_PACED`.

**Step 3: Run** `just debug-cargo test -p posh --bin posh remote::` —
expected PASS (`mux_peer_opens_daemonlink_per_session_channel`'s `cm()`
advertises no id 10). `just debug-cargo clippy -p posh --all-targets -- -D
warnings` — clean.

**Step 4: Commit** — message:
`M2 bridge: carry the viewport's SCROLLBACK2 entry into the daemon Init and forward its ack (posh#225 Stage 3)`

### Task 3.3: The daemon sends addressed (v2) history to paced viewports

Two commits: **Part A** is the behaviour (state, wiring, unit tests, FDR
lines); **Part B** is the flood harness, the regression tests and the
measurement.

**Promotion criteria:** N/A — opt-out on the viewport (`POSH_PACED=0`,
next attach) returns it to v1 history.

#### Part A — the core

**Files:**
- Modify: `crates/posh/src/session/daemon.rs` — constants beside
  `PACED_ACK_WAIT_MS` (`:73`); `Pacing` (`:313-325`) gains `history`,
  `last_was_history`, `history_resends`; `use
  crate::remote::history::{HistoryCursor, HistoryStart, SB2_ROWS_PER_BODY}`;
  `absorb_client_caps` (`:332-357`); new `ClientConn` methods after
  `send_paced_frame` (`:631`); `send_paced_frame` (`:622-631`);
  `queue_frame`'s caps (`:688-706`, `:731`); `send_paced_frames`
  (`:1117-1123`), `paced_poll_timeout` (`:1128-1135`); a new
  `reset_history_on_resize` after `reset_scrollback_floors_on_reflow`
  (`:1475`); `daemon_loop` — the poll (`:1806`), the Init arm
  (`:2107-2109`), the `resized` block (`:2342`), the send pass (`:2406`);
  every test call of `send_paced_frames` / `paced_poll_timeout`; tests in
  a `// ---- posh#225 Stage 3: v2 history ----` block.
- Modify: `docs/features/0021-flood-delivery.md` — Interface and
  Limitations (below).

**Step 1: Record the witness BEFORE touching code** — the Step 1
measurement of Task 3.0 (re-run it at this HEAD). Part B Step 6 compares.

**Step 2: Failing tests.** Helpers first:

```rust
    fn sb2_entry(epoch: u8, acked_rows: u64) -> caps::Cap {
        caps::encode_scrollback2_client(&caps::Scrollback2Client { ring_depth: 0, epoch, acked_rows })
    }

    /// A paced client whose Init carried SCROLLBACK + `entry`, opened the
    /// way the daemon's Init arm opens it.
    fn paced_v2_conn(term: &Terminal, entry: caps::Cap) -> (ClientConn, UnixStream) {
        let (mut c, peer) = paced_conn(term.rows(), term.cols(), &[SCROLLBACK_CAP[0].clone(), entry]);
        c.sb_floor = term.primary_scrollback_total();
        c.open_history(term);
        (c, peer)
    }

    /// What the viewport's next message would forward: its cumulative ack.
    fn ack_history(c: &mut ClientConn, epoch: u8, rows: u64) {
        c.absorb_client_caps(&[sb2_entry(epoch, rows)], 0, false);
    }

    fn history_bodies(frames: &[ServerFrame]) -> Vec<(u8, u64, usize)> // (epoch, row_offset, rows)
```

Tests (one line each on what they assert):

- `a_paced_scrollback2_init_opens_a_history_cursor_and_nothing_else_does`
  — `paced_v2_conn(.., sb2_entry(0, 0))` → `history` is `Some` with epoch
  1; `paced_conn` without id 10 → `None`; `lossy_conn` with id 10 →
  `pacing == None`; a 9-byte id-10 payload → `None`.
- `a_viewport_holding_an_epoch_continues_it_at_its_count` —
  `sb2_entry(7, 40)`: scroll 3, the history body is `(7, 40, 3)`.
- `a_bare_reinit_keeps_the_history_cursor` — ack 5 rows, then
  `apply_init(&encode_resize(..))`: the cursor is unchanged.
- `every_frame_to_a_v2_viewport_carries_the_scrollback2_ack` — the paced
  visible frame's caps decode id 10 as `{0x02, 1}`; a paced non-v2 client's
  frame carries no id 10; a `lossy_conn` fed the same output is
  byte-identical to a twin built before this change (compare two
  `lossy_conn`s, one given an `open_history` call — it must be a no-op).
- `a_v2_viewport_gets_scrollback2_bodies_and_never_v1` — attach frame acked,
  scroll 10, pass at the visible opportunity: ONE visible frame, no
  `FrameBody::Scrollback` behind it; pass at the history opportunity:
  `Scrollback2 { epoch: 1, row_offset: 0 }` with the 10 rows equal to
  `term.dump_scrollback_row(..)`, its `frame_num` equal to
  `last_visible_num()`, the producer's `current_num()` unmoved.
- `screen_and_history_take_turns_when_both_are_due` — with both due at one
  `now`: after a visible send the pass sends history; with both due again
  (buffer cleared, both clocks past), it sends the screen.
- `a_history_body_carries_at_most_sb2_rows_per_body` — scroll 600 with
  prompt acks: bodies of 256, 256, 88.
- `history_waits_for_room_in_the_window` — no acks: two bodies (0..256,
  256..512) at successive floors, then `history_send_at() == last send +
  HISTORY_RESEND_INITIAL_MS` though 88 fresh rows wait; `ack_history(1,
  256)` → due at the floor again, next body starts at 512.
- `a_withheld_ack_is_resent_from_the_ack_only_after_the_floor` — after
  0..512 and `ack_history(1, 100)`: nothing at `last + floor`; at `last +
  HISTORY_RESEND_INITIAL_MS` the body starts at 100.
- `resends_back_off_while_acks_stay_withheld` — successive resend deadlines
  are `base`, `2 base`, `4 base`, `8 base`, `8 base` after the last send;
  an advancing `ack_history` resets to `base`.
- `the_resend_floor_follows_the_measured_ack_latency` — `acks.srtt_ms =
  Some(600)` → `history_resend_after() == 1200`; `Some(50)` →
  `PACED_ACK_WAIT_MS`; `None` → `HISTORY_RESEND_INITIAL_MS`.
- `a_stale_epoch_or_backward_ack_is_ignored` — `ack_history(2, 9)` in epoch
  1 and `ack_history(1, 3)` after 5 leave the ack at 5.
- `a_stalled_v2_viewport_gets_one_forward_jump_of_the_evicted_span` — a
  5x20 terminal with a 50-row ring: send one body (0..10) and ack it; scroll
  200 more with no pass; then passes with prompt acks until caught up: the
  bodies' offsets are `0, f, f + n…` with exactly one discontinuity, `f ==
  avail - 50` (the jump is `f - 10` rows, the rows evicted before their
  turn), and the rows delivered after it are the ring's 50, in order.
- `a_viewports_own_resize_bumps_its_epoch_and_a_width_change_bumps_all` —
  two v2 clients; `apply_resize` one to a new height and
  `reset_history_on_resize(&mut clients, &term, cols)`: its epoch is 2, the
  other's 1; then resize the terminal's width and call it with the old
  width: both bump; the next frame to each carries its new epoch.
- `no_history_body_while_the_overlay_is_up` — `send_paced_frames(.., src,
  None, now)` sends no `Scrollback2` though rows are pending, and
  `paced_poll_timeout(.., None, now) == -1` for a clean screen.
- `the_poll_wakes_for_pending_history` — clean screen, pending rows:
  `paced_poll_timeout(.., Some(&term), now)` is the history floor deadline.
- `the_exit_flush_sends_only_the_screen` — `flush_paced_frames` on a dirty
  v2 client with pending rows queues one visible frame and no body.

Run `just debug-cargo test -p posh --bin posh v2` — expected: compile
errors (`open_history`, the new arities).

**Step 3: Implement the state.**

```rust
/// v2 history (posh#225 Stage 3): rows a paced viewport may have in flight
/// before its ack — about two bodies per round trip, the daemon's stand-in
/// for `server_loop`'s SRTT-paced send interval. The static share Task 3.4
/// makes dynamic. A tuning value: change it only with a measurement in FDR 0021.
const HISTORY_WINDOW_ROWS: u64 = 2 * SB2_ROWS_PER_BODY;
/// The v2 resend floor before any ack latency has been measured: TCP's
/// initial RTO. A tuning value, as above.
const HISTORY_RESEND_INITIAL_MS: u64 = 4 * PACED_ACK_WAIT_MS;
/// Resend backoff: the floor doubles per resend without ack progress, at
/// most this many times.
const HISTORY_RESEND_MAX_DOUBLINGS: u32 = 3;
```

`Pacing` gains:

```rust
    /// RFC 0009 v2 history (posh#225 Stage 3): `Some` when this paced
    /// viewport's Init carried a well-formed `CAP_SCROLLBACK2`; it then gets
    /// `Scrollback2` bodies and never v1 `Scrollback` frames.
    history: Option<HistoryCursor>,
    /// Whether the newest paced send was a history body (`server_loop`'s
    /// `last_was_sb`): when both kinds are due, the other one goes.
    last_was_history: bool,
    /// Resend bodies since the v2 ack last advanced (the backoff exponent).
    history_resends: u32,
```

**Step 4: Open, ack, bump.** `impl ClientConn`:

```rust
    /// Open this paced viewport's v2 cursor from its Init's SCROLLBACK2
    /// entry, at `term`'s total (the daemon's Init arm, beside `sb_floor`).
    /// A viewport holding an epoch is continued at its count, so a
    /// reconnect keeps its ring (a fresh epoch would make it clear it); one
    /// holding none (epoch 0) gets a fresh epoch. A bare re-Init keeps the
    /// cursor it has.
    fn open_history(&mut self, term: &Terminal) {
        if self.producer.is_none() {
            return;
        }
        let entry = caps::find(&self.caps, caps::CAP_SCROLLBACK2)
            .and_then(|c| caps::decode_scrollback2_client(&c.payload).ok());
        let (Some(p), Some(entry)) = (self.pacing.as_mut(), entry) else { return };
        if p.history.is_some() {
            return;
        }
        let start = match entry.epoch {
            0 => HistoryStart::Fresh,
            epoch => HistoryStart::Continue { epoch, rows: entry.acked_rows },
        };
        let mut cursor = HistoryCursor::new((self.rows, self.cols));
        cursor.activate(start, term.primary_scrollback_total());
        p.history = Some(cursor);
    }
```

In `absorb_client_caps`, after the push-cmd latch:

```rust
        // posh#225 Stage 3 (RFC 0009 §3): a v2 viewport's cumulative ack,
        // forwarded by the M2 bridge on every change.
        if let Some(entry) = caps::find(table, caps::CAP_SCROLLBACK2)
            .and_then(|c| caps::decode_scrollback2_client(&c.payload).ok())
        {
            if let Some(p) = self.pacing.as_mut() {
                if p.history.as_mut().is_some_and(|h| h.on_ack(entry.epoch, entry.acked_rows)) {
                    p.history_resends = 0;
                }
            }
        }
```

(On an Init it runs inside `apply_init`, before `open_history`, and finds
no cursor: the Init's count is taken by `open_history` itself.)

```rust
/// v2 epochs (RFC 0009 §1.1, posh#225 Stage 3), run beside
/// `reset_scrollback_floors_on_reflow`: a viewport whose own reported size
/// changed cleared its ring, and a session width change reflowed the
/// daemon's, so each re-anchors at the current total in a new epoch.
fn reset_history_on_resize(clients: &mut [ClientConn], term: &Terminal, cols_before: u16) {
    let total = term.primary_scrollback_total();
    let reflowed = term.cols() != cols_before;
    for c in clients.iter_mut() {
        let size = (c.rows, c.cols);
        let Some(p) = c.pacing.as_mut() else { continue };
        let Some(h) = p.history.as_mut() else { continue };
        let before = h.epoch();
        h.on_client_size(size, total);
        if reflowed && h.epoch() == before {
            h.bump_epoch(total);
        }
        if h.epoch() != before {
            p.history_resends = 0;
        }
    }
}
```

**Step 5: The history opportunity and the body.** `impl ClientConn`:

```rust
    fn has_history(&self) -> bool {
        self.pacing.as_ref().is_some_and(|p| p.history.is_some())
    }

    /// The v2 resend floor: twice the measured ack latency (never under the
    /// ack wait), `HISTORY_RESEND_INITIAL_MS` before any sample, doubled per
    /// resend without progress.
    fn history_resend_after(&self) -> u64 {
        let Some(p) = self.pacing.as_ref() else { return HISTORY_RESEND_INITIAL_MS };
        let base = p.acks.srtt_ms.map_or(HISTORY_RESEND_INITIAL_MS, |s| (2 * s).max(PACED_ACK_WAIT_MS));
        base << p.history_resends.min(HISTORY_RESEND_MAX_DOUBLINGS)
    }

    /// When this client may next be sent a history body — the history half
    /// of the one-predicate rule (`send_paced_frames` and
    /// `paced_poll_timeout` both ask it). Fresh rows with room in the
    /// window: the floor after the last body. Otherwise, rows in flight: the
    /// resend deadline. `None` with bytes queued, or with nothing fresh and
    /// nothing in flight.
    fn history_send_at(&self, term: &Terminal) -> Option<u64> {
        let h = self.pacing.as_ref().and_then(|p| p.history)?;
        if !self.write_buf.is_empty() || self.producer.is_none() {
            return None;
        }
        let fresh = h.avail(term.primary_scrollback_total()) > h.sent_upto();
        if fresh && h.in_flight() < HISTORY_WINDOW_ROWS {
            return Some(h.last_send() + PACED_FRAME_FLOOR_MS);
        }
        (h.in_flight() > 0).then(|| h.last_send() + self.history_resend_after())
    }

    /// Queue one v2 body from `term` (the session terminal), riding the
    /// newest visible frame number (RFC 0009 §2: an annotation).
    fn send_history_body(&mut self, term: &Terminal, now: u64) {
        let rto = self.history_resend_after();
        let flags = self.echo_flag | self.overlay_flag;
        let (Some(producer), Some(p)) = (self.producer.as_ref(), self.pacing.as_mut()) else { return };
        let Some(h) = p.history.as_mut() else { return };
        if h.resend_due(now, rto) {
            p.history_resends += 1;
        }
        let body = h.next_body(term, now, rto, SB2_ROWS_PER_BODY);
        let epoch = h.epoch().expect("an open cursor is active");
        p.last_was_history = true;
        let bytes = ServerFrame {
            flags,
            caps: caps::own_table(&[caps::encode_scrollback2_ack(epoch)]),
            frame_num: producer.current_num(),
            input_ack: 0,
            echo_ack: 0,
            body,
        }
        .encode();
        self.queue(Tag::Frame, &bytes);
    }
```

(`HistoryCursor` is `Copy` but `Pacing` is not since 3.0's `VecDeque`, so
`history_send_at` goes through `self.pacing.as_ref()` and the mutating
paths take `as_mut`.) In `send_paced_frame`: call
`maybe_queue_scrollback(src)` only when `!self.has_history()`, and set
`p.last_was_history = false` beside `dirty = false`. In `queue_frame`,
after building `activity_cap`: when `self.pacing.as_ref().and_then(|p|
p.history).and_then(|h| h.epoch())` is `Some(e)`, push
`caps::encode_scrollback2_ack(e)` (a v2 viewport adopts the epoch from the
first frame it gets — RFC 0009 §1.1).

**Step 6: The pass and the poll.**

```rust
fn send_paced_frames(clients: &mut [ClientConn], src: &Terminal, history: Option<&Terminal>, now: u64) {
    for c in clients.iter_mut() {
        let screen = c.paced_send_at().is_some_and(|at| now >= at);
        let rows = history.filter(|t| c.history_send_at(t).is_some_and(|at| now >= at));
        let last_was_history = c.pacing.is_some_and(|p| p.last_was_history);
        match (screen, rows) {
            (true, Some(t)) if !last_was_history => c.send_history_body(t, now),
            (true, _) => c.send_paced_frame(src, now),
            (false, Some(t)) => c.send_history_body(t, now),
            (false, None) => {}
        }
    }
}

fn paced_poll_timeout(clients: &[ClientConn], history: Option<&Terminal>, now: u64) -> i32 {
    clients
        .iter()
        .flat_map(|c| [c.paced_send_at(), history.and_then(|t| c.history_send_at(t))])
        .flatten()
        .map(|at| at.saturating_sub(now))
        .min()
        .map_or(-1, |ms| i32::try_from(ms).unwrap_or(i32::MAX))
}
```

Doc comments say: `history` is the session terminal, `None` while the
escape overlay is up (`server_loop` `:2050`). Update every test call
(`send_paced_frames(.., &term, now)` → `(.., &term, Some(&term), now)`,
except `a_source_swap_marks_a_paced_client_dirty_and_its_next_frame_is_full`
which passes `None`; `paced_poll_timeout(.., now)` → `(.., None, now)`).
`flush_paced_frames` is unchanged (screen only).

**Step 7: Wire `daemon_loop`.**
- Init arm, after `c.sb_floor = …` (`:2107-2109`):
  `c.open_history(term);` (for every Init, not only the first: a bare
  re-Init is a no-op inside).
- `resized` block, after `reset_scrollback_floors_on_reflow` (`:2342`):
  `reset_history_on_resize(clients, term, cols_before);`.
- Poll (`:1806`): `paced_poll_timeout(clients, overlay.is_none().then_some(&*term), now)`.
- Send pass (`:2406`): `send_paced_frames(clients, src,
  overlay.is_none().then_some(&*term), util::now_ms());`.

**Step 8: Run** `just debug-cargo test -p posh --bin posh v2` and
`just debug-cargo test -p posh --bin posh session::daemon` — expected PASS:
every Stage 2 paced test builds clients without id 10, so they keep v1
history behind their frames (`send_paced_frames_carries_scrollback_right_behind_the_visible_frame`
is the regression witness).

**Step 9: FDR 0021 (status stays `experimental`).**
- *Interface*: a paced viewport that advertises `SCROLLBACK2` (every
  current roaming viewport) gets RFC 0009 v2 history from the daemon:
  rows addressed and acknowledged, resent from the ack, one body per send
  opportunity, at most two bodies in flight; a reconnect continues its
  epoch (forward-only, as before).
- *Decisions*: one body per opportunity with the coin; the window; the
  resend floor and backoff; the epoch rules; continue-on-attach.
- *Limitations*: "v1 history re-carry" becomes "a never-acking viewport is
  re-sent at most one window (512 rows) per resend floor, backing off to one
  per 8 × the floor"; "History during a flood needs RTT below ≈ 5 ×
  `PACED_ACK_WAIT_MS`" becomes "history trickles at about one window per
  round trip (≈ 512 rows/RTT), so a flood longer than the ring at that rate
  loses its oldest rows as a forward jump — silent until Stage 4 draws it";
  the slow-reader bullet becomes "a reader slower than the flood loses the
  rows the ring evicts before their turn; each loss is a forward jump of
  exactly that span, and the reader ends holding the whole retained ring".
  Part B fills the numbers.
- *Tuning Levers*: `HISTORY_WINDOW_ROWS`, `HISTORY_RESEND_INITIAL_MS`,
  `HISTORY_RESEND_MAX_DOUBLINGS`, `SB2_ROWS_PER_BODY` (shared with
  `server_loop`); "measurement: Task 3.3 Part B".

**Step 10: Lint.** `just lint-fmt`; `just debug-cargo clippy -p posh
--all-targets -- -D warnings` — clean.

**Step 11: Commit** — message:
`daemon: addressed v2 history for paced viewports (posh#225 Stage 3)`
with a body naming: v2 for paced viewports that advertised SCROLLBACK2, v1
unchanged for everyone else; one body per opportunity (screen/history
coin); a 512-row ack-clocked window; resend from the ack after twice the
measured ack latency, backing off; epoch per own resize and width change;
continue-on-attach.

#### Part B — the flood, addressed: harness, regression tests, measurement

**Files:**
- Modify: `crates/posh/src/session/daemon.rs` — the posh#225 test block:
  `FloodCase` (`:5651-5666`) and every literal; `FloodRun` (`:5603-5647`);
  `measure_flood` (`:5740-5896`); `FloodLedger` (`:5972-6095`);
  `print_flood_header`/`print_flood_row` (`:6097-6125`); the ideal-reader
  measurement (`:6196-6244`); `PACED_FLOOD_CADENCES` (`:6974`) is reused;
  new tests after the Stage 3.0 block.
- Modify: `docs/features/0021-flood-delivery.md` — the measured rows.

**Step 1: Extend the harness** (no new assertions yet).
- `FloodCase` gains `v2: bool` — `true` (paced only) adds `sb2_entry(0, 0)`
  to the client's Init and calls `c.open_history(&term)` right after
  `c.sb_floor = …`. Every existing literal sets `false`;
  `paced_flood_case` sets `false` too (so Stage 2's tests stay v1), and a
  new `paced_v2_flood_case(acks, prefill)` sets `true`.
- A reference: after each chunk, append the rows it scrolled
  (`term.dump_scrollback_row(ring_len - k ..)` for the `k` new ones) to
  `reference: Vec<Vec<u8>>`, indexed by relative row from attach.
- A viewport model, `FloodViewport`, applying RFC 0009 §1.1/§3 exactly as
  `remote/client.rs:3137-3146` and `:3219-3252` do: adopt the epoch from
  any frame's id-10 ack (clearing on change), then for a `Scrollback2`
  body: wrong epoch → `rows_stale`; covered → `rows_repeated += n`;
  partial overlap → `rows_repeated += n`; else, if `row_offset > t` →
  `forward_jumps += 1`, `rows_jumped += row_offset - t`; compare each
  appended row with `reference[row_offset + i]` (`rows_mismatched` on a
  difference); `t = end`. It records `(at, t)` after every application.
- `FloodLedger` keeps the decoded frames: `pending: VecDeque<(end,
  ServerFrame)>`. A gated drain pops the frames it delivered and hands them
  to the viewport model at that step's `now`; `Always` hands them over at
  `account`; `Never` never does. `account` counts a `Scrollback2` as
  history, not visible (`history_bodies`, `largest_history_body`,
  `scrollback_bytes`, `rows_shipped`).
- v2 acks follow the cadence: `EveryNewest(1)` → the viewport's current
  `t`; `Lagged(k)` → its `t` as of `now - k * pace` (the latest `(at, t)` at
  or before it); `Never` → none. Applied through
  `c.absorb_client_caps(&[sb2_entry(epoch, t)], now, false)` — the path
  `Tag::ClientCaps` takes.
- `FloodRun` gains `history_bodies`, `largest_history_body`, `rows_unique`
  (the viewport's final `t`), `rows_repeated`, `forward_jumps`,
  `rows_jumped`, `rows_mismatched`, `rows_stale`; for a v2 run
  `rows_acked` is the cursor's `acked_rows()`.
- The idle tail, for a v2 run, continues until the screen is not owed, the
  buffer is empty, the cursor has no fresh rows and its `in_flight() == 0`
  — except under `Never`, where history cannot progress without an ack
  (the window stays full), so the tail runs a fixed `NEVER_ACK_TAIL_MS =
  20_000` of fake time (long enough for resends at 1, 2, 4 and 8 s) and
  stops. It wakes at the nearest of `paced_poll_timeout(..,
  Some(&term), now)`, the next frame ack, and the next v2 ack
  (`paced_next_history_ack_at`: the first `(at, t)` with `t >
  acked_rows()` and `at + k * pace > now`). The visible idle bound stays;
  the tail as a whole is bounded by `HISTORY_TAIL_BOUND_MS = 120_000`
  (assert, so a non-terminating tail fails instead of hanging).
- `print_flood_row` gains `v2 hbody uniq rep jmp jumped` columns (`-` for
  non-v2 runs); every pre-existing column is printed as before.

Run `just debug-cargo test -p posh --bin posh posh225` — expected PASS:
no existing case sets `v2`.

**Step 2: Regression tests** (write, run, expect PASS — if one fails, the
core is wrong: stop and report rather than loosen it). All at 4 KiB chunks,
`pace: Some(1)`, `OneWritePerChunk` unless named:

- `posh225_v2_flood_ships_every_scrolled_row_exactly_once` — 256 KiB,
  `EveryNewest(1)`, `Lagged(50)`, `Lagged(300)` × prefill `0` and
  `SCROLLBACK + 200`: `rows_shipped == rows_scrolled`, `rows_unique ==
  rows_scrolled`, `rows_repeated == 0`, `forward_jumps == 0`,
  `rows_mismatched == 0`, `rows_acked == rows_scrolled`. (No repeats at
  these RTTs because the first resend floor, 1 s, exceeds them and the
  measured floor is ≥ 2 RTT afterwards.) This is the posh#240 witness for
  v2: no row reaches the viewport twice.
- `posh225_v2_flood_backlog_is_one_body_for_every_cadence` — 256 KiB, every
  cadence in `PACED_FLOOD_CADENCES` plus `Lagged(1500)` × both prefills:
  `crossed_backlog_at == None`; `max_visible_queued <= 1`;
  `peak_write_buf <= largest_visible.max(largest_history_body)` (one body
  at a time); **`peak_write_buf < 64 KiB`** — the bound Task 2.5 deferred;
  `largest_visible < 64 KiB`; `last_screen_delivered == Some(true)`.
- `posh225_v2_flood_at_a_1500_ms_rtt_delivers_its_history` — 256 KiB,
  `Lagged(1500)`, both prefills: `rows_unique == rows_scrolled`,
  `rows_acked == rows_scrolled`, `forward_jumps == 0`, `rows_mismatched ==
  0`, `rows_repeated <= 2 * HISTORY_WINDOW_ROWS` (at most the spurious
  resends before the first latency sample), and `full_frames == 0` (the
  visible base now survives this RTT; expected — if it fails, record the
  measured cliff in the FDR instead of asserting it, and say so in the
  commit). Stage 2 measured 0 rows acked here.
- `posh225_v2_flood_without_acks_resends_one_window_per_backed_off_floor` —
  256 KiB, `Never`, `FloodDrain::Always`: the viewport receives rows
  `0..HISTORY_WINDOW_ROWS` and nothing beyond (`rows_unique ==
  HISTORY_WINDOW_ROWS`: no ack, no room); the history bodies' send times
  after the first window are spaced at `HISTORY_RESEND_INITIAL_MS × 1, 2, 4,
  8, 8 …` (recorded by the harness per body); total history bytes over
  the run are at most `(1 + resend rounds) × HISTORY_WINDOW_ROWS` rows'
  worth — against Stage 2's one ring per ack wait.
- `posh225_v2_slow_reader_loses_only_rows_evicted_before_their_turn` —
  2 MiB, `FloodDrain::Trickle(1 KiB)`, `EveryNewest(1)` and `Lagged(50)`,
  both prefills: `rows_unique + rows_jumped == rows_scrolled` (every row
  is delivered or inside a forward jump — no silent seam), `rows_mismatched
  == 0`, `rows_repeated == 0`, the viewport's final `t == avail`, and the
  last forward jump ends at or before `avail - SCROLLBACK` (every row the
  ring still holds at the end was delivered; the model records each jump's
  end), `max_visible_queued <= 1`, `peak_write_buf <=
  largest_visible.max(largest_history_body)`, `last_screen_delivered ==
  Some(true)`. Print the row; the measured loss goes in the FDR beside
  Stage 2's ~4,300.
- `non_paced_and_v1_paced_streams_are_identical_beside_a_v2_client` —
  extend `non_paced_stream_is_identical_beside_a_paced_client`'s closure
  with a third run that also attaches a `paced_v2_conn` (acked each step
  through both `handle_frame_ack` and `ack_history`): the non-paced
  client's stream equals run A's.

Run `just debug-cargo test --release -p posh --bin posh posh225_v2` and
`just debug-cargo test -p posh --bin posh non_paced_and_v1_paced` —
expected PASS.

**Step 3: Measure.** Add v2 rows to
`posh225_flood_backlog_ideal_reader_measurement`: `pace: Some(1)`, `v2:
true`, cadences `EveryNewest(1)`, `Lagged(50)`, `Lagged(300)`,
`Lagged(1500)`, `Lagged(2500)` (the expected new visible cliff), `Never`,
both rings, 1 KiB and 4 KiB chunks, plus a `Trickle(1 KiB)` row. Run
`just debug-cargo test --release -p posh --bin posh posh225_flood_backlog_ideal_reader_measurement -- --ignored --nocapture`.

**Step 4: Witness.** Every pre-existing column of every non-v2 row of the
table equals Part A Step 1's (no non-v2 stream moved).

**Step 5: FDR 0021.** Tuning Levers: the v2 rows (how to re-run them).
Limitations: the numbers for the RTT rows, the never-acked re-send, the
slow reader; the measured visible cliff (or that `Lagged(2500)` showed
none).

**Step 6: Lint** (`just lint-fmt`, clippy as above) and **commit** —
message:
`posh#225 Stage 3: v2 flood harness, regression tests and measurements`
with the v2 measurement rows in the body and:
`Closes #240` — plus one sentence: "v2 viewports (paced, the default
remote path) get each row exactly once by address; an unpaced lossy
viewport (relayed, or `POSH_PACED=0`) stays on v1 until the old delivery
mode is retired (Stage 7)."

### Task 3.4: The trickle — live first, history by backpressure

> **HELD until the field `nix gc` data (ack latencies from Task 3.0) exists
> — do not start.** The lever and the backpressure rule below are
> PROPOSED. Plan open question 3 (the lever's name, default and signal) is
> the gate; the operator answers it from that data.

**What the field data must show to choose the default:** the `paced ack
latency` series from the session log for (a) an idle session (the link's
floor — `min`), (b) a `nix gc` flood with Stage 3's static window, and (c)
the same flood with history held off (`HISTORY_WINDOW_ROWS` forced to 0 in
a debug build, or an older viewport). If (b)'s `srtt` stays within 2 ×
(a)'s `min`, history is not what delays the screen on that link and the
ceiling only sets catch-up speed — propose the largest ceiling that keeps
(b) there. If (b) inflates past 2 × min while (c) does not, the halving
rule below is what bites, and the default is the ceiling at which (b)'s
srtt settles at ≈ 2 × min. If (c) inflates too, the link is saturated by
screens alone and the trickle can only stay out of the way (budget → 0).

**Promotion criteria:** the lever's default is set by the field data
above; `POSH_HISTORY_ROWS=0` is the opt-out (live-only), read per attach.

**Files (proposed):**
- Modify: `crates/posh-proto/src/caps.rs` — `PACED_VERSION = 2`;
  `encode_paced(history_rows: u16)` writes `[2, rows u16 LE]`;
  `decode_paced_history_rows(payload) -> Option<u16>` (`None` for a v1
  payload — the daemon then uses `SB2_ROWS_PER_BODY`). `decode_paced`
  unchanged (version byte only). Tests at the end of `mod tests` (after
  `paced_entry_tolerates_appended_fields_and_rejects_an_empty_one`).
- Modify: `docs/rfcs/0001-target-grammar-and-capability-table.md` — row 23:
  "v2 appends the history ceiling (u16 LE rows per body; `0` = live only)".
- Modify: `crates/posh/src/session/mod.rs` — `parse_history_rows(Option<&str>)
  -> u16` (unset/unparseable → 256; `0` → 0; clamp to `u16::MAX`) and
  `history_rows_selected()`.
- Modify: `crates/posh/src/remote/client.rs` — `ClientState::history_rows`
  beside `paced`; `outgoing_caps` writes `encode_paced(st.history_rows)`;
  the About gate list names `POSH_HISTORY_ROWS`.
- Modify: `crates/posh/src/session/daemon.rs` — `Pacing.history_ceiling:
  u16` (from Init), `history_budget: u16`; `history_send_at` /
  `send_history_body` / `send_paced_frames` (below).
- Modify: `doc/posh-client.1.scd`, `doc/posh.1.scd` ENVIRONMENT —
  `POSH_HISTORY_ROWS` (the lever's man entry lands with the lever).

**The rule (proposed):**
- **Budget.** Starts at the ceiling. On each ack-latency sample (Task 3.0):
  `sample > 2 × min_ms` → `budget = budget / 2`; else `budget =
  min(2 × budget, ceiling)` (from 0, back to 1). Body rows = `budget`;
  window = `2 × budget`. Ceiling `0` → no history body is ever sent (the
  cursor still opens and the epoch still rides frames, so turning it on
  next attach continues cleanly).
- **Priority.** A screen body always wins an opportunity when the screen
  is owed; a history body takes an opportunity otherwise, and at least
  every `HISTORY_EVERY_NTH = 4`th opportunity while the budget is non-zero
  (replacing 3.3's 1:1 coin), so history cannot starve on a busy screen.

**Steps (to run when un-held):**
1. caps tests: `paced_v2_payload_carries_the_history_ceiling`,
   `a_v1_paced_payload_has_no_ceiling`, `the_ceiling_is_little_endian_u16`;
   run `just debug-cargo test -p posh-proto paced` (fail → implement → pass).
2. Gate tests in `session/mod.rs`: `history_rows_gate_defaults_and_parses`
   (unset → 256, `"0"` → 0, `"1000"` → 1000, `"x"` → 256, `"70000"` →
   `u16::MAX`); viewport test `outgoing_caps_carries_the_history_ceiling`.
   Run `just debug-cargo test -p posh --bin posh history_rows`.
3. Daemon tests: `a_zero_ceiling_sends_no_history_body` (flood, prompt
   acks: `history_bodies == 0`, the epoch ack still rides frames);
   `the_budget_halves_on_an_inflated_ack_and_recovers` (feed samples 100,
   300, 300 → budget 256, 128, 64; then 100, 100 → 128, 256);
   `a_busy_screen_still_gets_every_nth_opportunity_for_history` (dirty at
   every pass: one history body per `HISTORY_EVERY_NTH` sends);
   `the_screen_always_wins_when_owed_below_the_nth` (bodies 1..N−1 are
   screens). Run `just debug-cargo test -p posh --bin posh trickle`.
4. Flood tests: `posh225_trickle_keeps_screen_latency_within_one_wait_while_history_advances`
   — a harness link whose RTT grows with the bytes in flight (a new
   `FloodAcks::Loaded { base_ms, ms_per_kib }`): the time from a screen
   becoming owed to its frame being sent stays ≤ `PACED_ACK_WAIT_MS`
   while `rows_unique` keeps rising; `posh225_trickle_budget_tracks_induced_ack_delay`.
5. Measurement: extend `just debug-mux-load` (justfile `:778`, group
   `debug`) with a loaded-link flood scenario that prints the per-viewport
   `paced ack latency` series and history rows/s; record the command and
   the chosen numbers in FDR 0021's Tuning Levers.
6. Records: man pages (`POSH_HISTORY_ROWS`), RFC 0001 row 23, FDR 0021
   (Interface: the lever; Tuning Levers: budget rule, `HISTORY_EVERY_NTH`,
   the measured default), plan open question 3 → answered.
7. Commit — message:
   `viewport: POSH_HISTORY_ROWS; daemon: history share by ack-latency backpressure (posh#225 Stage 3)`.

### Task 3.5: Records

**Files:**
- Modify: `docs/rfcs/0009-scrollback-stream-separation.md` — a new
  `### 5. The session-socket path` before Security Considerations
  (`:202`), status stays `experimental`:
  - On the session socket (RFC 0008) capabilities are Init-persistent: a
    client advertises `SCROLLBACK2` on its `Tag::Init` (an M2 bridge
    carries its viewport's entry; a relay does not, ADR 0007) and sends
    each changed cumulative ack as a `Tag::ClientCaps` entry; a daemon
    reads the ack from either.
  - The daemon emits v2 only to a client that also advertised `CAP_PACED`
    (RFC 0008 §3.2); others keep RFC 0002.
  - A new attachment's row space continues the epoch and count the Init
    entry names (epoch 0: a fresh epoch at row 0); rows scrolled before
    the attachment are not delivered (Stage 5 amends this with the resume
    cursor).
  - The epoch bumps on the client's own reported size change and on a
    session width change.
  - Covered Requirements gains the Task 3.2 bridge tests and the Task 3.3
    daemon tests.
- Modify: `docs/rfcs/0008-unified-session-frame-transport.md` §3.2
  (`:189-220`): "plus the history (`SCROLLBACK`) body that rides
  immediately behind that frame" becomes: for a client on RFC 0009 v2,
  history bodies are sent at their own opportunities, one body per
  opportunity, at most one unsent body held; for an RFC 0002 client, as
  before.
- Modify: `docs/features/0021-flood-delivery.md` — anything Parts A/B left
  as a placeholder; the Limitations list no longer says "(Stage 3)" for
  the cliff, the re-send and the slow reader.
- Modify: this plan — a "Stage 3 as built" section in the style of
  "Stage 2 as built", superseding this stage's task text where it differs.
- Man pages: none until Task 3.4 lands its lever (`docs/README.md`: status
  and records move with the code; no lever exists yet).

**Step 1:** `just lint-doc` and `just lint-fmt` — clean.

**Step 2: Commit** — message:
`docs: RFC 0009 session-socket path; RFC 0008 §3.2 history bodies; Stage 3 as built (posh#225)`

**Step 3:** merge with `merge-this-session` (its pre-merge hook is the CI
lane; do not run `just` first). Task 3.4 stays held.

**Stage 3 exit check:** with this build on the remote host, in a
mux-attached session (paced, the default): (1) the session log shows
`paced ack latency` lines during a `nix gc` — keep them, they are Task
3.4's input; (2) after the `nix gc`, wheel up: the output is contiguous up
to where history could keep up (≈ 512 rows per round trip) — older than
that is a forward jump, silent until Stage 4; (3) `posh history
<host>:<session> | tail -n 200` and the last 200 rows of the viewport's
scrollback agree; (4) detach and re-attach: the viewport keeps its ring
(continue-on-attach), and only rows scrolled while detached are missing.

---

## Stage 4 — holes the viewport draws

Decisions 4, 8. Viewport-side only, plus one server-reported number.

### Task 4.1: The server reports how much history exists
- A new capability (next free id) on frames to viewports that advertise
  it: `{epoch, avail_rows: u64}` — the epoch-relative row count the server
  holds. **Not** an extension of the `SCROLLBACK2` ack entry: its decoder
  is exact-length (`caps.rs:558-562`) and an old viewport would stop
  adopting epochs. RFC 0001 registry + RFC 0009 amendment.
- Daemon and `server_loop` both emit it (shared via `HistoryCursor`).

### Task 4.2: `ScrollbackRing` records holes
- Files: `remote/sync.rs:551-601`. A forward jump in the v2 append
  (`remote/client.rs:3235-3238`) records a *not received* hole of
  `row_offset − sb2_rows` lines at the current tail instead of appending
  silently. Holes live beside the rows (a side list of `(index, lines)`),
  not as fake rows: a hole is a view decoration (decision 8), and the ring
  stays append-only.
- Eviction from the viewport's own ring drops holes that scroll out.
- Tests: jump recorded with the right count; duplicate/partial-overlap
  branches unchanged; ring eviction removes old holes.

### Task 4.3: The scroll view draws hole rows and the arriving count
- Files: `remote/scrollview.rs:85-144` (the row loop at `:118-130`),
  `posh-proto/src/display.rs:926-935` (`apply_scroll_indicator`).
- One collapsed row per hole at its position: `··· N lines not received ···`.
  The single *arriving* hole sits between the last history row and the
  live screen while `avail_rows > sb2_rows`: `··· N lines arriving ···`,
  and the top bar gains `· N lines still arriving`.
- Anchoring: the view stays on the content being read when the arriving
  hole shrinks or fills — extend the existing `set_scroll(offset + grew)`
  rule and pin it with a test that fills a hole while scrolled up.
- Visual treatment is an **open item** (look-alike transition or other):
  ship a dim, centred row; leave the styling in one function.
- Tests: `compose_scroll_frame` golden frames for (a) a not-received hole
  mid-history, (b) an arriving hole above the live screen, (c) both, (d)
  caught up — no hole, no bar suffix.

### Task 4.4: Records
- FDR 0005: the Interface and Limitations sections gain holes and the
  arriving count; "incomplete view" stops being silent.

**Stage 4 exit check:** wheel up mid-flood on a throttled link: the bar
counts down, the arriving row shrinks, the text being read does not move.

---

## Stage 5 — history across a reconnect, without a stampede

Decision 9.

### Task 5.1: History on the resume cursor
- Files: `remote/resume.rs` (`SessionResume` gains the history position —
  the compiler then points at every construction site and the OPEN codec;
  that is the mechanism working), `remote/mux.rs` (the local mux daemon
  learns the position from the viewport's outgoing `CAP_SCROLLBACK2` entry
  and the epoch/base from relayed frames), `remote/server.rs`
  (`handle_session_instruction` OPEN → the bridge's daemon Init carries
  it), `session/daemon.rs` (a resumed attach seeds its `HistoryCursor`
  instead of anchoring at "now", when the size is unchanged and the base
  is still plausible; otherwise a fresh epoch, which the viewport already
  handles by clearing).
- What must cross the wire is an **absolute** position (the daemon's
  monotonic scrollback total), because the daemon-side epoch state dies
  with the old attach. Spike this first: the smallest field set that lets
  a fresh attach resume the same epoch without the viewport clearing its
  ring. RFC 0015 amendment (versioned resume block, old-peer decode kept).
- Same treatment for the FDR 0012 re-home (`rehome_bridge`): a switch to a
  *different* session is a new row space (fresh epoch — correct); a
  re-home to the same session resumes.
- Tests: reconnect mid-flood — rows produced during the outage arrive, in
  order, once; an outage longer than a ring yields one not-received hole;
  an old client's resume block still decodes; a size change during the
  outage starts a fresh epoch.

### Task 5.2: Stampede guard
- Files: `remote/server.rs` (the bridge's per-sweep handling of daemon
  links, `:644-881`).
- Send-time production gives the lever for free: a daemon produces nothing
  new for a viewport whose socket it cannot write, so a bridge that reads
  a daemon link *later* slows that viewport's catch-up without buffering
  anything. The guard is a per-sweep history budget shared across the
  wire's session channels: screen frames are always read and forwarded;
  history bodies are forwarded round-robin until the budget is spent, and
  the remaining links are left unread until the next sweep.
- Tests (the `debug-mux-load` scenario family): N channels reconnect
  together each owing a full ring — an unrelated channel's screen latency
  stays under a stated bound, and aggregate history bytes per second stay
  under the budget; one channel's catch-up does not starve another's.

### Task 5.3: Records — RFC 0015, RFC 0011 §4.1 (the history budget in the
ordered drain), FDR *Flood delivery*.

**Stage 5 exit check:** suspend the laptop during a ten-minute build,
resume, wheel up: the build output is there.

---

## Stage 6 — the local attach, on the same path

Decision 11. Opt-in first (decision 13).

### Task 6.1: Share the remote client's frame-apply state machine
- The local client's `render_frame` has no base guard and treats
  `ReackAndWait` as fatal — the wedge that kept `POSH_COALESCE` off
  (posh#137's root-cause comment). Extract the remote client's apply
  logic (base guard, graceful re-ack, resync on `base < applied_num`,
  stale/dup discard — `remote/client.rs` `apply_frame`) into a module both
  clients call. Refactor under the remote client's existing tests first;
  then switch the local client to it.
- Tests: the posh#137 reproduction (distinct diffs all claiming one stale
  base) recovers by resync instead of exiting.

### Task 6.2: Local viewport speaks `CAP_PACED` + v2 + holes
- `session/client.rs`: advertise under `POSH_PACED=1` (local default
  **off**), send `Tag::FrameAck`, apply `Scrollback2` through the shared
  code, use the shared `ScrollbackRing` holes and scroll view.
- `CAP_COALESCE`/`POSH_COALESCE` become aliases of the paced path or are
  removed; decide in this task and record it.

### Task 6.3: Soak and flip
- The local default flips to on in its own commit after the operator has
  run it without a wedge; the FDR records the date and the rollback
  (`POSH_PACED=0`).

---

## Stage 7 — man pages, promotion, retirement

Decision 13's documentation requirement, and its explicit final step.

### Task 7.1: Man pages (lands with the stage that introduces each lever,
collected here as the checklist)
- `doc/posh-client.1.scd` ENVIRONMENT: `POSH_PACED`, `POSH_HISTORY_ROWS`.
- `doc/posh.1.scd`: the same variables where the front door documents
  remote attach; a **ROLLOUT** subsection modelled on the existing
  `POSH_MUX_SESSIONS` text (`doc/posh.1.scd:390`, `:573`) stating, per
  path, the current default, how to opt in or out, and how to tell which
  mode a live viewport is in.
- `doc/posh.7.scd`: the principle (a session never slows for a viewport),
  skip-to-latest, trailing history, holes.
- `doc/posh-server.1.scd`: the daemon log lines (`dropping slow client …
  paced=`) and that the cap is a backstop.
- "Commands to transition states": `posh status` (RFC 0014) shows each
  attached viewport's mode — add the field — so "is this viewport paced?"
  has a command, not a log grep.
- `just lint-doc` after every edit (scdoc: a line starting with `[` and a
  `*` inside `_italic_` are parse errors).

### Task 7.2: Promotion steps, written down
- In the FDR: the three states per path (opt-in → default-on → only mode),
  the evidence required to move, and the exact change that moves each.

### Task 7.3: Retire the old delivery mode
- **Promotion criteria:** both paths default-on for a release with no
  wedge reports; no supported viewport build lacks `CAP_PACED`.
- Remove the append-per-PTY-read path for frame clients and v1 history
  emission from the daemon; `Tag::Output` clients (the version-skew case)
  keep working. Its own commit, its own merge.
- FDR *Flood delivery* → `stable` is a separate deliberate act
  (`docs/README.md`), not part of this task.

---

## Queue: issues discovered along the way

Operator instruction (2026-10-05): every issue discovered during this work
is queued and worked **after the main sequence** (the stages above), not
interleaved with it. Add to this list as issues are filed; work it top to
bottom once Stage 7 is done, or sooner only if the operator re-orders it.

| # | Issue | Found | Note |
|---|---|---|---|
| 1 | **posh#226** — a viewport dropped by the backlog valve is never told why | field triage | Required by decision 6's backstop; independent of every stage. |
| 2 | **posh#227** — drop policy for a healthy-but-outpaced client | UX grilling | Settled by decision 6 (recorded on the issue); close when Stage 2 lands — no separate work expected. Closed by the Stage 2 merge. |
| 3 | **posh#228** — `dump_vt`'s relative cursor anchor is moved by modes replayed after the flow (scroll region, origin mode, tab stops, kitty placements, DECCOLM) | Task 1.1 edge-case tests and review | Pre-existing; the mirror's cursor lands on the wrong row. Pinned by two `#[ignore]`d tests in `posh-term/src/dump.rs` (scroll region, tab stop); un-ignore both in the fix. Same-size viewports stop hitting it after Stage 1; other geometries still do. |
| 4 | **posh#229** — `dump_vt` loses a line when a soft-wrapped row is followed by an empty row; the mirror draws the screen one row low | Task 1.1 code review | Pre-existing, and an ordinary shell state triggers it (wrap a command by one character, backspace). Pinned by an `#[ignore]`d test; un-ignore it in the fix. Same-size viewports stop hitting it after Stage 1; `posh history` (VT form) and other geometries still do. |
| 5 | **posh#238** — one socket write per daemon iteration: measure whether a small send buffer (macOS 8 KiB) still out-produces a healthy reader after Stage 1 | Stage 1 review | Unverified. Stage 2 removes the platform dependence; close with the measurement either way. |
| 6 | **posh#233** — bound the frame dump at any geometry while the alternate screen is active | Stage 1 review | Unproven; equivalence tests first. Would close the mismatched-geometry gap for full-screen applications. |
| 7 | **posh#235** — return a dump's shape with its bytes (one call, not `dump_vt_mirror` + `dump_vt_mirror_is_bounded`) | Stage 1 cleanup | Settle before promoting the provisional posh-term entries to the frozen list. |
| 8 | **posh#236** — tighten the taller-mirror replay from `2 * mirror_rows` | Stage 1 cleanup | `2 * mirror_rows - rows` now; the exact height difference in the posh#229 fix. |
| 9 | **posh#237** — `FrameProducer::encode_visible` clones its acked dump and snapshot per encode | Stage 1 cleanup | Pre-existing; still ring-sized for a viewport on the full dump. posh-proto API change. |
| 10 | **posh#234** — `broadcast_output` clones the `Snapshot` for a producer-less client, then drops it | Stage 1 cleanup | Only matters with baseline and frame clients attached together. |
| 11 | **posh#231** — can an orphaned zero-width spacer cell break the soft-wrap replay? | Stage 1 review | A reviewer's theoretical gap; not established that posh-term can produce the state. |
| 12 | **posh#230** — build `ClientConn` test fixtures from one constructor | Stage 1 cleanup | Test maintenance; eight struct literals today. |
| 13 | **posh#232** — a shared send/receive helper for `remote/server.rs`'s tests | Stage 1 cleanup | Test maintenance; ~a dozen copies of one loop. |
| 14 | *(no issue)* — `relay.rs`: `content_caps` has no doc comment of its own; `forwarded_client_caps`'s doc (`:220-229`) is fused onto it, and `bridge_init_content` now points readers there | Task 2.2 review | Pre-existing; a one-line doc fix in a separate commit. Operator sequenced it here (2026-10-06). |
| 15 | **posh#239** — `the_daemon_loop_sends_a_resized_client_a_frame_for_its_new_geometry` fails intermittently with "capability payload/entry truncated" in `mirror_frames` | Stage 2 test runs | Root cause: a test-side race on raw pre-Init output. Test fixed; the production side (a connection received broadcast output before its Init) fixed and merged 2026-10-06 (`5be221b`). **Closed.** |
| 16 | **posh#241** — `just lint-fmt` does not gate Rust formatting (`conformist.nix` is nixfmt + shfmt only); `remote/server.rs` fails `rustfmt --check` on master | Task 3.1 | Repo-level gap; decide rustfmt-in-conformist (one mechanical reformat commit) vs documenting the exclusion in AGENTS.md. |
| 17 | **posh#242** — `switch_route_target` can pick a connection that never sent `Init` (a concurrent `posh list` probe) in the never-typed tie case | posh#239 review | One-line filter on `initialized()` + a test. Pre-existing. |
| 18 | **posh#243** — RFC 0009 v2: a body lost on the wire while a later body is in flight becomes an unrepairable forward jump indistinguishable from eviction | Task 3.3 review | Inherited from `server_loop`; reachable on the default path since Stage 3. Operator kept the two-body window (2026-10-06). Proper fix: an eviction marker in RFC 0009, with Stage 4 (holes). |

Recorded elsewhere rather than filed: the `ClientConn::mirror_geometry()`
accessor is a comment on **posh#210** (the `CAP_SESSION_SIZE` / RFC 0012
implementation issue), since it is a prerequisite step of that work.

All of rows 5–13 were filed 2026-10-06. The repo has no `triage` label, so
none carries one.

## Follow-ups this plan does not do
- **Pre-attach back-fill** (FDR 0005's deferred extension). Stage 3's
  `HistoryCursor` takes its starting position and fill order as inputs so
  this needs no protocol change (decision 10).
- **A live-screen notice** (decision 12) — revisit after hands-on use.

## Open questions for the operator

1. **Stage 1 ungated** — answered: accepted (2026-10-05).
2. **Mismatched geometries.** Stage 1 preserves a taller viewport's
   rendering (history above a bottom-anchored grid) and leaves wider,
   narrower and shorter viewports on the full dump. Whether to make
   mismatched-geometry rendering consistent is still ADR 0006 / RFC 0012
   territory and still open.
3. **Trickle lever** — `POSH_HISTORY_ROWS`, default 256, ack-latency
   signal: confirm or rename before Task 3.4 merges.
