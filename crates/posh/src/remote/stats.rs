//! Optional performance instrumentation for the roaming remote transport.
//!
//! Gated by `$POSH_DEBUG_LOG`: when it names a writable path, the client and
//! server each initialize the shared file logger (`util::log_init`) and flush a
//! periodic one-line summary of transport counters through `util::log_write`
//! (timestamped, 5 MB rotation — the same sink the session daemon uses). Unset,
//! every method is a cheap no-op and the hot path is untouched.
//!
//! The collector holds only counters and primitives; the live gauges (SRTT,
//! RTO, prediction state, wire bytes) are passed in at flush time so this stays
//! decoupled from `Connection` and `PredictionEngine`.

use std::path::Path;
use std::time::Instant;

use crate::util::{self, now_ms};

/// Minimum gap between emitted summary lines: idle ticks don't spam the log.
const FLUSH_INTERVAL_MS: u64 = 1000;

/// A visible model frozen for at least this long WHILE diff frames keep
/// arriving is the apply-stall wedge signature the detector self-logs (#wedge).
const WEDGE_FROZEN_MS: u64 = 3000;

/// Frame inter-arrival gap that trips the client's "Last contact" banner
/// (#false-disconnect): mirrors `posh_proto::display::SERVER_LATE_AFTER` so the
/// stats collector counts a "late" arrival gap by the SAME threshold the banner
/// uses. Kept as a local mirror rather than importing the display constant so
/// the counter's definition is self-contained; a debug-only asserted equality
/// in the tests below guards the two against drifting apart.
const LATE_GAP_MS: u64 = posh_proto::display::SERVER_LATE_AFTER;

#[derive(Default)]
pub struct Stats {
    enabled: bool,
    /// #8/#117 watchdog auto-recovery: ON by default (a frozen visible model
    /// while frames keep arriving forces a resync — the uniform net over every
    /// silent-drop apply path). POSH_WEDGE_WATCHDOG=0/off/false/no opts out.
    /// Independent of `enabled` so recovery runs with periodic logging off.
    wedge_watchdog: bool,
    /// Whether POSH_WEDGE_WATCHDOG was EXPLICITLY set on (not just defaulted):
    /// the debug-posture signal that `want_server_diag` (CAP_DIAG, #6) keys on,
    /// so the default-on watchdog does not silently re-enable the per-frame
    /// server piggyback that default sessions deliberately avoid.
    wedge_watchdog_explicit: bool,
    /// RFC 0007 metric bus: when a GP predictor species is active the client
    /// needs the compute-timing terminals (apply/compose/loop) even with
    /// periodic logging off, so timing is collected when `instrument()` — i.e.
    /// `enabled || gp_active` — not just `enabled`.
    gp_active: bool,
    last_flush: u64,
    /// A meaningful counter advanced since the last emit. Idle-only activity
    /// (skipped renders) deliberately does not set this, so a quiescent session
    /// stops logging instead of repeating an unchanged line every second.
    dirty: bool,
    last_bytes_rx: u64,
    last_bytes_tx: u64,

    // Frame counters — client: frames received; server: frames transmitted.
    frames_total: u64,
    frames_full: u64,
    frames_diff: u64,
    frames_empty: u64,

    // Client transport-liveness (#false-disconnect): frame inter-arrival timing,
    // the signal behind the "Last contact N ago" banner. `server_late` fires on
    // a >SERVER_LATE_AFTER gap between DECODED frames, not on measured loss, so a
    // large `frame_gap_ms_max` on an otherwise reliable link is the fingerprint
    // of a lost/late heartbeat tripping the banner while the session is healthy.
    // `frame_gaps_late` counts arrival gaps that crossed the banner threshold —
    // chronic near-threshold jitter vs a single long stall. `last_frame_arrival`
    // is the wall-clock ms of the previous decoded frame (0 before the first).
    last_frame_arrival: u64,
    frame_gap_ms_max: u64,
    frame_gaps_late: u64,

    // Client render activity.
    render_writes: u64,
    render_bytes_out: u64,
    render_skipped_idle: u64,

    // Client compute timing — the mirror of the server's dump_vt_us. `apply` is
    // the full-dump re-parse (apply_frame), `compose` is the snapshot + diff
    // render (compose_frame). Reported as windowed avg + max (worst single
    // frame in the window), reset each flush, so a latency spike is visible in
    // the second it happened rather than smeared into a session average.
    apply_us_total: u64,
    apply_count: u64,
    apply_us_max: u64,
    compose_us_total: u64,
    compose_count: u64,
    compose_us_max: u64,

    // Most-recent (not windowed, not flush-reset) single-frame compute costs,
    // for the RFC 0007 metric bus. Updated by the same record_* calls; surfaced
    // via the `last_*` getters so the bus reads a current value regardless of
    // the flush cycle.
    last_apply_us: u64,
    last_compose_us: u64,
    /// Server: most-recent `dump_vt` cost. The server-forwarded `dump_vt_us`
    /// metric terminal (RFC 0007 §3); populated whenever `instrument()`.
    last_dump_vt_us: u64,
    last_loop_busy_us: u64,
    last_loop_idle_us: u64,

    // Event-loop timing (windowed): per iteration, time blocked in poll (idle)
    // vs doing work (busy), the longest single busy stretch, and the iteration
    // count. Drives the busy% / stall / jitter view. Reset each flush.
    loop_iters: u64,
    loop_busy_us: u64,
    loop_idle_us: u64,
    loop_busy_us_max: u64,

