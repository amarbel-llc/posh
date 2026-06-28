//! RFC 0007 §2: the metric vector the evolved predictor species consume.
//!
//! Every terminal is an `f64` so the GP-DAG operates over a flat numeric input
//! vector. Categorical signals (frontmost-app identity) are hashed to a stable
//! numeric id *before* they reach here; structured signals (process trees) are
//! reduced to scalar features. Encoding conventions for the scalar terminals:
//!
//! - `NaN` = the terminal is momentarily *unavailable* (no source has reported);
//!   the evolved program treats `NaN` propagation as non-fatal (the genome
//!   simply scores poorly), never as a panic.
//! - boolean state is `0.0` / `1.0`.
//! - a color is `-1.0` for the terminal default, else packed 24-bit RGB
//!   (`r<<16 | g<<8 | b`); `Default` is a real state, distinct from `NaN`.
//!
//! The field order below IS the schema-versioned contract: a persisted genome's
//! leaf indices reference [`MetricVector::to_terminals`] positions, so the order
//! MUST NOT change within a [`METRIC_SCHEMA_VERSION`]. Adding terminals is a
//! version bump with a migration path (RFC 0007 §8); new terminals are appended
//! so prior leaf indices stay valid.

// Scaffold contract surface (RFC 0007 §2/§3): the transport / server-forwarded /
// host terminals and the gatherer entry points are referenced once the full
// metric bus and the mephisto genome are wired. Allow until then.
#![allow(dead_code)]

use posh_term::{Color, UnderlineStyle};

use crate::remote::display::Snapshot;

/// The metric-vector schema version. Bumped whenever the terminal set in
/// [`MetricVector`] changes; persisted genomes are tagged with it (RFC 0007 §8)
/// and rejected/migrated on mismatch.
///
/// v2 (this revision) appends the screen-state terminals (geometry, cursor, the
/// SGR pen at the cursor, and the DEC modes) to v1's transport/host set.
pub const METRIC_SCHEMA_VERSION: u32 = 2;

/// Number of terminals in the current schema. Equals the field count of
/// [`MetricVector`] and the length of [`MetricVector::to_terminals`].
pub const TERMINAL_COUNT: usize = 48;

/// Sentinel for a `Color::Default` terminal (distinct from `NaN`-unavailable).
pub const COLOR_DEFAULT: f64 = -1.0;

/// RFC 0007 §2 terminal set. See the module docs for the scalar encodings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetricVector {
    // --- Transport health (client-measured; `datagram.rs` / `stats.rs`) ---
    pub srtt_ms: f64,
    pub rto_ms: f64,
    pub send_interval_ms: f64,
    pub retransmit_rate: f64,
    pub outstanding: f64,
    pub bw_up_bps: f64,
    // --- Client render headroom (derived from loop-iter timings) ---
    pub fps: f64,
    pub loop_busy_frac: f64,
    pub apply_us: f64,
    pub compose_us: f64,
    pub dump_vt_us: f64,
    // --- Predictor self-feedback (recent window) ---
    pub pred_correct_rate: f64,
    pub pred_nocredit_rate: f64,
    pub pred_incorrect_rate: f64,
    pub epoch_lag: f64,
    // --- Terminal/session state (already client-side: alt-screen from the
    // reconstructed server_term, ECHO from the per-frame FLAG_ECHO bit) ---
    pub alt_screen: f64,
    pub echo_flag: f64,
    // --- Host stats (both hosts) ---
    pub local_load1: f64,
    pub remote_load1: f64,
    pub local_mem_avail_frac: f64,
    pub remote_mem_avail_frac: f64,
    // --- Frontmost app / process tree (both hosts; categorical pre-hashed) ---
    pub local_frontmost_app: f64,
    pub remote_frontmost_app: f64,
    pub remote_proc_count: f64,
    pub remote_fg_proc_id: f64,
    // === v2: screen state, read client-side from the displayed Snapshot ===
    // --- Geometry & cursor ---
    pub screen_rows: f64,
    pub screen_cols: f64,
    pub cursor_row: f64,
    pub cursor_col: f64,
    pub cursor_visible: f64,
    // --- SGR pen at the cursor cell ("current styling") ---
    pub pen_fg: f64,
    pub pen_bg: f64,
    pub pen_bold: f64,
    pub pen_dim: f64,
    pub pen_italic: f64,
    pub pen_underline: f64,
    pub pen_blink: f64,
    pub pen_inverse: f64,
    pub pen_invisible: f64,
    pub pen_strikethrough: f64,
    // --- DEC modes ---
    pub reverse_video: f64,
    pub bracketed_paste: f64,
    pub focus_reporting: f64,
    pub alternate_scroll: f64,
    pub app_cursor_keys: f64,
    pub app_keypad: f64,
    pub mouse_mode: f64,
    pub mouse_encoding: f64,
}

