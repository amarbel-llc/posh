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
- **Exit check: NOT yet done.** The field run — a `nix gc`-shaped flood on a
  remote session, SIGUSR2 to the mux peer twice and `session_frames=`
  compared, Ctrl-C mid-flood — needs this build deployed to the remote host.

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
  Expected but unverified until Task 2.5's measurement: with far fewer
  frames in flight per round trip, a remote viewport's acks land inside
  the producer's 8-frame window again, so it leaves the lost-base regime
  in which v1 history stops entirely.
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
paced, whether `sb_acked` is now non-zero — the "remote viewport leaves the
lost-base regime" expectation above is unverified until this row.

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

Decisions 1, 3, 7.

### Task 3.1: Extract `server_loop`'s v2 cursor into a shared type
- Files: new `crates/posh/src/remote/sb2.rs` (or `posh-proto` if the
  harness needs it); `remote/server.rs:1301-1307`, `:1863-1888`,
  `:2012-2022`, `:2172-2205`.
- A `HistoryCursor { epoch, epoch_base, acked_rows, sent_upto, last_send }`
  with `on_client_entry`, `on_resize`, `want(now, rto)`, and
  `next_body(term, cap) -> FrameBody::Scrollback2`. Pure refactor first:
  `server_loop` uses it and its tests are unchanged. The daemon comment
  "mirror of server.rs:761-770 — keep in sync" is what this removes.
- Tests: the existing v2 tests pass untouched; add unit tests for the
  cursor (in-order, RTO re-anchor, eviction forward jump, epoch bump).

### Task 3.2: The bridge carries the viewport's v2 entry
- Files: `remote/relay.rs` (`content_caps` gains `CAP_SCROLLBACK2` for
  Init), `remote/server.rs:1156-1169` (`bridge_client_message` forwards
  the per-message `CAP_SCROLLBACK2` entry — it carries the cumulative ack —
  via `Tag::ClientCaps`, in the M2 bridge's own list, never the relay's).
- Tests: the daemon sees v2 on Init and a fresh ack on each client message.

### Task 3.3: Daemon emits v2 for paced viewports
- Files: `session/daemon.rs` — a `HistoryCursor` per paced viewport that
  advertised v2; `absorb_client_caps` feeds it; the send pass from 2.3
  alternates screen and history bodies. v1 `maybe_queue_scrollback` remains
  for non-paced viewports only.
- The server's `SCROLLBACK2` ack entry rides every frame (the viewport
  keys its ring reset off it — `remote/client.rs:3126-3139`).
- Tests: rows shipped == rows scrolled (no repeats) under every
  `FloodAcks` cadence; with acks withheld, rows are resent only after the
  RTO and from the ack; a flood of more than one ring with a stalled
  viewport yields one forward jump of the right size; the Task 0.2 test's
  `Never` cadence now also satisfies the backlog bounds (delete its
  carve-out).

### Task 3.4: The trickle — live first, history by backpressure
- Lever: the history ceiling rides `CAP_PACED`'s payload (rows per body;
  `0` = live only), set on the viewport by `POSH_HISTORY_ROWS` (name and
  default are an **open item** in the design doc — propose `256`, matching
  `SB2_ROWS_PER_BODY`, and confirm with the operator before merging).
- Dynamic share: start at the ceiling; halve the per-body row budget when
  the last screen frame's ack took more than twice the running minimum;
  double (up to the ceiling) when it did not. A screen body always wins
  the opportunity when the viewport is dirty; a history body takes it
  otherwise, and at least every Nth opportunity while the budget is
  non-zero so history cannot starve on a busy screen.
- Tests: with a slow fake link, screen latency stays within one interval
  while history advances; with ceiling `0`, no history body is ever sent;
  budget shrinks under induced ack delay and recovers.
- This is the task most likely to need tuning against a real link; record
  the measurement recipe (extend `debug-mux-load`) and the chosen numbers
  in the FDR.

### Task 3.5: Records
- RFC 0009: the session-socket path (Init-persistent advertisement +
  per-message ack via `ClientCaps`), stays `experimental`.
- FDR *Flood delivery*: the trickle lever and its measured default.

**Stage 3 exit check:** after `nix gc` on a remote attach, wheel-up shows
the output contiguous; `posh history` on the session and the viewport's
scrollback agree for the last screenfuls.

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