    // Client input latency (windowed, ms): keystroke -> the frame whose
    // input_ack confirms the server consumed it. Captures send-pacing + network
    // + server processing — the keystroke-lag the user feels, which srtt alone
    // (pure network RTT) misses. Reset each flush.
    input_ms_total: u64,
    input_count: u64,
    input_ms_max: u64,

    // Server framing economics.
    /// Sum of full-dump bytes over frames that had a diff option (whether or
    /// not the diff was chosen) — the denominator for `diff_saved_pct`.
    full_bytes_considered: u64,
    /// Sum of (full_len - diff_len) over frames sent as diffs.
    diff_saved_bytes: u64,
    retransmits: u64,
    dump_vt_us_total: u64,
    dump_vt_count: u64,
    dump_vt_us_max: u64,

    // Client apply-path outcome histogram (#wedge debuggability): how received
    // visible/scrollback bodies resolved in `apply_frame`. A climbing `basemis`
    // while the visible model is frozen is the apply-stall fingerprint, distinct
    // from an idle session where no frames arrive at all.
    apply_advanced: u64,
    apply_stale: u64,
    apply_dup: u64,
    apply_basemis: u64,
    /// RFC 0006 base-checksum mismatches: the diff base NUMBER matched but its
    /// CONTENT checksum did not, so the client refused the body and resynced.
    /// Distinct from `apply_basemis` (a frame-number mismatch); a nonzero count
    /// means a base divergence was caught before it could short-base wedge or
    /// silently corrupt (#94).
    apply_base_sum_mismatch: u64,
    apply_reack: u64,
    apply_nochange: u64,
    /// Received Scrollback bodies (RFC 0002) — excluded from the full/diff/empty
    /// frame counters, surfaced separately so a scrollback storm/reset is visible.
    frames_scrollback: u64,
    /// The visible/scrollback frame last seen at the apply gate: (num, base,
    /// kind). Reported in the dump + wedge line so a stall names what it rejects.
    last_rx_num: u64,
    last_rx_base: u64,
    last_rx_body: FrameKind,

    // Wedge auto-detector: the visible-model generation and the diff-rx count
    // when the model last advanced, so a model frozen past WEDGE_FROZEN_MS WHILE
    // diff frames keep arriving (the apply-stall signature) self-logs once.
    wedge_last_term_gen: u64,
    wedge_term_gen_since: u64,
    wedge_diff_at_change: u64,
    wedge_warned: bool,
}

/// Client prediction gauges sampled at flush time (POSH_DEBUG_LOG). Bundled so
/// the flush signatures don't sprout a dozen positional `u64`s that are easy to
/// transpose. `active`/`shown`/`epoch_lag` are instantaneous; the rest are
/// cumulative counters.
#[derive(Clone, Copy, Default)]
pub struct PredictSample {
    pub active: bool,
    pub shown: u64,
    pub epoch_lag: u64,
    pub resets: u64,
    pub correct: u64,
    pub nocredit: u64,
    pub incorrect: u64,
    /// `nocredit` split by cause (#predict-echo): (unknown, blank,
    /// matched_original). `matched` dominating is the credit-starvation signature.
    pub nocredit_unknown: u64,
    pub nocredit_blank: u64,
    pub nocredit_matched: u64,
}

/// Wire-body kind of a received frame, recorded at the apply gate so the dump
/// and the wedge detector can name exactly which frame a frozen client is
/// rejecting (#wedge debuggability).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FrameKind {
    #[default]
    None,
    Full,
    Diff,
    Morph,
    Scrollback,
}

impl FrameKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FrameKind::None => "none",
            FrameKind::Full => "full",
            FrameKind::Diff => "diff",
            FrameKind::Morph => "morph",
            FrameKind::Scrollback => "scrollback",
        }
    }
}

/// Snapshot of the client apply-path histogram + last received frame, for the
/// SIGUSR2 dump (#wedge debuggability). A climbing `basemis` together with a
/// frozen visible model (`term_gen`) is the apply-stall fingerprint.
#[derive(Clone, Copy, Default)]
pub struct ApplySnapshot {
    pub advanced: u64,
    pub stale: u64,
    pub dup: u64,
    pub basemis: u64,
    pub base_sum_mismatch: u64,
    pub reack: u64,
    pub nochange: u64,
    pub scrollback_rx: u64,
    pub last_rx_num: u64,
    pub last_rx_base: u64,
    pub last_rx_body: FrameKind,
}

/// Snapshot of the transport-liveness gauges for the connection-health view
/// (#false-disconnect): the counters that explain a "Last contact" banner
/// without a second terminal. A nonzero `frame_gaps_late` with a healthy
/// `retransmits` and steady `heartbeats_rx` is the false-disconnect fingerprint
/// — the banner tripped on an arrival gap, not on a genuinely dead peer.
#[derive(Clone, Copy, Default)]
pub struct LinkSnapshot {
    pub frames_total: u64,
    pub frames_full: u64,
    pub frames_diff: u64,
    /// Empty frames received — the on-wire heartbeat count (RFC 0008 §3).
    pub heartbeats_rx: u64,
    pub frames_scrollback: u64,
    /// Largest gap (ms) ever seen between two decoded frames this session.
    pub frame_gap_ms_max: u64,
    /// Arrival gaps that exceeded the banner threshold (`LATE_GAP_MS`).
    pub frame_gaps_late: u64,
    pub retransmits: u64,
}