impl MetricVector {
    /// A vector with every terminal unavailable (`NaN`). The neutral starting
    /// value before any source has reported (RFC 0007 §2).
    pub fn unavailable() -> MetricVector {
        MetricVector {
            srtt_ms: f64::NAN,
            rto_ms: f64::NAN,
            send_interval_ms: f64::NAN,
            retransmit_rate: f64::NAN,
            outstanding: f64::NAN,
            bw_up_bps: f64::NAN,
            fps: f64::NAN,
            loop_busy_frac: f64::NAN,
            apply_us: f64::NAN,
            compose_us: f64::NAN,
            dump_vt_us: f64::NAN,
            pred_correct_rate: f64::NAN,
            pred_nocredit_rate: f64::NAN,
            pred_incorrect_rate: f64::NAN,
            epoch_lag: f64::NAN,
            alt_screen: f64::NAN,
            echo_flag: f64::NAN,
            local_load1: f64::NAN,
            remote_load1: f64::NAN,
            local_mem_avail_frac: f64::NAN,
            remote_mem_avail_frac: f64::NAN,
            local_frontmost_app: f64::NAN,
            remote_frontmost_app: f64::NAN,
            remote_proc_count: f64::NAN,
            remote_fg_proc_id: f64::NAN,
            screen_rows: f64::NAN,
            screen_cols: f64::NAN,
            cursor_row: f64::NAN,
            cursor_col: f64::NAN,
            cursor_visible: f64::NAN,
            pen_fg: f64::NAN,
            pen_bg: f64::NAN,
            pen_bold: f64::NAN,
            pen_dim: f64::NAN,
            pen_italic: f64::NAN,
            pen_underline: f64::NAN,
            pen_blink: f64::NAN,
            pen_inverse: f64::NAN,
            pen_invisible: f64::NAN,
            pen_strikethrough: f64::NAN,
            reverse_video: f64::NAN,
            bracketed_paste: f64::NAN,
            focus_reporting: f64::NAN,
            alternate_scroll: f64::NAN,
            app_cursor_keys: f64::NAN,
            app_keypad: f64::NAN,
            mouse_mode: f64::NAN,
            mouse_encoding: f64::NAN,
        }
    }

    /// Fill the client-side SCREEN-STATE terminals (geometry, cursor, the SGR
    /// pen at the cursor cell, and the DEC modes) from the displayed snapshot
    /// (RFC 0007 §3). Transport / predictor / server-forwarded / host terminals
    /// are left untouched — the caller fills those from their own sources.
    pub fn fill_screen_state(&mut self, snap: &Snapshot) {
        self.screen_rows = snap.rows as f64;
        self.screen_cols = snap.cols as f64;
        self.cursor_row = snap.cursor_row as f64;
        self.cursor_col = snap.cursor_col as f64;
        self.cursor_visible = bool_terminal(snap.cursor_visible);

        // The pen is the SGR style of the cell currently under the cursor. If
        // the cursor cell is somehow out of range the pen terminals stay NaN.
        if let Some(cell) = snap.cell(snap.cursor_row, snap.cursor_col) {
            let s = &cell.style;
            self.pen_fg = color_terminal(s.fg);
            self.pen_bg = color_terminal(s.bg);
            self.pen_bold = bool_terminal(s.bold);
            self.pen_dim = bool_terminal(s.dim);
            self.pen_italic = bool_terminal(s.italic);
            self.pen_underline = underline_terminal(s.underline);
            self.pen_blink = bool_terminal(s.blink);
            self.pen_inverse = bool_terminal(s.inverse);
            self.pen_invisible = bool_terminal(s.invisible);
            self.pen_strikethrough = bool_terminal(s.strikethrough);
        }

        self.reverse_video = bool_terminal(snap.reverse_video);
        self.bracketed_paste = bool_terminal(snap.bracketed_paste);
        self.focus_reporting = bool_terminal(snap.focus_reporting);
        self.alternate_scroll = bool_terminal(snap.alternate_scroll);
        self.app_cursor_keys = bool_terminal(snap.app_cursor_keys);
        self.app_keypad = bool_terminal(snap.app_keypad);
        self.mouse_mode = snap.mouse_mode as f64;
        self.mouse_encoding = snap.mouse_encoding as f64;
    }

