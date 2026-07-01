# Unified Durable Session Transport — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use eng:subagent-driven-development to implement this plan task-by-task.

**Goal:** Make the session daemon the single `posh-proto` `ServerFrame` producer so local clients (Unix socket) and remote clients (AEAD-UDP via a thin relay) consume one frame stream — eliminating the `host:session` double terminal model and bringing diff/morph/scrollback-sync and the command palette to local sessions.

**Architecture:** FDR 0001 "Architecture B" (A3). Extract a transport-agnostic `FrameProducer` from `remote/server.rs`; the daemon runs one producer per frame-capable client and emits `Tag::Frame` over the socket, negotiated via the existing RFC 0001 capability table, falling back to `Tag::Output`. The reliable socket is the lossless degenerate of the datagram protocol (immediate acks, no fragmentation/RTO). `posh-server` is reduced to a frame relay. A new explicit CLI (`posh attach` + a TTY-gated picker, unified `posh list`) sits on top.

**Tech Stack:** Rust workspace (`crates/posh`, `crates/posh-proto`, `crates/poshterity`); `posh_term::Terminal`; `posh-proto` `framesync` codecs + `caps` table; Go/Bubble Tea for the picker (`posh-palette` renderer, `spinclass/internal/sessionpick` as prior art). Tests: `just debug-cargo test -p posh` (fast loop), `just build-rust` (hermetic gate), `FrameHarness` (`poshterity/src/framereplay.rs`).

**Rollback:** A single `POSH_FRAMESYNC`-style build/env switch forces the daemon to emit `Tag::Output` only and the remote bootstrap to use the legacy `posh-server new -- posh attach` composition (RFC 0008 §6). Each phase is independently revertable; the wire is capability-negotiated so mixed-version peers degrade, never corrupt.

**Normative references:** FDR 0011 (`docs/features/0011-unified-durable-sessions.md`), RFC 0008 (`docs/rfcs/0008-unified-session-frame-transport.md`), design trail (`docs/plans/2026-06-30-unified-session-transport-design.md`), committed at `7622029`.

**Downstream / adjacent (sequenced in from upstream, 2026-07-01):**
- **FDR 0012** (`docs/features/0012-session-layer-collapse.md`, `exploring`) — a relay-*retarget* feature (posh-in-posh collapse) that hangs off Phase 3's relay. RFC 0008 §3 was amended to anticipate retargeting. It is **not** implemented in this plan; it only **constrains the Phase 3 relay design** (below) and MUST NOT promote past `proposed` until this plan's relay is `experimental`.
- **`docs/wheel-scroll-behavior.md`** — the default gate-OFF wheel→arrow passthrough (terminal-native alternate-scroll) and the *maturity* bar for flipping `POSH_SESSION_FRAMES` on. Sharpens the Phase 2 exit gate / Phase 5 flip call (below).

---

## Granularity note

**Phase 1 is fully bite-sized** (TDD steps, exact files, exact commands) because its shapes are known cold from the current code. **Phases 2–5 are specified as task lists** — files to touch, what/why, acceptance criteria, test approach, commit boundary — to be expanded into bite-sized TDD steps *when reached*, because each depends on the concrete types the prior phase introduces (notably the `FrameProducer` API and the client-side applier). The `eng:subagent-driven-development` executor elaborates per task anyway.

## Dev-loop commands (use throughout)

- Fast single-crate test: `just debug-cargo test -p posh <module::path>`
- Fast proto test: `just debug-cargo test -p posh-proto <module::path>`
- Harness test: `just debug-cargo test -p poshterity framereplay`
- Compile check (cheap): `just debug-cargo build -p posh`
- Hermetic gate (pre-merge runs this anyway — do NOT run redundantly): `just`
- Commit cadence: one commit per task (test+impl together), conventional-commit style (`feat(posh): …`, `refactor(posh-proto): …`), signed (piggy-agent must be unlocked).

---

# Phase 1 — Daemon frame production over the socket

Lands the producer, the `Tag::Frame` path, and capability negotiation — all gated so nothing changes for existing clients (no client advertises frames until Phase 2). Promotion/rollback: `POSH_FRAMESYNC` off ⇒ `Tag::Output` only.

### Task 1.1: Add `Tag::Frame = 12`

**Promotion criteria:** N/A (additive).