impl Stats {
    /// Reads `$POSH_DEBUG_LOG`; on a non-empty path that `log_init` accepts the
    /// collector is enabled, otherwise it is an inert no-op. Logging is opt-in:
    /// the `#wedge` breadcrumbs and periodic summaries stay in the binary but are
    /// dormant until `$POSH_DEBUG_LOG` names a writable path (or the runtime
    /// `Ctrl-^` toggle / `SIGUSR2` opens a sink).
    pub fn new() -> Stats {
        let enabled = match std::env::var_os("POSH_DEBUG_LOG") {
            Some(p) if !p.is_empty() => util::log_init(Path::new(&p)).is_ok(),
            _ => false,
        };
        // #117: watchdog recovery is ON by default; POSH_WEDGE_WATCHDOG only
        // opts out (0/off/false/no) — mirroring the server's POSH_WEDGE_CAPTURE.
        // An explicit ON value is remembered separately as the debug-posture
        // signal for CAP_DIAG (want_server_diag), which must NOT default on.
        let watchdog_env = std::env::var("POSH_WEDGE_WATCHDOG").ok();
        let off = matches!(
            watchdog_env.as_deref(),
            Some("0") | Some("off") | Some("false") | Some("no")
        );
        let now = now_ms();
        Stats {
            enabled,
            wedge_watchdog: !off,
            wedge_watchdog_explicit: !off && watchdog_env.as_deref().is_some_and(|v| !v.is_empty()),
            last_flush: now,
            wedge_term_gen_since: now,
            ..Default::default()
        }
    }

    /// Flip the collector on/off at runtime (the `Ctrl-^ d` debug-logging
    /// toggle). When off, the periodic flush is skipped even if a sink is open.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Mark whether a GP predictor species (RFC 0007) is active. When it is, the
    /// compute timers run even with periodic logging off so the metric bus can
    /// read the `last_*` costs.
    pub fn set_gp_active(&mut self, on: bool) {
        self.gp_active = on;
    }

    /// Whether per-frame compute timing should be collected: periodic logging is
    /// on, or a GP species needs the timing terminals.
    pub fn instrument(&self) -> bool {
        self.enabled || self.gp_active
    }

    /// Most-recent single-frame compute costs (µs) for the RFC 0007 metric bus.
    pub fn last_apply_us(&self) -> u64 {
        self.last_apply_us
    }
    pub fn last_compose_us(&self) -> u64 {
        self.last_compose_us
    }
    /// The server's most-recent single-frame `dump_vt` cost (µs), for the
    /// RFC 0007 metric bus' server-forwarded `dump_vt_us` terminal. Populated
    /// whenever `instrument()`, so a GP species reads it without POSH_DEBUG_LOG.
    pub fn last_dump_vt_us(&self) -> u64 {
        self.last_dump_vt_us
    }
    /// Cumulative server retransmit count (frames re-sent for lack of an ack).
    /// The metric bus delta's it over the sample window into `retransmit_rate`.
    pub fn retransmits(&self) -> u64 {
        self.retransmits
    }
    /// Busy/idle µs of the most recent event-loop iteration (drives fps +
    /// busy-fraction in the metric bus).
    pub fn last_loop_busy_us(&self) -> u64 {
        self.last_loop_busy_us
    }
    pub fn last_loop_idle_us(&self) -> u64 {
        self.last_loop_idle_us
    }

    // --- recording -----------------------------------------------------------

    pub fn record_frame_full(&mut self) {
        self.frames_total += 1;
        self.frames_full += 1;
        self.dirty = true;
    }
    pub fn record_frame_diff(&mut self) {
        self.frames_total += 1;
        self.frames_diff += 1;
        self.dirty = true;
    }
    pub fn record_frame_empty(&mut self) {
        self.frames_total += 1;
        self.frames_empty += 1;
        self.dirty = true;
    }
    /// A received Scrollback body (RFC 0002). Tracked apart from the
    /// full/diff/empty `frames_total` economics, which it is deliberately not
    /// part of, so a scrollback storm/reset stays visible (#wedge).
    pub fn record_frame_scrollback(&mut self) {
        self.frames_scrollback += 1;
        self.dirty = true;
    }

    /// Record the wall-clock arrival of a decoded server frame for the
    /// transport-liveness view (#false-disconnect). Called once per decoded
    /// frame (before the per-kind counters), it folds the gap since the previous
    /// arrival into `frame_gap_ms_max` and, when that gap crossed the banner
    /// threshold, bumps `frame_gaps_late` — the count of moments the "Last
    /// contact" banner would have appeared. The first frame only seeds the
    /// baseline (no gap to measure), so `now == 0` and the pre-contact state are
    /// both ignored. Independent of `enabled`: liveness is cheap and useful even
    /// with periodic logging off (the palette command reads it live).
    pub fn record_frame_arrival(&mut self, now: u64) {
        if self.last_frame_arrival != 0 {
            let gap = now.saturating_sub(self.last_frame_arrival);
            self.frame_gap_ms_max = self.frame_gap_ms_max.max(gap);
            if gap > LATE_GAP_MS {
                self.frame_gaps_late += 1;
            }
        }
        self.last_frame_arrival = now;
    }

    /// Snapshot the transport-liveness gauges for the connection-health view.
    /// `heartbeats_rx` reuses `frames_empty`: an Empty body IS the on-wire
    /// heartbeat (RFC 0008 §3), so its count is the heartbeat-arrival count.
    pub fn link_snapshot(&self) -> LinkSnapshot {
        LinkSnapshot {
            frames_total: self.frames_total,
            frames_full: self.frames_full,
            frames_diff: self.frames_diff,
            heartbeats_rx: self.frames_empty,
            frames_scrollback: self.frames_scrollback,
            frame_gap_ms_max: self.frame_gap_ms_max,
            frame_gaps_late: self.frame_gaps_late,
            retransmits: self.retransmits,
        }
    }

