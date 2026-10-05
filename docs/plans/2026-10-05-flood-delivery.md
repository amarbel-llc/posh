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
replays, and the rendering is pinned byte-for-byte by tests. This plan
ships Stage 1 **ungated**, rollback by revert. If a lever is wanted
anyway, the cheapest is a daemon-side `POSH_FRAME_TAIL=full` read at
session start — but it cannot take effect without a new session, which is
why it is not proposed.

---

## Facts the plan relies on (verified 2026-10-05, worktree HEAD `ee87e23`)

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

Decisions 2, 5, 6, 13. After this stage the daemon holds at most one
unsent screen per capable viewport, whatever the frame size.

**The model, mirrored from `server_loop`:** per viewport, a `dirty` flag
(the source terminal's `generation()` moved past the last framed one) and
a *send opportunity*. An opportunity exists when the viewport's
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

### Task 2.1: Allocate the capability
- Files: `crates/posh-proto/src/caps.rs`; `docs/rfcs/0001-*.md` (registry
  table, in place).
- `CAP_PACED` (next free id — read the RFC 0001 table first): a viewport
  advertises it to say "decide what to send me at send time". Payload one
  version byte, room to grow (Stage 3 adds the history ceiling).
- Tests: encode/decode round trip; unknown-payload-length tolerated.

### Task 2.2: The viewport advertises it; the bridge carries it
- Files: `remote/client.rs` (advertise beside `CAP_SCROLLBACK2`, ~`:3810`),
  `remote/relay.rs:246-251` (`content_caps` — the Init path; this is
  negotiated on Init, not an introspection forward), `remote/server.rs`
  bridge tests (`:4724`, `:5080` assert the Init table).
- Lever: `POSH_PACED` on the viewport — unset/`1` advertises, `0` does
  not. Parsed once, by one function, shared by remote and (Stage 6) local.
- Tests: default advertises; `POSH_PACED=0` does not; the bridge's daemon
  Init carries it iff the viewport's message did.

### Task 2.3: Daemon — dirty tracking and send-time production
- Files: `session/daemon.rs` — `ClientConn` (new: `paced`, `last_gen`,
  `fresh_sent_ms`, `fresh_num`), `broadcast_output`, `queue_frame`, the
  main loop's write section (`:1800-1829`) and `util::poll` timeout
  (`:1391`, today `-1`).
- A paced viewport is skipped by `broadcast_output` (marked dirty). After
  the per-client read/write section, a new pass produces at most one fresh
  visible frame per paced viewport that has an opportunity. The poll
  timeout becomes the nearest pacing deadline among dirty paced viewports,
  `-1` when there is none.
- Tests (in-process, the `measure_flood` harness extended with a paced
  client and a fake clock injected as a `now_ms` parameter — do not sleep
  in tests):
  - a flood with acks withheld queues **exactly one** visible frame, then
    one per interval;
  - with prompt acks, frames ≤ chunks and `write_buf` never holds two
    visible frames;
  - `peak_write_buf` < 64 KiB for every cadence in `FloodAcks`, full ring
    or empty;
  - the final frame after the flood ends reflects the terminal's last
    state (no stale screen at quiescence — the `server_loop` wedge class;
    mirror its `force_frame`-at-quiescence nudge).
- A viewport without `CAP_PACED` takes the existing path untouched: assert
  its byte stream for a fixed input is identical before and after
  (`posh225_*` measurement on a non-paced client as the witness).

### Task 2.4: The backstop stays, and says so
- `MAX_CLIENT_BACKLOG` remains for non-frame clients and bugs (decision 6).
  Add the `paced=` flag to the `dropping slow client` log line so a drop
  of a paced viewport is recognisable as a bug.
- The reason reaching the viewport is posh#226; link it, do not do it here.

### Task 2.5: Records
- FDR *Flood delivery* created at `experimental` in the commit that lands
  2.3 (levers: `POSH_PACED`; rollback: unset the cap).
- RFC 0008 §3: the send discipline for a paced session-socket client.

**Stage 2 exit check:** a remote flood shows the bridge forwarding a
handful of frames per second instead of one per PTY read
(`just debug-posh-dump` on the bridge; add a counter to the status line if
one is missing), and Ctrl-C mid-flood repaints within one pacing interval.

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

## Follow-ups this plan does not do

- **posh#226** — tell a dropped viewport why. Required by decision 6's
  backstop; independent of every stage here.
- **posh#227** — settled by decision 6 (recorded on the issue); close it
  when Stage 2 lands.
- **Pre-attach back-fill** (FDR 0005's deferred extension). Stage 3's
  `HistoryCursor` takes its starting position and fill order as inputs so
  this needs no protocol change (decision 10).
- **A live-screen notice** (decision 12) — revisit after hands-on use.

## Open questions for the operator

1. **Stage 1 ungated** — accept the rollout exception above?
2. **Taller viewports.** Stage 1 preserves today's rendering exactly: a
   viewport taller than the session shows earlier history above the grid
   once anything has scrolled (and the grid at the top with blank rows
   below before that). That inconsistency is ADR 0006 / RFC 0012
   territory and is left alone here. Say if you would rather Stage 1 also
   made it consistent.
3. **Trickle lever** — `POSH_HISTORY_ROWS`, default 256, ack-latency
   signal: confirm or rename before Task 3.4 merges.