    /// Fill the client-measured TRANSPORT terminals (RFC 0007 §3): `srtt`/`rto`/
    /// `send_interval` from [`Connection`](crate::remote::datagram::Connection),
    /// and `outstanding` from the input outbox's unacked length.
    ///
    /// `retransmit_rate` and `bw_up_bps` stay `NaN` here: the retransmit counter
    /// is server-side only (folded into the server-forwarded work), and there is
    /// no upstream-bandwidth estimator yet (deferred).
    pub fn fill_transport(
        &mut self,
        srtt_ms: f64,
        rto_ms: f64,
        send_interval_ms: f64,
        outstanding: f64,
    ) {
        self.srtt_ms = srtt_ms;
        self.rto_ms = rto_ms;
        self.send_interval_ms = send_interval_ms;
        self.outstanding = outstanding;
    }

    /// Fill the client RENDER-HEADROOM terminals (RFC 0007 §2) from the most
    /// recent event-loop iteration and frame compute costs. `dump_vt_us` is a
    /// server-side cost and is not set here (it stays `NaN` client-side).
    pub fn fill_render_headroom(
        &mut self,
        fps: f64,
        loop_busy_frac: f64,
        apply_us: f64,
        compose_us: f64,
    ) {
        self.fps = fps;
        self.loop_busy_frac = loop_busy_frac;
        self.apply_us = apply_us;
        self.compose_us = compose_us;
    }

    /// Fill the session-gate terminals (RFC 0007 §2). Both are available
    /// client-side without forwarding: `alt_screen` from the reconstructed
    /// authoritative server terminal, `echo_flag` from the per-frame `FLAG_ECHO`
    /// runtime bit.
    pub fn fill_session_gate(&mut self, alt_screen: bool, echo_flag: bool) {
        self.alt_screen = bool_terminal(alt_screen);
        self.echo_flag = bool_terminal(echo_flag);
    }

    /// Fill the server-forwarded terminals from a decoded `CAP_METRICS` payload
    /// (RFC 0007 §3), in `caps::METRICS_FIELDS` order. The first five are the
    /// remote host/app/proc terminals; the last two are the server-side costs
    /// the client cannot measure locally — `retransmit_rate` (the server's
    /// retransmits/sec) and `dump_vt_us` (its most-recent frame-dump cost),
    /// which `fill_transport`/`fill_render_headroom` deliberately leave `NaN`.
    /// Absent values arrive as `NaN` and pass straight through.
    pub fn fill_remote(&mut self, fields: [f64; 7]) {
        let [load1, mem_avail_frac, frontmost_app, proc_count, fg_proc_id, retransmit_rate, dump_vt_us] =
            fields;
        self.remote_load1 = load1;
        self.remote_mem_avail_frac = mem_avail_frac;
        self.remote_frontmost_app = frontmost_app;
        self.remote_proc_count = proc_count;
        self.remote_fg_proc_id = fg_proc_id;
        self.retransmit_rate = retransmit_rate;
        self.dump_vt_us = dump_vt_us;
    }

    /// Fill the predictor SELF-FEEDBACK terminals from the live
    /// [`PredictorStats`]. The rates are lifetime ratios of the cumulative
    /// outcome counters — a proxy for the RFC's "recent window"; windowing them
    /// is a follow-up (TODO RFC 0007 §2). `epoch_lag` is exact.
    pub fn fill_predictor_feedback(&mut self, stats: &super::PredictorStats) {
        let (correct, nocredit, incorrect) = stats.outcomes;
        let total = (correct + nocredit + incorrect) as f64;
        if total > 0.0 {
            self.pred_correct_rate = correct as f64 / total;
            self.pred_nocredit_rate = nocredit as f64 / total;
            self.pred_incorrect_rate = incorrect as f64 / total;
        }
        self.epoch_lag = stats.epoch_lag as f64;
    }