    // --- client apply-path histogram (#wedge debuggability) ------------------

    /// Record the visible/scrollback frame seen at the apply gate. `base` is the
    /// body's diff/morph/scrollback base, or `num` for a self-contained `Full`.
    pub fn record_apply_rx(&mut self, num: u64, base: u64, kind: FrameKind) {
        self.last_rx_num = num;
        self.last_rx_base = base;
        self.last_rx_body = kind;
    }
    /// The body advanced the client's applied state (or appended scrollback).
    pub fn record_apply_advanced(&mut self) {
        self.apply_advanced += 1;
    }
    /// `frame_num < applied_num`: a stale retransmission, re-acked not applied.
    pub fn record_apply_stale(&mut self) {
        self.apply_stale += 1;
    }
    /// `frame_num == applied_num`: a duplicate retransmission, re-acked.
    pub fn record_apply_dup(&mut self) {
        self.apply_dup += 1;
    }
    /// Base != applied_num: the body anchors on a state we are not at, so it is
    /// re-acked and dropped. A climbing `basemis` with a frozen model IS the
    /// apply-stall wedge.
    pub fn record_apply_basemis(&mut self) {
        self.apply_basemis += 1;
    }
    /// RFC 0006: a base-checksum mismatch caught a divergent diff base (the base
    /// number matched but the content did not). Counted separately from basemis.
    pub fn record_apply_base_sum_mismatch(&mut self) {
        self.apply_base_sum_mismatch += 1;
    }
    /// The applier surfaced an undecodable body (re-ack and wait for a keyframe).
    pub fn record_apply_reack(&mut self) {
        self.apply_reack += 1;
    }
    /// The applier reported no visible change.
    pub fn record_apply_nochange(&mut self) {
        self.apply_nochange += 1;
    }

    /// Snapshot the apply-path histogram + last-received frame for the dump.
    pub fn apply_snapshot(&self) -> ApplySnapshot {
        ApplySnapshot {
            advanced: self.apply_advanced,
            stale: self.apply_stale,
            dup: self.apply_dup,
            basemis: self.apply_basemis,
            base_sum_mismatch: self.apply_base_sum_mismatch,
            reack: self.apply_reack,
            nochange: self.apply_nochange,
            scrollback_rx: self.frames_scrollback,
            last_rx_num: self.last_rx_num,
            last_rx_base: self.last_rx_base,
            last_rx_body: self.last_rx_body,
        }
    }

    /// Wedge auto-detector: when the visible model (`term_gen`) has not advanced
    /// for `WEDGE_FROZEN_MS` WHILE diff frames keep arriving — the apply-stall
    /// signature, distinct from an idle session where no frames arrive — emit one
    /// `wedge` line carrying the apply histogram and the rejected frame, then
    /// latch until the model advances. Returns whether it emitted (for tests).
    pub fn check_wedge(&mut self, now: u64, term_gen: u64, applied_num: u64, codec: &str) -> bool {
        // Detection is UNCONDITIONAL (#117): a cheap gen/timestamp compare per
        // loop iteration, so the default-on watchdog recovery always has its
        // signal. Only the wedge log line below is gated on `enabled`.
        if term_gen != self.wedge_last_term_gen {
            self.wedge_last_term_gen = term_gen;
            self.wedge_term_gen_since = now;
            self.wedge_diff_at_change = self.frames_diff;
            self.wedge_warned = false;
            return false;
        }
        if self.wedge_warned {
            return false;
        }
        let frozen_ms = now.saturating_sub(self.wedge_term_gen_since);
        let diffs_since = self.frames_diff.saturating_sub(self.wedge_diff_at_change);
        if frozen_ms < WEDGE_FROZEN_MS || diffs_since == 0 {
            return false;
        }
        if self.enabled {
            util::log_write(
                "wedge",
                &format!(
                    "visible model frozen {frozen_ms}ms while {diffs_since} diff frames arrived \
                     (apply-stall) applied_num={applied_num} codec={codec} \
                     last_rx(num={} base={} body={}) \
                     apply(adv={} stale={} dup={} basemis={} reack={} nochange={})",
                    self.last_rx_num,
                    self.last_rx_base,
                    self.last_rx_body.as_str(),
                    self.apply_advanced,
                    self.apply_stale,
                    self.apply_dup,
                    self.apply_basemis,
                    self.apply_reack,
                    self.apply_nochange,
                ),
            );
        }
        self.wedge_warned = true;
        true
    }

    pub fn record_render(&mut self, bytes_out: usize) {
        self.render_writes += 1;
        self.render_bytes_out += bytes_out as u64;
        self.dirty = true;
    }
    pub fn record_render_skip(&mut self) {
        self.render_skipped_idle += 1;
    }

    /// Whether timing is active, so callers can skip `Instant::now` on the hot
    /// path when instrumentation is off (the `let t = enabled().then(...)` idiom).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether the wedge watchdog is armed (default ON, #117), so the client
    /// acts (forensics + resync) on a `check_wedge` fire.
    pub fn wedge_watchdog(&self) -> bool {
        self.wedge_watchdog
    }

    /// Whether POSH_WEDGE_WATCHDOG was EXPLICITLY set on — the debug-posture
    /// signal for CAP_DIAG negotiation, distinct from the default-on recovery.
    pub fn wedge_watchdog_explicit(&self) -> bool {
        self.wedge_watchdog_explicit
    }