**Files:**
- Modify: `crates/posh/src/session/ipc.rs:9-26` (enum), `:28-45` (`from_u8` match)
- Test: same file's `#[cfg(test)]` mod (`ipc.rs:250`)

**Step 1 — failing test:**
```rust
#[test]
fn frame_tag_roundtrips() {
    let f = encode_frame(Tag::Frame, b"abc");
    let mut fb = FrameBuffer::new();
    fb.feed(&f);
    let got = fb.next().unwrap().unwrap();
    assert_eq!(got.tag, Tag::Frame);
    assert_eq!(got.payload, b"abc");
}
```
**Step 2 — run, expect fail:** `just debug-cargo test -p posh ipc::tests::frame_tag_roundtrips` → FAIL (`Tag::Frame` undefined).

**Step 3 — implement:** add `Frame = 12,` to the enum; add `12 => Some(Tag::Frame),` to `from_u8`.

**Step 4 — pass:** rerun → PASS.

**Step 5 — commit:** `feat(posh): add Tag::Frame=12 session IPC tag`.

### Task 1.2: Extract a transport-agnostic `FrameProducer` into `posh-proto`

The linchpin. Lift `server.rs`'s `FrameState` + acked-base + `outstanding` + encoder-selection (`server.rs:119-144,210-228,725-760,867-888,1175-1228`) into a reusable producer. **No behavior change to `posh-server` yet** — it's a pure refactor that `server.rs` then calls.

**Promotion criteria:** N/A (refactor; server.rs behavior must be identical, proven by its existing tests at `remote/server.rs:1236` staying green).

**Files:**
- Create: `crates/posh-proto/src/framesync/producer.rs`
- Modify: `crates/posh-proto/src/framesync/mod.rs` (export `Producer`)
- Modify: `crates/posh/src/remote/server.rs` (replace the inline FrameState/acked/outstanding logic with `Producer` calls)
- Test: inline `#[cfg(test)]` in `producer.rs`

**API to introduce** (wraps existing `FrameEncoder`, `Baseline`, `CurrentFrame`, `ServerFrame`):
```rust
pub struct FrameProducer { /* num, current FrameState-equiv, acked Baseline, outstanding: Vec<_>, sb_total, encoder: Box<dyn FrameEncoder> */ }

impl FrameProducer {
    pub fn new(rows: u16, cols: u16, sync: FrameSync) -> FrameProducer;
    /// Produce the next frame body from the terminal's current screen against
    /// the acked base; bump frame_num; retain the frame as outstanding.
    pub fn next_body(&mut self, dump: &[u8], snapshot: &Snapshot, alt: bool, dims: (u16,u16)) -> (u64 /*num*/, FrameBody);
    /// Advance the acked base to `num` (from current or outstanding); drop older outstanding.
    pub fn ack(&mut self, num: u64);
    /// Force the next body to be a Full keyframe (resize / fresh attach).
    pub fn reset(&mut self);
}
```

**Steps (TDD):**
1. Write `producer.rs` tests first that pin today's `server.rs` semantics: (a) first `next_body` with no ack ⇒ `FrameBody::Full`; (b) after `ack(1)`, second `next_body` ⇒ `Diff`/`Morph` against base 1; (c) `ack` of a dropped (not-outstanding) frame ⇒ next body `Full` (lost-base path, mirrors `update_acks` setting `acked_data=None`). Model on `poshterity` `FrameHarness` usage and `framesync` codec tests.
2. Run → fail (no `Producer`). 
3. Implement `Producer` by moving the logic verbatim from `server.rs` (the `FrameState`, `acked_num/acked_data/acked_baseline`, `outstanding`, the `morph_enc`/`dumpdiff_enc` selection at `server.rs:884-888`).
4. Refactor `server.rs` to call `producer.next_body(...)` + `producer.ack(msg.acked_frame)`; delete the now-duplicated inline state. Keep `ServerFrame` assembly (`server.rs:1069-1080`) in `server.rs` (it owns flags/caps/input_ack/echo_ack).
5. `just debug-cargo test -p posh-proto framesync::producer` PASS **and** `just debug-cargo test -p posh remote::server` PASS (no regression). 
6. Commit: `refactor(posh-proto): extract FrameProducer from remote server`.

### Task 1.3: Negotiate capabilities on `Tag::Init`