    /// The flat terminal vector the GP program reads. Index order is the
    /// schema-versioned contract (RFC 0007 §2/§8): leaf index `i` is the field
    /// at position `i` here. MUST match [`TERMINAL_COUNT`] and the field order.
    pub fn to_terminals(self) -> [f64; TERMINAL_COUNT] {
        [
            self.srtt_ms,
            self.rto_ms,
            self.send_interval_ms,
            self.retransmit_rate,
            self.outstanding,
            self.bw_up_bps,
            self.fps,
            self.loop_busy_frac,
            self.apply_us,
            self.compose_us,
            self.dump_vt_us,
            self.pred_correct_rate,
            self.pred_nocredit_rate,
            self.pred_incorrect_rate,
            self.epoch_lag,
            self.alt_screen,
            self.echo_flag,
            self.local_load1,
            self.remote_load1,
            self.local_mem_avail_frac,
            self.remote_mem_avail_frac,
            self.local_frontmost_app,
            self.remote_frontmost_app,
            self.remote_proc_count,
            self.remote_fg_proc_id,
            self.screen_rows,
            self.screen_cols,
            self.cursor_row,
            self.cursor_col,
            self.cursor_visible,
            self.pen_fg,
            self.pen_bg,
            self.pen_bold,
            self.pen_dim,
            self.pen_italic,
            self.pen_underline,
            self.pen_blink,
            self.pen_inverse,
            self.pen_invisible,
            self.pen_strikethrough,
            self.reverse_video,
            self.bracketed_paste,
            self.focus_reporting,
            self.alternate_scroll,
            self.app_cursor_keys,
            self.app_keypad,
            self.mouse_mode,
            self.mouse_encoding,
        ]
    }
}

/// Boolean → terminal value (`0.0` / `1.0`).
fn bool_terminal(b: bool) -> f64 {
    if b {
        1.0
    } else {
        0.0
    }
}

/// SGR color → terminal value: [`COLOR_DEFAULT`] for the terminal default, else
/// packed 24-bit RGB (`Indexed` resolves through the default palette).
fn color_terminal(c: Color) -> f64 {
    match c.to_rgb() {
        None => COLOR_DEFAULT,
        Some((r, g, b)) => ((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)) as f64,
    }
}

/// Underline style → ordinal terminal value.
fn underline_terminal(u: UnderlineStyle) -> f64 {
    match u {
        UnderlineStyle::None => 0.0,
        UnderlineStyle::Single => 1.0,
        UnderlineStyle::Double => 2.0,
        UnderlineStyle::Curly => 3.0,
        UnderlineStyle::Dotted => 4.0,
        UnderlineStyle::Dashed => 5.0,
    }
}

/// Produces the current [`MetricVector`] each tick (RFC 0007 §3).
///
/// The client-local terminals are read directly; the server-forwarded terminals
/// (`alt_screen`, `echo_flag`, `remote_*`) arrive over the RFC 0001 per-frame
/// capability channel; the OSC/host/process-tree terminals come from host
/// probes. A real implementation merges all sources into one snapshot.
pub trait MetricSource: Send {
    fn sample(&self) -> MetricVector;
}

