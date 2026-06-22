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

#[derive(Default)]
pub struct Stats {
    enabled: bool,
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
}

impl Stats {
    /// Reads `$POSH_DEBUG_LOG`; on a non-empty path that `log_init` accepts the
    /// collector is enabled, otherwise it is an inert no-op.
    pub fn new() -> Stats {
        let enabled = match std::env::var_os("POSH_DEBUG_LOG") {
            Some(p) if !p.is_empty() => util::log_init(Path::new(&p)).is_ok(),
            _ => false,
        };
        Stats {
            enabled,
            last_flush: now_ms(),
            ..Default::default()
        }
    }

    /// Flip the collector on/off at runtime (the `Ctrl-^ d` debug-logging
    /// toggle). When off, the periodic flush is skipped even if a sink is open.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
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

    /// Client: accumulate one apply_frame re-parse (full-dump `term.process`).
    pub fn record_apply_us(&mut self, us: u64) {
        self.apply_us_total += us;
        self.apply_count += 1;
        self.apply_us_max = self.apply_us_max.max(us);
    }
    /// Client: accumulate one compose_frame render (snapshot + `new_frame` diff).
    pub fn record_compose_us(&mut self, us: u64) {
        self.compose_us_total += us;
        self.compose_count += 1;
        self.compose_us_max = self.compose_us_max.max(us);
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
    /// microseconds. When disabled the closure runs untouched.
    pub fn time_dump_vt<F: FnOnce() -> Vec<u8>>(&mut self, f: F) -> Vec<u8> {
        if !self.enabled {
            return f();
        }
        let t = Instant::now();
        let out = f();
        let us = t.elapsed().as_micros() as u64;
        self.dump_vt_us_total += us;
        self.dump_vt_count += 1;
        self.dump_vt_us_max = self.dump_vt_us_max.max(us);
        out
    }

    /// Fraction of considered full-dump bytes avoided by diffing, as a whole
    /// percent. Zero when no frames have been sent yet.
    pub fn diff_saved_pct(&self) -> u64 {
        if self.full_bytes_considered == 0 {
            0
        } else {
            self.diff_saved_bytes * 100 / self.full_bytes_considered
        }
    }

    fn avg_dump_vt_us(&self) -> u64 {
        if self.dump_vt_count == 0 {
            0
        } else {
            self.dump_vt_us_total / self.dump_vt_count
        }
    }

    fn avg_apply_us(&self) -> u64 {
        if self.apply_count == 0 {
            0
        } else {
            self.apply_us_total / self.apply_count
        }
    }

    fn avg_input_ms(&self) -> u64 {
        if self.input_count == 0 {
            0
        } else {
            self.input_ms_total / self.input_count
        }
    }

    fn avg_compose_us(&self) -> u64 {
        if self.compose_count == 0 {
            0
        } else {
            self.compose_us_total / self.compose_count
        }
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
        if total == 0 {
            0
        } else {
            self.loop_busy_us * 100 / total
        }
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
                 render writes={} bytes_out={} skipped_idle={} \
                 apply_us={}/{} compose_us={}/{} input_ms={}/{} \
                 loop iters={} busy={}us idle={}us busy_pct={}% max_iter_us={}",
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
        assert_eq!(s.frames_total, 4);
        assert_eq!(s.frames_full, 1);
        assert_eq!(s.frames_diff, 2);
        assert_eq!(s.frames_empty, 1);
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
            "apply_us=60/80", // windowed avg / max
            "compose_us=60/60",
            "input_ms=23/34",
            "loop iters=1 busy=100us idle=900us busy_pct=10% max_iter_us=100",
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