**Promotion criteria:** N/A (additive; baseline daemons ignore the trailing table).

**Files:**
- Modify: `crates/posh/src/session/client.rs:186-192` (append `caps::own_table` after the 4-byte resize in the Init payload)
- Modify: `crates/posh/src/session/daemon.rs:496-508` (after `decode_resize`, parse a trailing capability table if present)
- Modify: `crates/posh/src/session/daemon.rs:101-114` (add `caps: Vec<caps::Cap>` to `ClientConn`, default empty)
- Reuse: `posh_proto::caps::{own_table, decode_table, find, Cap, CAP_PROTOCOL_VERSION}`
- Test: `daemon.rs:610` mod

**Wire shape:** Init payload = `encode_resize(rows,cols)` (4 bytes) `++ caps::encode_table(&caps::own_table(&extras))`. A baseline daemon reads only the 4-byte prefix and ignores the rest (verify `decode_resize` tolerates trailing bytes — it slices `[0..4]`).

**Steps:**
1. Test: a daemon fed an Init payload with a 4-byte resize **plus** a cap table records `CAP_PROTOCOL_VERSION` on that `ClientConn`; an Init with only 4 bytes records empty caps (baseline). Drive via the existing daemon-loopback test harness (`tests/session_integration.rs` style or the inline daemon test).
2. Run → fail.
3. Implement: client appends `caps::encode_table(&caps::own_table(&[]))`; daemon, in the `Tag::Init` arm, after `decode_resize`, if `frame.payload.len() > 4` calls `caps::decode_table(&frame.payload[4..])` and stores the result on `c.caps`.
4. Pass. Commit: `feat(posh): negotiate capability table on session Tag::Init`.

### Task 1.4: Per-client `FrameProducer` in the daemon; emit `Tag::Frame`

**Promotion criteria:** retire `Tag::Output` once the promotion criterion (FDR 0011) is met; until then both coexist by negotiation.