/// Gather the currently-wired client-local terminals from the displayed
/// snapshot. The screen-state terminals (RFC 0007 §2 v2) are filled; transport,
/// predictor-feedback, host, and server-forwarded terminals remain `NaN` until
/// their sources are wired (RFC 0007 §3). While they are `NaN` the evolved
/// species fall back to the adaptive shadow (RFC 0007 §7.1), so partial
/// gathering is a safe no-op.
pub fn gather_client_local(snap: &Snapshot) -> MetricVector {
    let mut m = MetricVector::unavailable();
    m.fill_screen_state(snap);
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use posh_term::{Cell, Style};

    #[test]
    fn terminal_count_matches_array_len() {
        assert_eq!(
            MetricVector::unavailable().to_terminals().len(),
            TERMINAL_COUNT
        );
    }

    #[test]
    fn unavailable_is_all_nan() {
        assert!(MetricVector::unavailable()
            .to_terminals()
            .iter()
            .all(|t| t.is_nan()));
    }

    #[test]
    fn transport_and_feedback_terminals_fill_from_their_sources() {
        let mut m = MetricVector::unavailable();
        m.fill_transport(18.5, 120.0, 50.0, 3.0);
        assert_eq!(m.srtt_ms, 18.5);
        assert_eq!(m.rto_ms, 120.0);
        assert_eq!(m.send_interval_ms, 50.0);
        assert_eq!(m.outstanding, 3.0);
        m.fill_render_headroom(60.0, 0.25, 90.0, 140.0);
        assert_eq!(m.fps, 60.0);
        assert_eq!(m.loop_busy_frac, 0.25);
        assert_eq!(m.apply_us, 90.0);
        assert_eq!(m.compose_us, 140.0);
        // dump_vt_us is server-side; not set by fill_render_headroom.
        assert!(m.dump_vt_us.is_nan());

        let stats = super::super::PredictorStats {
            active: true,
            shown_cells: 0,
            epoch_lag: 4,
            mispredict_resets: 0,
            outcomes: (7, 2, 1), // 7 correct, 2 nocredit, 1 incorrect of 10
            nocredit_reasons: (0, 0, 0),
            srtt_trigger: false,
        };
        m.fill_predictor_feedback(&stats);
        assert_eq!(m.pred_correct_rate, 0.7);
        assert_eq!(m.pred_nocredit_rate, 0.2);
        assert!((m.pred_incorrect_rate - 0.1).abs() < 1e-9);
        assert_eq!(m.epoch_lag, 4.0);

        m.fill_session_gate(true, false);
        assert_eq!(m.alt_screen, 1.0);
        assert_eq!(m.echo_flag, 0.0);

        // Untouched terminals (no source wired here) stay NaN.
        assert!(m.bw_up_bps.is_nan());
    }

    #[test]
    fn screen_state_terminals_are_filled_from_the_snapshot() {
        let mut snap = Snapshot::blank(24, 80);
        snap.cursor_row = 3;
        snap.cursor_col = 7;
        snap.bracketed_paste = true;
        snap.mouse_mode = 1000;
        // Style the cursor cell: bold red-on-default.
        let mut styled = Cell::blank(Style::default());
        styled.style.bold = true;
        styled.style.fg = Color::Rgb(255, 0, 0);
        snap.cells[3][7] = styled;

        let m = gather_client_local(&snap);
        assert_eq!(m.screen_rows, 24.0);
        assert_eq!(m.screen_cols, 80.0);
        assert_eq!(m.cursor_row, 3.0);
        assert_eq!(m.cursor_col, 7.0);
        assert_eq!(m.pen_bold, 1.0);
        assert_eq!(m.pen_fg, 0xFF0000 as f64);
        assert_eq!(m.pen_bg, COLOR_DEFAULT);
        assert_eq!(m.bracketed_paste, 1.0);
        assert_eq!(m.mouse_mode, 1000.0);
        // Unwired terminals stay NaN.
        assert!(m.srtt_ms.is_nan());
        assert!(m.alt_screen.is_nan());
    }

    #[test]
    fn remote_terminals_fill_in_cap_metrics_order() {
        // The CAP_METRICS v2 payload order (RFC 0007 §3): five remote host/app/
        // proc terminals, then the two server-side costs.
        let mut m = MetricVector::unavailable();
        m.fill_remote([0.8, 0.6, 111.0, 42.0, 222.0, 12.5, 333.0]);
        assert_eq!(m.remote_load1, 0.8);
        assert_eq!(m.remote_mem_avail_frac, 0.6);
        assert_eq!(m.remote_frontmost_app, 111.0);
        assert_eq!(m.remote_proc_count, 42.0);
        assert_eq!(m.remote_fg_proc_id, 222.0);
        // The two server-side counters land in the terminals fill_transport /
        // fill_render_headroom deliberately leave NaN.
        assert_eq!(m.retransmit_rate, 12.5);
        assert_eq!(m.dump_vt_us, 333.0);
    }
}