    /// Client: accumulate one apply_frame re-parse (full-dump `term.process`).
    pub fn record_apply_us(&mut self, us: u64) {
        self.apply_us_total += us;
        self.apply_count += 1;
        self.apply_us_max = self.apply_us_max.max(us);
        self.last_apply_us = us;
    }
    /// Client: accumulate one compose_frame render (snapshot + `new_frame` diff).
    pub fn record_compose_us(&mut self, us: u64) {
        self.compose_us_total += us;
        self.compose_count += 1;
        self.compose_us_max = self.compose_us_max.max(us);
        self.last_compose_us = us;
    }

    /// Client: one keystroke→consumed round-trip, in milliseconds (the input
    /// byte's outbox-queue time to the frame whose `input_ack` covered it).
    pub fn record_input_ms(&mut self, ms: u64) {
        self.input_ms_total += ms;
        self.input_count += 1;
        self.input_ms_max = self.input_ms_max.max(ms);
    }

    /// One event-loop iteration: `idle_us` blocked in poll, `busy_us` doing
    /// work. Cheap accumulation; the caller gates the `Instant`s on `enabled()`.
    /// Does not mark `dirty` — the loop spins constantly, so loop stats ride out
    /// on whatever frame activity triggers a flush rather than forcing one.
    pub fn record_loop_iter(&mut self, busy_us: u64, idle_us: u64) {
        self.loop_iters += 1;
        self.loop_busy_us += busy_us;
        self.loop_idle_us += idle_us;
        self.loop_busy_us_max = self.loop_busy_us_max.max(busy_us);
        self.last_loop_busy_us = busy_us;
        self.last_loop_idle_us = idle_us;
    }

    /// A frame the server sent as a diff: record the full-dump size it would
    /// otherwise have cost and the bytes the diff saved.
    pub fn record_diff_frame(&mut self, full_len: usize, diff_len: usize) {
        self.full_bytes_considered += full_len as u64;
        self.diff_saved_bytes += full_len.saturating_sub(diff_len) as u64;
    }
    /// A frame the server sent as a full dump despite a diff being available
    /// (the diff wasn't smaller): it saved nothing but still counts toward the
    /// denominator so the percentage isn't flattering.
    pub fn record_full_frame(&mut self, full_len: usize) {
        self.full_bytes_considered += full_len as u64;
    }
    pub fn record_retransmit(&mut self) {
        self.retransmits += 1;
        self.dirty = true;
    }

    /// Times `f` (the `dump_vt()` call) and accumulates its cost in
    /// microseconds. When not instrumenting (neither logging nor a GP species
    /// active) the closure runs untouched. Gated on `instrument()` rather than
    /// `enabled` so the server-forwarded `dump_vt_us` metric terminal is live
    /// for a GP client even with POSH_DEBUG_LOG off (RFC 0007 §3).
    pub fn time_dump_vt<F: FnOnce() -> Vec<u8>>(&mut self, f: F) -> Vec<u8> {
        if !self.instrument() {
            return f();
        }
        let t = Instant::now();
        let out = f();
        let us = t.elapsed().as_micros() as u64;
        self.dump_vt_us_total += us;
        self.dump_vt_count += 1;
        self.dump_vt_us_max = self.dump_vt_us_max.max(us);
        self.last_dump_vt_us = us;
        out
    }

    /// Fraction of considered full-dump bytes avoided by diffing, as a whole
    /// percent. Zero when no frames have been sent yet.
    pub fn diff_saved_pct(&self) -> u64 {
        (self.diff_saved_bytes * 100)
            .checked_div(self.full_bytes_considered)
            .unwrap_or(0)
    }

    fn avg_dump_vt_us(&self) -> u64 {
        self.dump_vt_us_total.checked_div(self.dump_vt_count).unwrap_or(0)
    }

    fn avg_apply_us(&self) -> u64 {
        self.apply_us_total.checked_div(self.apply_count).unwrap_or(0)
    }

    fn avg_input_ms(&self) -> u64 {
        self.input_ms_total.checked_div(self.input_count).unwrap_or(0)
    }

    fn avg_compose_us(&self) -> u64 {
        self.compose_us_total.checked_div(self.compose_count).unwrap_or(0)
    }

    // --- flushing ------------------------------------------------------------

    /// Whether a periodic flush is due: enabled, something changed, and at
    /// least `FLUSH_INTERVAL_MS` since the last emit.
    fn should_flush(&self, now: u64) -> bool {
        self.enabled && self.dirty && now.saturating_sub(self.last_flush) >= FLUSH_INTERVAL_MS
    }

    fn bandwidth(&self, now: u64, bytes: u64, last: u64) -> f64 {
        let dt = now.saturating_sub(self.last_flush).max(1);
        bytes.saturating_sub(last) as f64 * 1000.0 / dt as f64
    }

    fn mark_flushed(&mut self, now: u64, bytes_rx: u64, bytes_tx: u64) {
        self.dirty = false;
        self.last_flush = now;
        self.last_bytes_rx = bytes_rx;
        self.last_bytes_tx = bytes_tx;
        // Windowed timing resets so each line reflects only its own interval
        // (a spike shows up in the second it happened, not the running average).
        self.apply_us_total = 0;
        self.apply_count = 0;
        self.apply_us_max = 0;
        self.compose_us_total = 0;
        self.compose_count = 0;
        self.compose_us_max = 0;
        self.dump_vt_us_total = 0;
        self.dump_vt_count = 0;
        self.dump_vt_us_max = 0;
        self.loop_iters = 0;
        self.loop_busy_us = 0;
        self.loop_idle_us = 0;
        self.loop_busy_us_max = 0;
        self.input_ms_total = 0;
        self.input_count = 0;
        self.input_ms_max = 0;
    }