**Files:**
- Modify: `crates/posh/src/session/daemon.rs:101-114` (`ClientConn` gains `producer: Option<FrameProducer>`, set when `c.caps` advertises frames)
- Modify: `crates/posh/src/session/daemon.rs:411-444` (broadcast: for frame-capable clients, produce a frame from `term` and queue `Tag::Frame`; non-capable keep `Tag::Output`)
- Modify: `crates/posh/src/session/daemon.rs:596-599` (replay: for frame-capable clients, the first frame is the producer's `Full` — drop the `dump_vt_flat()`→`Tag::Output` path)
- Gate: read `POSH_FRAMESYNC` (off ⇒ never construct a producer ⇒ always `Tag::Output`)
- Test: `daemon.rs:610` + a new loopback integration test

**Key behaviors:**
- A frame-capable `ClientConn` gets `Some(FrameProducer::new(rows,cols,sync))`. On each visible `term` change: build `dump = term.dump_vt()` + `snapshot = Snapshot::from_term(term)`, call `producer.next_body(...)`, wrap in `ServerFrame{ flags, caps: own_table(&[]), frame_num, input_ack: 0 (socket input is reliable; see Task 1.5), echo_ack: 0, body }`, `queue(Tag::Frame, &frame.encode())`, then **immediately `producer.ack(num)`** (reliable transport — Task 1.5).
- Fresh attach ⇒ producer has no base ⇒ first body is `Full` = the replay. The `dump_vt_flat()` replay path is removed for frame-capable clients.
- Per-client producers are independent (clients attach at different times ⇒ different bases).

**Steps:** TDD via a loopback test: start a daemon, connect a synthetic frame-capable consumer (advertise caps in Init), feed PTY output, assert the consumer receives `Tag::Frame` records whose decoded `ServerFrame` bodies reconstruct the screen (apply via a `FrameApplier` into a scratch `Terminal`, compare `Snapshot::from_term` to the daemon's). A non-capable consumer still gets `Tag::Output`. Commit: `feat(posh): daemon emits ServerFrames to frame-capable clients`.

### Task 1.5: Reliable-as-degenerate (socket producer never loses a base)

**Promotion criteria:** N/A.

**Files:** `crates/posh/src/session/daemon.rs` (the producer call site from 1.4); Test: `poshterity` property test or an inline daemon test.

**Behavior (RFC 0008 §2):** over the socket the daemon calls `producer.ack(num)` immediately after `next_body` — base is always the last frame; no fragmentation, no AEAD, no RTO, no `outstanding` growth. `input_ack`/`echo_ack` are inert (set 0). A negotiated `BASE_SUM` is honored but never triggers a resync.

**Steps — property test:** feed an identical input script through (a) the daemon's socket producer and (b) a lossless `FrameHarness` (`FrameSync::Morph` and `::DumpDiff`); assert both yield identical final client `Snapshot`s, and that the socket path emits zero `Full` frames after the first (always-acked ⇒ always diffable). Run: `just debug-cargo test -p poshterity` + the daemon test. Commit: `test(posh): reliable-transport-as-degenerate property test`.

### Task 1.6: Socket version-skew matrix

**Promotion criteria:** N/A (this is the dual-architecture proof).

**Files:** new test module (model on `posh-proto/src/caps.rs:370-450` and `tests/session_integration.rs`).

**Cases (RFC 0008 §6):** new-daemon×new-client ⇒ frames; new-daemon×old-client (no Init table) ⇒ `Tag::Output`; old-daemon×new-client (table ignored) ⇒ `Tag::Output` rendered; old×old ⇒ unchanged. Simulate "old" by withholding/ignoring the cap table. Commit: `test(posh): 4-way session-socket version-skew matrix`.

**Phase 1 exit gate:** `just` green; `POSH_FRAMESYNC=off` reproduces today's behavior exactly.

---

# Phase 2 — Unified client (local frame consumer) + palette-local

**Goal:** the local client consumes `Tag::Frame` via a client-side `Terminal` + `FrameApplier`, rendering deltas in place like the remote client. Key decomposition insight (research 2026-07-01): the truly shared primitives already live in `posh-proto` (`framesync::FrameApplier` + `DumpDiff`/`MorphDelta`; `display::{Snapshot::from_term, new_frame_opt}`). Over the reliable socket the local consumer is a **clean minimal consumer** over those primitives — it idles the loss/latency machinery (no base-sum/resync, no prediction) exactly as the daemon producer does, so it is NOT a fork of the 144k remote client. The genuinely-shared UI machinery (palette compositor, scrollback ring) is extracted to a shared module in later tasks; the transport-specific orchestration (resync, prediction, roaming) stays remote-only.

Landmarks: remote consumer/render machinery `remote/client.rs:1585-1984` (`apply_frame`/`render`/`compose_frame`/`composite_palette`) around `ClientState`; local client `session/client.rs:176` `client_loop` (3 poll fds; `Tag::Output`→`stdout_buf` at :288; no posh_term model; already enters alt-screen + sends caps in Init at :186).

**Tasks:**
- **2.1 Local client consumes `Tag::Frame` (minimal reliable consumer).** In `session/client.rs` `client_loop`: hold a client-side `posh_term::Terminal`, `applied_data`/`applied_num`, `last_drawn: Snapshot`, and a `Box<dyn FrameApplier>`; add a `Tag::Frame` arm that decodes the `ServerFrame`, applies the `FrameBody` via the applier, and renders the delta to `stdout_buf` via `Snapshot::from_term` + `new_frame_opt` (repaint in place, no scroll). Tolerate leading `Tag::Output` (raw to stdout until the first `Full` establishes the model — no special handling needed per research). Reliable-degenerate: no base-sum/resync, no prediction. MUST keep green: DetachMatcher units, `Tag::Exit` code propagation, alt-screen takeover/restore, SIGWINCH/SIGCONT. Test: loopback/pty test that with `POSH_SESSION_FRAMES=on` the local client renders the same screen the raw path produced. Commit boundary: client renders frames.
- **2.2 End-to-end local-frames verification + gate.** Confirm the full local path (daemon `POSH_SESSION_FRAMES` on → local client consumes) renders identically to today's raw path; gate-off → `Tag::Output` unchanged. May fold into 2.1's tests. Acceptance: end-to-end local frame rendering verified; gate-off inert.
- **2.3 Extract the shared palette compositor.** Lift `composite_palette` (`client.rs:1869`) + the `Palette` lifecycle (`remote/palette.rs`: spawn/open/screen/pump/poll_events) into a shared module both clients use; behavior-preserving for remote (`remote/client.rs` tests stay green). Acceptance: one palette code path.
- **2.4 Palette on local sessions.** Wire `Ctrl-^` in `session/client.rs` to the shared compositor onto the local Snapshot; transport-aware menu (no "quit transport" locally → map to detach). Escape-to-shell: the DAEMON spawns the overlay PTY in the session cwd (generalize `remote/server.rs`'s overlay to the daemon, which already owns the PTY/cwd). Acceptance: palette opens over a local session; shell-out works. Cross-ref FDR 0008/0009.
- **2.5 Scrollback sync (the #16 gate-flip prerequisite).** Daemon produces `FrameBody::Scrollback` (via `FrameProducer::advance_scrollback` + a Scrollback body read from the daemon's `Terminal` ring — the ring-read logic currently in `server.rs`); local client keeps a scrollback ring + scroll view (share `ScrollbackRing` + `compose_scroll_frame` from `remote/client.rs`). Negotiate `SCROLLBACK`; verify depth parity (daemon ring ≥ a bare terminal's). Acceptance: local scrollback works via posh with no outer-terminal regression — this unblocks flipping the gate.

**Phase 2 exit gate:** local attach renders frames + palette + scrollback locally; `POSH_SESSION_FRAMES` can be flipped on for local without regression; `just` green. (Defaulting the gate ON is a rollout decision, deferred to Phase 5 / a separate call.)

**Gate-flip maturity bar (per `docs/wheel-scroll-behavior.md`).** Flipping the fleet default is *not* just "no regression": it changes user-visible behavior — the wheel goes from terminal-native arrows (alternate-scroll passthrough) to posh's scroll-view, and scrollback ownership moves to posh (the #104 convergence). So the flip call weighs **soak + palette-local (2.4)**, not the gate alone. Reconciling the doc with this plan's thesis: it lists "no resync/prediction" as a consumer gap, but on the reliable local socket those are absent **by design** (reliable-as-degenerate, Phase 2 intro) — *not* a maturity deficit. The genuine pre-flip items are therefore palette-local (2.4) and a real end-to-end soak; the #106 wheel-teardown and #107 resize-gap accept are already resolved. A personal `POSH_SESSION_FRAMES=1` opt-in (`~/.env`) is the dogfooding path ahead of the fleet-wide call; eng/clown set no gate, so the fleet default stays OFF until then.

---

# Phase 3 — Frame relay (reduce posh-server)

**Goal:** `posh-server`, in the `host:session` case, connects to the session socket and relays frames over UDP instead of running an inner `posh attach` in a second PTY. No second terminal model.

**Design constraint (FDR 0012 — retarget-readiness).** The relay's only per-session state is *which daemon socket it is connected to*. Structure that target as a **replaceable field, not a construction invariant**: the relay must be able to drop its current daemon connection and open a new one mid-transport, letting the new daemon's `Full` keyframe (§2) re-establish the base — the same reset as a fresh attach. RFC 0008 §3 (amended) says nothing may presume a single fixed target for the transport's lifetime. Do **not** build the layer-collapse UX (trigger, replace-vs-stack, the offer prompt) here — that is FDR 0012's job, downstream — but do not *preclude* it: a retrofit from a hardcoded single target is expensive, a replaceable field is nearly free now.

**Tasks:**
- **3.1 Relay mode in posh-server.** New path: connect to the session's Unix socket as a frame-capable client (reuse `client_core`/`FrameBuffer`), relay each `Tag::Frame` body into a datagram `ServerFrame` (it already *is* one — re-seal/fragment, don't re-model). Bridge the UDP reliable input stream (`InputInbox`, `sync.rs:422-445`) into socket `Tag::Input` writes. Reuse the `FrameProducer`? No — the daemon now produces; the relay forwards. Acceptance: `posh box:dev` over loopback works with one terminal model (assert no second `Terminal` constructed in the session path).
- **3.2 Capability bridging.** Relay terminates `AGENT_FORWARD`/`AGENT_DATA`/`AGENT_ACK` (its `AgentEndpoint`, unchanged) and forwards all other cap entries transparently between client and daemon. Acceptance: agent forwarding still works for sessions created through a forwarding connection; content caps pass through (scrollback/morph negotiated client↔daemon).
- **3.3 Bootstrap selection.** `remote_command` (`sshwrap.rs:60-95`) / `cmd_ssh_session` (`main.rs:352-398`) pick the relay path; legacy `posh-server new -- posh attach` stays behind `POSH_FRAMESYNC` as rollback (RFC 0008 §5.2/§6). Acceptance: both paths interoperate by negotiation.

**Phase 3 exit gate:** `host:session` runs single-model; agent forwarding unchanged; cross-host manual walkthrough (`docs/manual-testing.md`) added.

---

# Phase 4 — CLI surface: `posh attach`, picker, unified `posh list`

**Goal:** the explicit interface from FDR 0011 / RFC 0008 §5.1.

**Tasks:**
- **4.1 Grammar amendment.** `target.rs`: `box:`/`user@box:` ⇒ host scope; `:` ⇒ local scope; bare `Host` under `attach` ⇒ host scope (not plain shell). Add scope target kinds. TDD against the table-driven tests at `target.rs:122-286` (extend the normative table; update changed rows with comments citing RFC 0008 §5.1). Acceptance: amended `Target::parse` table green; legacy non-scope forms unchanged.
- **4.2 `--ephemeral`.** `attach` flag selecting the legacy daemon-less roaming shell for a host target (deferrable per FDR 0011). 
- **4.3 Picker.** Port `spinclass/internal/sessionpick` (Bubble Tea filterable list, local+remote rows, description-titled). TTY-gated: non-TTY ⇒ error with candidate list, never launch (RFC 0008 §5.1). Wire to `posh attach` with a scope/empty target. Reuse the `posh-palette` Go module dependency surface.
- **4.4 Unified `posh list`.** Model on `spinclass/cmd/spinclass/list_view.go`: four modes (plain/non-TTY, pretty lipgloss TTY default, `--format json`, `--watch` Bubble Tea). Columns: URI, status, last-activity, cwd, description. **Hide remotely-spawned detached workers by default**; `--workers` includes them. Acceptance: pipes stay plain; TTY styled; worker filter correct.
- **4.5 Session description.** Settable label stored with the session (new `Tag`/IPC verb or daemon metadata via `Tag::Info`), surfaced in `list` + picker.
- **4.6 Completion.** Extend `completions.rs` for the amended grammar: bare ⇒ local sessions + host aliases + subcommands; `host:` ⇒ that host's session names (existing cached ssh query).

**Phase 4 exit gate:** `posh attach`/`list`/picker/completion work local+remote; `just` green; bats lane (new — none exist yet) optional follow-up.

---

# Phase 5 — Retire the double-model

**Goal:** once the promotion criterion holds, delete the legacy path.

**Promotion criteria (FDR 0011):** both fleet hosts on a frame-protocol build, two weeks of daily cross-host use, **zero** observed fallback to the `Tag::Output` / inner-attach path.

**Tasks:**
- **5.1** Remove the `posh-server new -- posh attach` inner-attach composition and the second-PTY path from `server.rs`.
- **5.2** Remove `Tag::Output` emission from the daemon (keep the tag for one release as a defensive decode, then drop) and retire `Tag::History` (subsumed by `SCROLLBACK` frames).
- **5.3** Remove the `POSH_FRAMESYNC` rollback switch.
- **5.4** Update FDR 0011 / RFC 0008 status to `accepted`; prune the Compatibility/rollback sections that referenced the dual path.

**Out of scope (tracked):** agent forwarding on durable/local-origin sessions — #53 (`Setenv`/`posh setenv`), #103 (host-global filesystem rendezvous); multi-host aggregated `list`; auto-reaping.

---

## Testing appendix

- **Unit/producer:** `crates/posh-proto/src/framesync/producer.rs` (inline), model on `framesync` codec tests + `FrameHarness`.
- **Caps/skew:** model on `crates/posh-proto/src/caps.rs:370-450`.
- **Grammar:** extend `crates/posh/src/target.rs:122-286`.
- **Daemon/loopback + e2e:** `crates/posh/tests/session_integration.rs` (spawns the `posh` binary).
- **Deterministic frames:** `crates/poshterity/src/framereplay.rs` `FrameHarness` (`new`/`feed`/`deliver`/`ack`/`converged`/`drop_next`/`server_snapshot`/`client_snapshot`).
- **Cross-host** (real sshd, agent forwarding, roam): manual, `docs/manual-testing.md`.
- **Gate:** `just` (validate+lint+build+test) — the pre-merge hook; don't run redundantly.