    /// Share of the window spent doing work vs blocked in poll, as a whole
    /// percent. The high-CPU / stall signal: ~0% is a healthy idle loop, a high
    /// value with a large `max_iter_us` is a stall or a hot path.
    fn loop_busy_pct(&self) -> u64 {
        let total = self.loop_busy_us + self.loop_idle_us;
        (self.loop_busy_us * 100).checked_div(total).unwrap_or(0)
    }

    /// Periodic client summary; no-op unless a flush is due.
    #[allow(clippy::too_many_arguments)]
    pub fn flush_client(
        &mut self,
        now: u64,
        srtt: f64,
        rto: u64,
        send_interval: u64,
        predict: PredictSample,
        srtt_trig: bool,
        bytes_rx: u64,
        bytes_tx: u64,
    ) {
        if self.should_flush(now) {
            self.emit_client(
                "client", now, srtt, rto, send_interval, predict, srtt_trig, bytes_rx, bytes_tx,
            );
        }
    }

    /// Final client summary on loop exit; emitted whenever enabled.
    #[allow(clippy::too_many_arguments)]
    pub fn final_client(
        &mut self,
        now: u64,
        srtt: f64,
        rto: u64,
        send_interval: u64,
        predict: PredictSample,
        srtt_trig: bool,
        bytes_rx: u64,
        bytes_tx: u64,
    ) {
        if self.enabled {
            self.emit_client(
                "client final", now, srtt, rto, send_interval, predict, srtt_trig, bytes_rx,
                bytes_tx,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_client(
        &mut self,
        label: &str,
        now: u64,
        srtt: f64,
        rto: u64,
        send_interval: u64,
        predict: PredictSample,
        srtt_trig: bool,
        bytes_rx: u64,
        bytes_tx: u64,
    ) {
        let bw_down = self.bandwidth(now, bytes_rx, self.last_bytes_rx);
        util::log_write(
            "stats",
            &format!(
                "{label} srtt={srtt:.0}ms rto={rto}ms send_int={send_interval}ms \
                 frames rx={} (full={} diff={} empty={}) bytes_rx={bytes_rx} bw_down={} \
                 predict active={} shown={} epoch_lag={} resets={} \
                 correct={} nocredit={} incorrect={} srtt_trig={} \
                 nocredit_by(unk={} blank={} matched={}) \
                 render writes={} bytes_out={} skipped_idle={} \
                 apply_us={}/{} compose_us={}/{} input_ms={}/{} \
                 loop iters={} busy={}us idle={}us busy_pct={}% max_iter_us={} \
                 apply(adv={} stale={} dup={} basemis={} reack={} nochange={}) sb_rx={}",
                self.frames_total,
                self.frames_full,
                self.frames_diff,
                self.frames_empty,
                human_rate(bw_down),
                predict.active as u8,
                predict.shown,
                predict.epoch_lag,
                predict.resets,
                predict.correct,
                predict.nocredit,
                predict.incorrect,
                srtt_trig as u8,
                predict.nocredit_unknown,
                predict.nocredit_blank,
                predict.nocredit_matched,
                self.render_writes,
                self.render_bytes_out,
                self.render_skipped_idle,
                self.avg_apply_us(),
                self.apply_us_max,
                self.avg_compose_us(),
                self.compose_us_max,
                self.avg_input_ms(),
                self.input_ms_max,
                self.loop_iters,
                self.loop_busy_us,
                self.loop_idle_us,
                self.loop_busy_pct(),
                self.loop_busy_us_max,
                self.apply_advanced,
                self.apply_stale,
                self.apply_dup,
                self.apply_basemis,
                self.apply_reack,
                self.apply_nochange,
                self.frames_scrollback,
            ),
        );
        self.mark_flushed(now, bytes_rx, bytes_tx);
    }

    /// Periodic server summary; no-op unless a flush is due.
    pub fn flush_server(&mut self, now: u64, srtt: f64, rto: u64, outstanding: usize, bytes_tx: u64) {
        if self.should_flush(now) {
            self.emit_server("server", now, srtt, rto, outstanding, bytes_tx);
        }
    }

    /// Final server summary on loop exit; emitted whenever enabled.
    pub fn final_server(&mut self, now: u64, srtt: f64, rto: u64, outstanding: usize, bytes_tx: u64) {
        if self.enabled {
            self.emit_server("server final", now, srtt, rto, outstanding, bytes_tx);
        }
    }

    fn emit_server(
        &mut self,
        label: &str,
        now: u64,
        srtt: f64,
        rto: u64,
        outstanding: usize,
        bytes_tx: u64,
    ) {
        let bw_up = self.bandwidth(now, bytes_tx, self.last_bytes_tx);
        util::log_write(
            "stats",
            &format!(
                "{label} srtt={srtt:.0}ms rto={rto}ms frames tx={} (full={} diff={} empty={}) \
                 diff_saved={}% dump_vt_us={}/{} bytes_tx={bytes_tx} bw_up={} outstanding={outstanding} \
                 retransmit={} \
                 loop iters={} busy={}us idle={}us busy_pct={}% max_iter_us={}",
                self.frames_total,
                self.frames_full,
                self.frames_diff,
                self.frames_empty,
                self.diff_saved_pct(),
                self.avg_dump_vt_us(),
                self.dump_vt_us_max,
                human_rate(bw_up),
                self.retransmits,
                self.loop_iters,
                self.loop_busy_us,
                self.loop_idle_us,
                self.loop_busy_pct(),
                self.loop_busy_us_max,
            ),
        );
        // bytes_rx is unused server-side; reuse the rx slot to track tx deltas.
        self.mark_flushed(now, self.last_bytes_rx, bytes_tx);
    }
}

/// Human-readable byte rate, e.g. "4.1KB/s" / "512B/s" / "2.3MB/s".
fn human_rate(bytes_per_sec: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if bytes_per_sec >= MB {
        format!("{:.1}MB/s", bytes_per_sec / MB)
    } else if bytes_per_sec >= KB {
        format!("{:.1}KB/s", bytes_per_sec / KB)
    } else {
        format!("{:.0}B/s", bytes_per_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An enabled collector without touching the env or a real log file:
    /// `log_write` drops silently when the logger is uninitialized, so emits
    /// are exercised for their state transitions without producing output.
    fn enabled_stats() -> Stats {
        Stats {
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn frame_classification_counts() {
        let mut s = enabled_stats();
        s.record_frame_full();
        s.record_frame_diff();
        s.record_frame_diff();
        s.record_frame_empty();
        s.record_frame_scrollback();
        assert_eq!(s.frames_total, 4, "scrollback stays out of the full/diff/empty total");
        assert_eq!(s.frames_full, 1);
        assert_eq!(s.frames_diff, 2);
        assert_eq!(s.frames_empty, 1);
        assert_eq!(s.frames_scrollback, 1);
    }

    #[test]
    fn late_gap_threshold_mirrors_the_banner() {
        // The stats "late gap" counter must key on the SAME silence threshold the
        // "Last contact" banner uses, or the two disagree about when a
        // disconnect was perceived. This guards the local mirror against drift.
        assert_eq!(LATE_GAP_MS, posh_proto::display::SERVER_LATE_AFTER);
    }

    #[test]
    fn frame_arrival_tracks_max_and_late_gaps() {
        let mut s = enabled_stats();
        // First arrival only seeds the baseline — no gap to measure.
        s.record_frame_arrival(1_000);
        assert_eq!(s.frame_gap_ms_max, 0);
        assert_eq!(s.frame_gaps_late, 0);
        // A short gap (heartbeat cadence) updates the max but is not "late".
        s.record_frame_arrival(4_000); // +3000ms
        assert_eq!(s.frame_gap_ms_max, 3_000);
        assert_eq!(s.frame_gaps_late, 0);
        // A gap past the banner threshold counts as a late (would-be-banner) gap.
        s.record_frame_arrival(4_000 + LATE_GAP_MS + 1);
        assert_eq!(s.frame_gap_ms_max, LATE_GAP_MS + 1);
        assert_eq!(s.frame_gaps_late, 1);
        // A gap exactly at the threshold does NOT trip it (banner uses strict >).
        let base = s.last_frame_arrival;
        s.record_frame_arrival(base + LATE_GAP_MS);
        assert_eq!(s.frame_gaps_late, 1, "a gap == threshold is not late");
        // The snapshot reflects the accumulated gauges; heartbeats_rx == empties.
        s.record_frame_empty();
        let snap = s.link_snapshot();
        assert_eq!(snap.frame_gap_ms_max, LATE_GAP_MS + 1);
        assert_eq!(snap.frame_gaps_late, 1);
        assert_eq!(snap.heartbeats_rx, 1);
    }

    #[test]
    fn wedge_detector_fires_on_frozen_model_with_arriving_diffs() {
        let mut s = enabled_stats();
        // Model advances to gen 5 at t=0: arms the freeze baseline.
        assert!(!s.check_wedge(0, 5, 100, "morph"));
        // Diffs keep arriving while the visible model stays at gen 5...
        s.record_frame_diff();
        s.record_frame_diff();
        // ...not yet past the freeze threshold.
        assert!(!s.check_wedge(WEDGE_FROZEN_MS - 1, 5, 100, "morph"));
        // Past the threshold with diffs since the freeze began: fires once.
        assert!(s.check_wedge(WEDGE_FROZEN_MS, 5, 100, "morph"));
        // Latched: does not re-fire while the model stays frozen.
        assert!(!s.check_wedge(WEDGE_FROZEN_MS + 5000, 5, 100, "morph"));
        // The model advances: re-arms (no immediate re-fire).
        assert!(!s.check_wedge(WEDGE_FROZEN_MS + 5000, 6, 100, "morph"));
    }

    #[test]
    fn wedge_detector_ignores_idle_model_with_no_diffs() {
        let mut s = enabled_stats();
        assert!(!s.check_wedge(0, 5, 100, "morph"));
        // No diffs arrive: a long-frozen model is just an idle session, not a
        // wedge — must not warn no matter how long it sits.
        assert!(!s.check_wedge(WEDGE_FROZEN_MS * 100, 5, 100, "morph"));
    }

    #[test]
    fn wedge_detection_is_unconditional() {
        // #117: detection runs even with logging AND the watchdog off — the
        // return value is the recovery signal, and the caller owns the gating.
        // (Only the log line is `enabled`-gated.)
        let mut s = Stats::default(); // enabled = false, watchdog = false
        assert!(!s.check_wedge(0, 5, 100, "morph"));
        s.record_frame_diff();
        assert!(
            s.check_wedge(WEDGE_FROZEN_MS, 5, 100, "morph"),
            "fires with everything off"
        );
    }

    #[test]
    fn wedge_watchdog_detects_even_without_logging() {
        // #8: with the watchdog armed but POSH_DEBUG_LOG off, the apply-stall
        // fingerprint still fires (so the client can auto-recover).
        let mut s = Stats {
            wedge_watchdog: true,
            ..Default::default()
        };
        assert!(!s.check_wedge(0, 5, 100, "dumpdiff"));
        s.record_frame_diff();
        assert!(s.check_wedge(WEDGE_FROZEN_MS, 5, 100, "dumpdiff"), "watchdog fires");
        assert!(
            !s.check_wedge(WEDGE_FROZEN_MS + 1000, 5, 100, "dumpdiff"),
            "latched until the model advances",
        );
    }

    #[test]
    fn diff_saved_percentage() {
        let mut s = enabled_stats();
        assert_eq!(s.diff_saved_pct(), 0, "no frames yet");
        // One diff that saved 80 of a 100-byte dump, one full dump (saved 0).
        s.record_diff_frame(100, 20);
        s.record_full_frame(100);
        assert_eq!(s.diff_saved_bytes, 80);
        assert_eq!(s.full_bytes_considered, 200);
        assert_eq!(s.diff_saved_pct(), 40);
    }

    #[test]
    fn avg_dump_vt_handles_empty() {
        let s = enabled_stats();
        assert_eq!(s.avg_dump_vt_us(), 0);
    }

    #[test]
    fn flush_cadence_gating() {
        let mut s = enabled_stats();
        // Nothing changed yet: not due even past the interval.
        assert!(!s.should_flush(10_000));
        // A counter change marks dirty, but the interval hasn't elapsed.
        s.record_frame_full();
        assert!(!s.should_flush(FLUSH_INTERVAL_MS - 1));
        // Past the interval with pending changes: due.
        assert!(s.should_flush(FLUSH_INTERVAL_MS));
    }

    #[test]
    fn skipped_render_does_not_arm_a_flush() {
        let mut s = enabled_stats();
        s.record_render_skip();
        // Idle-only activity must not make a flush due, or quiescent sessions
        // would log every interval.
        assert!(!s.should_flush(10_000));
        assert_eq!(s.render_skipped_idle, 1);
    }

    #[test]
    fn disabled_collector_never_flushes() {
        let s = Stats::default(); // enabled = false
        assert!(!s.should_flush(10_000));
    }

    #[test]
    fn emit_resets_dirty_and_advances_window() {
        let mut s = enabled_stats();
        s.record_frame_full();
        s.emit_client(
            "client",
            2000,
            50.0,
            200,
            30,
            PredictSample::default(),
            false,
            4096,
            1024,
        );
        assert!(!s.dirty, "emit clears the dirty flag");
        assert_eq!(s.last_flush, 2000);
        assert_eq!(s.last_bytes_rx, 4096);
        assert_eq!(s.last_bytes_tx, 1024);
        assert!(!s.should_flush(2000 + FLUSH_INTERVAL_MS), "no new changes after emit");
    }

    #[test]
    fn human_rate_units() {
        assert_eq!(human_rate(512.0), "512B/s");
        assert_eq!(human_rate(2048.0), "2.0KB/s");
        assert_eq!(human_rate(3.0 * 1024.0 * 1024.0), "3.0MB/s");
    }

    #[test]
    fn emits_keyed_lines_through_the_real_log_sink() {
        // End-to-end path: util::log_init -> emit_* -> the rotating file sink,
        // proving the format strings and log_write wiring actually land on
        // disk. This is the only unit test that initializes the per-process
        // LOGGER; it asserts substrings, so concurrent emits from sibling tests
        // (which append) cannot invalidate it.
        let path =
            std::env::temp_dir().join(format!("posh-stats-test-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        util::log_init(&path).unwrap();

        let mut c = enabled_stats();
        c.record_frame_diff();
        c.record_apply_us(40);
        c.record_apply_us(80); // a slower frame: windowed avg 60, max 80
        c.record_compose_us(60);
        c.record_input_ms(12);
        c.record_input_ms(34); // keystroke latency: avg 23, max 34
        c.record_loop_iter(100, 900); // 100us busy, 900us idle => 10% busy
        c.emit_client(
            "client",
            1000,
            42.0,
            200,
            21,
            PredictSample {
                active: true,
                shown: 1,
                ..Default::default()
            },
            false,
            8192,
            256,
        );

        let mut s = enabled_stats();
        s.record_frame_full();
        s.record_diff_frame(100, 20);
        s.record_loop_iter(50, 950); // 5% busy
        s.emit_server("server", 2000, 42.0, 200, 3, 4096);

        let body = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        for key in [
            "client srtt=42ms",
            "frames rx=1 (full=0 diff=1",
            "bw_down=",
            "predict active=1 shown=1 epoch_lag=0 resets=0 correct=0 nocredit=0 incorrect=0 srtt_trig=0",
            "nocredit_by(unk=0 blank=0 matched=0)",
            "apply_us=60/80", // windowed avg / max
            "compose_us=60/60",
            "input_ms=23/34",
            "loop iters=1 busy=100us idle=900us busy_pct=10% max_iter_us=100",
            "apply(adv=0 stale=0 dup=0 basemis=0 reack=0 nochange=0) sb_rx=0",
        ] {
            assert!(body.contains(key), "missing client key {key:?} in:\n{body}");
        }
        for key in [
            "server srtt=42ms",
            "diff_saved=80%",
            "dump_vt_us=",
            "bw_up=",
            "loop iters=1 busy=50us idle=950us busy_pct=5% max_iter_us=50",
        ] {
            assert!(body.contains(key), "missing server key {key:?} in:\n{body}");
        }
    }
}
