//! The mosh adaptive prediction model (port of mosh's PredictionEngine from
//! terminaloverlay.cc): keystrokes echoed as overlay cells that belong to
//! epochs, displayed according to adaptive RTT/glitch triggers, and confirmed
//! or culled against acknowledged server frames. Drives the Always / Never /
//! Adaptive / Experimental selections.

use crate::remote::display::Snapshot;

use super::overlay::{NoCreditReason, OverlayBuffer, Validity};
use super::{
    PredictionModel, PredictionRenderer, Predictor, PredictorStats, FLAG_TRIGGER_HIGH,
    FLAG_TRIGGER_LOW, GLITCH_FLAG_THRESHOLD, GLITCH_REPAIR_COUNT, GLITCH_REPAIR_MININTERVAL,
    GLITCH_THRESHOLD, SRTT_TRIGGER_HIGH, SRTT_TRIGGER_LOW,
};

pub struct MoshPredictor {
    buf: OverlayBuffer,

    local_frame_acked: u64,
    local_frame_late_acked: u64,

    flagging: bool,      // underline displayed predictions
    srtt_trigger: bool,  // show predictions because of slow RTT
    glitch_trigger: u32, // show predictions because one took too long
    last_quick_confirmation: u64,
    send_interval: u64,

    last_height: u16,
    last_width: u16,

    display_preference: PredictionModel,

    /// Cumulative count of misprediction resets (a prediction validated wrong
    /// against a server frame, wiping the whole overlay). Instrumentation only.
    mispredict_resets: u64,
    /// Cumulative per-cell validation outcomes (instrumentation only). Only
    /// `pred_correct` advances `confirmed_epoch`; the split tells confirmation
    /// failure (nocredit dominates) from thrash (incorrect dominates).
    pred_correct: u64,
    pred_nocredit: u64,
    pred_incorrect: u64,
    /// `pred_nocredit` split by cause (#predict-echo): which branch of
    /// `get_validity` denied credit. `matched` dominating points at typing along
    /// content already on screen (autosuggestions / a TUI's own echo); `unknown`
    /// at cursor-cell churn; `blank` at blank predictions.
    pred_nocredit_unknown: u64,
    pred_nocredit_blank: u64,
    pred_nocredit_matched: u64,
}

impl MoshPredictor {
    /// `display_preference` must be one of Always / Never / Adaptive /
    /// Experimental (optimistic is its own model).
    pub fn new(display_preference: PredictionModel, predict_overwrite: bool) -> MoshPredictor {
        let bump_epoch_on_tentative = display_preference != PredictionModel::Experimental;
        MoshPredictor {
            buf: OverlayBuffer::new(predict_overwrite, bump_epoch_on_tentative),
            local_frame_acked: 0,
            local_frame_late_acked: 0,
            flagging: false,
            srtt_trigger: false,
            glitch_trigger: 0,
            last_quick_confirmation: 0,
            send_interval: 250,
            last_height: 0,
            last_width: 0,
            display_preference,
            mispredict_resets: 0,
            pred_correct: 0,
            pred_nocredit: 0,
            pred_incorrect: 0,
            pred_nocredit_unknown: 0,
            pred_nocredit_blank: 0,
            pred_nocredit_matched: 0,
        }
    }

    fn shown(&self) -> bool {
        match self.display_preference {
            PredictionModel::Never => false,
            PredictionModel::Always | PredictionModel::Experimental => true,
            PredictionModel::Adaptive => self.srtt_trigger || self.glitch_trigger > 0,
            // Optimistic never constructs a MoshPredictor.
            PredictionModel::Optimistic => true,
        }
    }

    /// Cells `render()` would actually paint right now: shown by the adaptive
    /// trigger AND past the tentative-epoch gate. The honest "is local echo
    /// visible" gauge, vs `active()` which also counts hidden predictions.
    fn shown_cells(&self) -> u64 {
        if !self.shown() {
            return 0;
        }
        let confirmed = self.buf.confirmed_epoch;
        self.buf
            .overlays
            .iter()
            .flat_map(|row| row.cells.iter())
            .filter(|c| c.active && !c.tentative(confirmed))
            .count() as u64
    }

    /// Port of PredictionEngine::cull, minus the optimistic branch (its own
    /// model).
    fn cull_mosh(&mut self, fb: &Snapshot, now: u64) {
        if self.display_preference == PredictionModel::Never {
            return;
        }

        if self.last_height != fb.rows || self.last_width != fb.cols {
            self.last_height = fb.rows;
            self.last_width = fb.cols;
            self.buf.reset();
        }

        // SRTT trigger with hysteresis.
        if self.send_interval > SRTT_TRIGGER_HIGH {
            self.srtt_trigger = true;
        } else if self.srtt_trigger && self.send_interval <= SRTT_TRIGGER_LOW && !self.buf.active() {
            // Only turn off when no predictions are being shown.
            self.srtt_trigger = false;
        }

        // Underlining with hysteresis.
        if self.send_interval > FLAG_TRIGGER_HIGH {
            self.flagging = true;
        } else if self.send_interval <= FLAG_TRIGGER_LOW {
            self.flagging = false;
        }

        // Really big glitches also activate underlining.
        if self.glitch_trigger > GLITCH_REPAIR_COUNT {
            self.flagging = true;
        }

        // Cell predictions.
        let late_ack = self.local_frame_late_acked;
        let mut do_reset = false;
        let mut kill_epochs: Vec<u64> = Vec::new();
        let mut confirmed_epoch = self.buf.confirmed_epoch;
        let mut glitch_trigger = self.glitch_trigger;
        let mut last_quick = self.last_quick_confirmation;
        let mut n_correct = 0u64;
        let mut n_nocredit = 0u64;
        let mut n_incorrect = 0u64;
        let mut n_nocredit_unknown = 0u64;
        let mut n_nocredit_blank = 0u64;
        let mut n_nocredit_matched = 0u64;
        let experimental = self.display_preference == PredictionModel::Experimental;

        self.buf.overlays.retain(|row| row.row_num < fb.rows);
        'rows: for row in self.buf.overlays.iter_mut() {
            let row_num = row.row_num;
            for j in 0..row.cells.len() {
                let validity = row.cells[j].get_validity(fb, row_num, late_ack);
                match validity {
                    Validity::IncorrectOrExpired => {
                        n_incorrect += 1;
                        let cell = &mut row.cells[j];
                        if cell.tentative(confirmed_epoch) {
                            if experimental {
                                cell.reset();
                            } else {
                                kill_epochs.push(cell.tentative_until_epoch);
                            }
                        } else if experimental {
                            cell.reset();
                        } else {
                            do_reset = true;
                            break 'rows;
                        }
                    }
                    Validity::Correct => {
                        n_correct += 1;
                        if row.cells[j].tentative_until_epoch > confirmed_epoch {
                            confirmed_epoch = row.cells[j].tentative_until_epoch;
                        }
                        // Quick confirmations slowly repair the glitch trigger.
                        if now.saturating_sub(row.cells[j].prediction_time) < GLITCH_THRESHOLD
                            && glitch_trigger > 0
                            && now.saturating_sub(GLITCH_REPAIR_MININTERVAL) >= last_quick
                        {
                            glitch_trigger -= 1;
                            last_quick = now;
                        }
                        // Match the rest of the row to the actual renditions.
                        if let Some(actual) = fb.cell(row_num, row.cells[j].col) {
                            let style = actual.style;
                            for k in row.cells[j..].iter_mut() {
                                k.replacement.style = style;
                            }
                        }
                        row.cells[j].reset();
                    }
                    Validity::CorrectNoCredit(reason) => {
                        n_nocredit += 1;
                        match reason {
                            NoCreditReason::Unknown => n_nocredit_unknown += 1,
                            NoCreditReason::Blank => n_nocredit_blank += 1,
                            NoCreditReason::MatchedOriginal => n_nocredit_matched += 1,
                        }
                        row.cells[j].reset();
                    }
                    Validity::Pending => {
                        let outstanding = now.saturating_sub(row.cells[j].prediction_time);
                        if outstanding >= GLITCH_FLAG_THRESHOLD {
                            glitch_trigger = GLITCH_REPAIR_COUNT * 2; // display and underline
                        } else if outstanding >= GLITCH_THRESHOLD
                            && glitch_trigger < GLITCH_REPAIR_COUNT
                        {
                            glitch_trigger = GLITCH_REPAIR_COUNT; // just display
                        }
                    }
                    Validity::Inactive => {}
                }
            }
        }

        self.buf.confirmed_epoch = confirmed_epoch;
        self.glitch_trigger = glitch_trigger;
        self.last_quick_confirmation = last_quick;
        self.pred_correct += n_correct;
        self.pred_nocredit += n_nocredit;
        self.pred_incorrect += n_incorrect;
        self.pred_nocredit_unknown += n_nocredit_unknown;
        self.pred_nocredit_blank += n_nocredit_blank;
        self.pred_nocredit_matched += n_nocredit_matched;

        if do_reset {
            self.mispredict_resets += 1;
            self.buf.reset();
            return;
        }
        for epoch in kill_epochs {
            self.buf.kill_epoch(epoch, fb);
        }

        // Cursor predictions.
        let cursor_wrong = self
            .buf
            .cursors
            .last()
            .map(|c| c.get_validity(fb, late_ack) == Validity::IncorrectOrExpired)
            .unwrap_or(false);
        if cursor_wrong {
            if experimental {
                self.buf.cursors.clear();
            } else {
                self.mispredict_resets += 1;
                self.buf.reset();
                return;
            }
        }
        self.buf
            .cursors
            .retain(|c| c.get_validity(fb, late_ack) == Validity::Pending);
    }
}

impl Predictor for MoshPredictor {
    fn set_frame_sent(&mut self, offset: u64) {
        self.buf.set_local_frame_sent(offset);
    }

    fn on_user_byte(&mut self, byte: u8, fb: &Snapshot, now: u64) {
        if self.display_preference == PredictionModel::Never {
            return;
        }
        if self.display_preference == PredictionModel::Experimental {
            self.buf.prediction_epoch = self.buf.confirmed_epoch;
        }

        self.cull_mosh(fb, now);
        self.buf.input(byte, fb, now);
    }

    fn on_server_frame(&mut self, input_ack: u64, echo_ack: u64, send_interval: u64) {
        // The ack setters clamp with max: callers feed them from every decoded
        // frame, including reordered/stale retransmissions whose acks are older
        // than what we already processed (mosh's transport-layer equivalents
        // are monotonic by construction).
        self.local_frame_acked = self.local_frame_acked.max(input_ack);
        self.local_frame_late_acked = self.local_frame_late_acked.max(echo_ack);
        self.send_interval = send_interval;
    }

    fn set_echo_safe(&mut self, _safe: bool) {
        // The mosh model has no optimistic echo gate.
    }

    fn cull(&mut self, fb: &Snapshot, now: u64) {
        self.cull_mosh(fb, now);
    }

    fn render(&self, fb: &mut Snapshot, renderer: &dyn PredictionRenderer) {
        if !self.shown() {
            return;
        }
        self.buf
            .render(fb, renderer, self.buf.confirmed_epoch, self.flagging);
    }

    fn reset(&mut self) {
        self.buf.reset();
    }

    fn active(&self) -> bool {
        self.buf.active()
    }

    fn needs_timer(&self) -> bool {
        // Timing-based triggers may still fire: poll with a short timeout so
        // glitches get detected.
        self.buf.active() && !(self.glitch_trigger > 0 && self.flagging)
    }

    fn stats(&self) -> PredictorStats {
        PredictorStats {
            active: self.buf.active(),
            shown_cells: self.shown_cells(),
            epoch_lag: self
                .buf
                .prediction_epoch
                .saturating_sub(self.buf.confirmed_epoch),
            mispredict_resets: self.mispredict_resets,
            outcomes: (self.pred_correct, self.pred_nocredit, self.pred_incorrect),
            nocredit_reasons: (
                self.pred_nocredit_unknown,
                self.pred_nocredit_blank,
                self.pred_nocredit_matched,
            ),
            srtt_trigger: self.srtt_trigger,
        }
    }
}

// Test-only accessors mirroring the old engine's #[cfg(test)] getters and the
// inherent setters the tests drove directly.
#[cfg(test)]
impl MoshPredictor {
    pub fn set_local_frame_sent(&mut self, x: u64) {
        self.buf.set_local_frame_sent(x);
    }

    pub fn set_local_frame_acked(&mut self, x: u64) {
        self.local_frame_acked = self.local_frame_acked.max(x);
    }

    pub fn set_local_frame_late_acked(&mut self, x: u64) {
        self.local_frame_late_acked = self.local_frame_late_acked.max(x);
    }

    pub fn set_send_interval(&mut self, x: u64) {
        self.send_interval = x;
    }

    pub fn local_frame_acked(&self) -> u64 {
        self.local_frame_acked
    }

    pub fn local_frame_late_acked(&self) -> u64 {
        self.local_frame_late_acked
    }

    pub fn flagging(&self) -> bool {
        self.flagging
    }

    pub fn glitch_trigger(&self) -> u32 {
        self.glitch_trigger
    }

    pub fn srtt_trigger_on(&self) -> bool {
        self.srtt_trigger
    }

    pub fn confirmed_epoch(&self) -> u64 {
        self.buf.confirmed_epoch
    }

    pub fn prediction_epoch(&self) -> u64 {
        self.buf.prediction_epoch
    }

    pub fn new_user_byte(&mut self, byte: u8, fb: &Snapshot, now: u64) {
        self.on_user_byte(byte, fb, now);
    }

    pub fn shown_cells_count(&self) -> u64 {
        self.shown_cells()
    }

    /// Test access to the overlay buffer (cursor/overlay inspection).
    pub(super) fn buf(&self) -> &OverlayBuffer {
        &self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::predict::test_support::{shown_char, PredictHarness};
    use crate::remote::predict::ReplaceRenderer;
    use posh_term::{Terminal, UnderlineStyle};

    fn snapshot(rows: u16, cols: u16, bytes: &[u8]) -> Snapshot {
        let mut t = Terminal::with_scrollback(rows, cols, 0);
        t.process(bytes);
        Snapshot::from_term(&t)
    }

    fn engine(pref: PredictionModel) -> MoshPredictor {
        MoshPredictor::new(pref, false)
    }

    #[test]
    fn never_preference_predicts_nothing() {
        let mut eng = engine(PredictionModel::Never);
        let fb = snapshot(5, 20, b"$ ");
        eng.new_user_byte(b'x', &fb, 100);
        assert!(!eng.active());
    }

    #[test]
    fn ack_counters_are_monotonic_across_reordered_frames() {
        // A reordered/stale server frame carries older acks than what we
        // already processed; they must not drive the counters backward
        // (mosh's transport-layer equivalents are monotonic).
        let mut eng = engine(PredictionModel::Always);
        eng.set_local_frame_acked(5);
        eng.set_local_frame_late_acked(4);
        eng.set_local_frame_acked(3);
        eng.set_local_frame_late_acked(2);
        assert_eq!(eng.local_frame_acked(), 5);
        assert_eq!(eng.local_frame_late_acked(), 4);
    }

    #[test]
    fn prediction_is_tentative_until_epoch_confirmed() {
        // mosh starts in prediction epoch 1 with confirmed epoch 0: brand-new
        // predictions stay hidden until the server confirms one.
        let mut eng = engine(PredictionModel::Always);
        let fb = snapshot(5, 20, b"$ ");
        eng.set_local_frame_sent(0);
        eng.new_user_byte(b'x', &fb, 100);
        assert!(eng.active());

        let mut overlaid = fb.clone();
        eng.render(&mut overlaid, &ReplaceRenderer);
        assert_eq!(shown_char(&overlaid, 0, 2), ' ', "tentative: not drawn");

        // Server confirms: echo ack covers the byte and the cell matches.
        let confirmed = snapshot(5, 20, b"$ x");
        eng.set_local_frame_late_acked(1);
        eng.cull(&confirmed, 150);
        assert_eq!(
            eng.confirmed_epoch(),
            eng.prediction_epoch(),
            "confirmation caught the prediction epoch up"
        );

        // The next prediction in the confirmed epoch is displayed.
        eng.set_local_frame_sent(1);
        eng.new_user_byte(b'y', &confirmed, 200);
        let mut overlaid = confirmed.clone();
        eng.render(&mut overlaid, &ReplaceRenderer);
        assert_eq!(shown_char(&overlaid, 0, 3), 'y');
        // ... and the cursor prediction advanced with it.
        assert_eq!(overlaid.cursor_col, 4);
    }

    /// Drives an engine to a confirmed epoch so subsequent predictions
    /// render immediately.
    fn confirmed_engine(pref: PredictionModel, fb_bytes: &[u8]) -> (MoshPredictor, Snapshot) {
        let mut eng = engine(pref);
        let fb = snapshot(5, 20, fb_bytes);
        eng.set_local_frame_sent(0);
        eng.new_user_byte(b'q', &fb, 0);
        // Server echoes 'q' at the predicted spot.
        let mut t = Terminal::with_scrollback(5, 20, 0);
        t.process(fb_bytes);
        t.process(b"q");
        let confirmed = Snapshot::from_term(&t);
        eng.set_local_frame_late_acked(1);
        eng.cull(&confirmed, 10);
        assert_eq!(
            eng.confirmed_epoch(),
            eng.prediction_epoch(),
            "warmup prediction confirmed"
        );
        eng.set_local_frame_sent(1);
        (eng, confirmed)
    }

    #[test]
    fn mismatch_culls_all_predictions() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Always, b"$ ");
        eng.new_user_byte(b'x', &fb, 100);
        assert!(eng.active());

        // Server state shows something else where 'x' was predicted.
        let mut t = Terminal::with_scrollback(5, 20, 0);
        t.process(b"$ qZ");
        let wrong = Snapshot::from_term(&t);
        eng.set_local_frame_late_acked(2);
        eng.cull(&wrong, 200);
        assert!(!eng.active(), "mismatched prediction culls everything");
    }

    #[test]
    fn control_byte_bumps_epoch_making_predictions_tentative() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Always, b"$ ");
        eng.new_user_byte(b'a', &fb, 100);
        let mut overlaid = fb.clone();
        eng.render(&mut overlaid, &ReplaceRenderer);
        assert_eq!(shown_char(&overlaid, 0, 3), 'a', "epoch-1 prediction shown");

        // Ctrl-T (random control byte) bumps the tentative epoch.
        eng.new_user_byte(0x14, &fb, 110);
        eng.new_user_byte(b'b', &fb, 120);
        let mut overlaid = fb.clone();
        eng.render(&mut overlaid, &ReplaceRenderer);
        assert_eq!(shown_char(&overlaid, 0, 3), 'a', "old epoch still shown");
        assert_eq!(
            shown_char(&overlaid, 0, 4),
            ' ',
            "post-control prediction is tentative and hidden"
        );
    }

    #[test]
    fn escape_sequence_bumps_epoch() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Always, b"$ ");
        let before = eng.prediction_epoch();
        // Up arrow: ESC [ A.
        for b in b"\x1b[A" {
            eng.new_user_byte(*b, &fb, 100);
        }
        assert!(
            eng.prediction_epoch() > before,
            "CSI A made input tentative"
        );
    }

    #[test]
    fn arrow_keys_move_predicted_cursor() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Always, b"$ abc");
        let start_col = fb.cursor_col;
        for b in b"\x1b[D" {
            eng.new_user_byte(*b, &fb, 100);
        }
        let mut overlaid = fb.clone();
        eng.render(&mut overlaid, &ReplaceRenderer);
        assert_eq!(overlaid.cursor_col, start_col - 1, "left arrow predicted");
        // ESC O C (application mode right arrow) is translated like CSI C.
        for b in b"\x1bOC" {
            eng.new_user_byte(*b, &fb, 110);
        }
        let mut overlaid = fb.clone();
        eng.render(&mut overlaid, &ReplaceRenderer);
        assert_eq!(overlaid.cursor_col, start_col, "right arrow predicted");
    }

    #[test]
    fn backspace_predicts_erase() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Always, b"$ ab");
        // fb shows "$ abq" with cursor after 'q'.
        let col = fb.cursor_col;
        eng.new_user_byte(0x7f, &fb, 100);
        let mut overlaid = fb.clone();
        eng.render(&mut overlaid, &ReplaceRenderer);
        assert_eq!(overlaid.cursor_col, col - 1, "cursor moved back");
        assert_eq!(
            shown_char(&overlaid, 0, col - 1),
            ' ',
            "erased cell predicted blank (shifted from the right)"
        );
    }

    #[test]
    fn correct_prediction_is_retired_without_glitch() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Always, b"$ ");
        eng.new_user_byte(b'x', &fb, 100);
        let mut t = Terminal::with_scrollback(5, 20, 0);
        t.process(b"$ qx");
        let echoed = Snapshot::from_term(&t);
        eng.set_local_frame_late_acked(2);
        eng.cull(&echoed, 150);
        assert_eq!(eng.glitch_trigger(), 0);
        // The cell prediction is retired; only nothing or cursor remains.
        let mut overlaid = echoed.clone();
        eng.render(&mut overlaid, &ReplaceRenderer);
        assert_eq!(shown_char(&overlaid, 0, 3), 'x', "real cell, no overlay");
    }

    #[test]
    fn glitch_trigger_fires_after_250ms_pending() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Adaptive, b"$ ");
        eng.set_send_interval(10); // fast link: srtt trigger off
        eng.new_user_byte(b'x', &fb, 1000);
        eng.cull(&fb, 1000 + GLITCH_THRESHOLD - 1);
        assert_eq!(eng.glitch_trigger(), 0, "not yet a glitch");
        eng.cull(&fb, 1000 + GLITCH_THRESHOLD);
        assert_eq!(
            eng.glitch_trigger(),
            GLITCH_REPAIR_COUNT,
            "250ms outstanding -> display"
        );
        assert!(!eng.flagging(), "displayed but not underlined yet");
        // 5s outstanding: underline too.
        eng.cull(&fb, 1000 + GLITCH_FLAG_THRESHOLD);
        assert_eq!(eng.glitch_trigger(), GLITCH_REPAIR_COUNT * 2);
        eng.cull(&fb, 1000 + GLITCH_FLAG_THRESHOLD + 1);
        assert!(eng.flagging(), "long glitch turns on underlining");
    }

    #[test]
    fn quick_confirmations_repair_glitch_trigger() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Adaptive, b"$ ");
        eng.set_send_interval(10);
        eng.new_user_byte(b'x', &fb, 1000);
        eng.cull(&fb, 1000 + GLITCH_THRESHOLD);
        assert_eq!(eng.glitch_trigger(), GLITCH_REPAIR_COUNT);

        // A quick confirmation decrements the trigger.
        let mut t = Terminal::with_scrollback(5, 20, 0);
        t.process(b"$ qx");
        let echoed = Snapshot::from_term(&t);
        eng.set_local_frame_late_acked(2);
        eng.set_local_frame_sent(2);
        eng.new_user_byte(b'y', &echoed, 2000);
        let mut t2 = Terminal::with_scrollback(5, 20, 0);
        t2.process(b"$ qxy");
        let echoed2 = Snapshot::from_term(&t2);
        eng.set_local_frame_late_acked(3);
        eng.cull(&echoed2, 2050); // confirmed within 250ms
        assert_eq!(eng.glitch_trigger(), GLITCH_REPAIR_COUNT - 1);
    }

    #[test]
    fn srtt_trigger_hysteresis() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Adaptive, b"$ ");
        eng.set_send_interval(40); // > 30ms high trigger
        eng.cull(&fb, 100);
        assert!(eng.srtt_trigger_on());
        // Dropping to 25 (between low and high) keeps it on.
        eng.set_send_interval(25);
        eng.cull(&fb, 200);
        assert!(eng.srtt_trigger_on());
        // At/below 20 with no active predictions it cures.
        eng.set_send_interval(20);
        eng.cull(&fb, 300);
        assert!(!eng.srtt_trigger_on());
    }

    #[test]
    fn flagging_hysteresis_via_send_interval() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Adaptive, b"$ ");
        eng.set_send_interval(100); // > 80ms
        eng.cull(&fb, 100);
        assert!(eng.flagging());
        eng.set_send_interval(60); // between 50 and 80: stays
        eng.cull(&fb, 200);
        assert!(eng.flagging());
        eng.set_send_interval(50); // <= 50: cured
        eng.cull(&fb, 300);
        assert!(!eng.flagging());
    }

    #[test]
    fn flagged_predictions_are_underlined() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Always, b"$ ");
        eng.set_send_interval(100); // flagging on
        eng.new_user_byte(b'z', &fb, 100);
        let mut overlaid = fb.clone();
        eng.render(&mut overlaid, &ReplaceRenderer);
        let col = fb.cursor_col;
        assert_eq!(shown_char(&overlaid, 0, col), 'z');
        assert_eq!(
            overlaid.cell(0, col).unwrap().style.underline,
            UnderlineStyle::Single,
            "slow-link predictions get underlined"
        );
    }

    #[test]
    fn adaptive_hides_predictions_on_fast_link() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Adaptive, b"$ ");
        eng.set_send_interval(10);
        eng.cull(&fb, 50);
        eng.new_user_byte(b'x', &fb, 100);
        let mut overlaid = fb.clone();
        eng.render(&mut overlaid, &ReplaceRenderer);
        assert_eq!(
            shown_char(&overlaid, 0, fb.cursor_col),
            ' ',
            "fast link: predictions exist but are not displayed"
        );
        assert!(eng.active());
    }

    #[test]
    fn newline_predicts_cursor_motion() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Always, b"$ hi");
        eng.new_user_byte(0x0d, &fb, 100);
        // CR predictions are tentative (epoch bumped); confirm the epoch by
        // checking the internal cursor moved.
        let c = eng.buf().cursors.last().unwrap();
        assert_eq!(c.col, 0);
        assert_eq!(c.row, fb.cursor_row + 1);
    }

    #[test]
    fn newline_on_last_row_predicts_blank_row_not_scroll() {
        let (mut eng, _) = confirmed_engine(PredictionModel::Always, b"$ ");
        let mut t = Terminal::with_scrollback(5, 20, 0);
        t.process(b"1\r\n2\r\n3\r\n4\r\n$ q");
        let fb = Snapshot::from_term(&t);
        assert_eq!(fb.cursor_row, fb.rows - 1);
        eng.new_user_byte(0x0d, &fb, 100);
        let c = eng.buf().cursors.last().unwrap();
        assert_eq!(c.row, fb.rows - 1, "no scroll prediction");
        assert_eq!(c.col, 0);
        // The last row has a blank prediction registered.
        assert!(eng
            .buf()
            .overlays
            .iter()
            .any(|r| r.row_num == fb.rows - 1 && r.cells.iter().all(|c| c.active)));
    }

    #[test]
    fn resize_resets_predictions() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Always, b"$ ");
        eng.new_user_byte(b'x', &fb, 100);
        assert!(eng.active());
        let bigger = snapshot(10, 40, b"$ ");
        eng.cull(&bigger, 200);
        assert!(!eng.active(), "size change resets the engine");
    }

    #[test]
    fn utf8_input_predicted_as_single_char() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Always, b"$ ");
        for b in "é".as_bytes() {
            eng.new_user_byte(*b, &fb, 100);
        }
        let mut overlaid = fb.clone();
        eng.render(&mut overlaid, &ReplaceRenderer);
        assert_eq!(shown_char(&overlaid, 0, fb.cursor_col), 'é');
    }

    #[test]
    fn insert_shifts_existing_text_right() {
        // Cursor placed in the middle of existing text.
        let (mut eng, _) = confirmed_engine(PredictionModel::Always, b"$ ");
        let mut t = Terminal::with_scrollback(5, 20, 0);
        t.process(b"$ qworld\x1b[1;4H"); // cursor on 'w'
        let fb = Snapshot::from_term(&t);
        eng.new_user_byte(b'X', &fb, 100);
        let mut overlaid = fb.clone();
        eng.render(&mut overlaid, &ReplaceRenderer);
        assert_eq!(shown_char(&overlaid, 0, 3), 'X');
        assert_eq!(shown_char(&overlaid, 0, 4), 'w', "text shifted right");
        assert_eq!(shown_char(&overlaid, 0, 5), 'o');
    }

    #[test]
    fn correct_confirmation_requires_late_ack() {
        let (mut eng, fb) = confirmed_engine(PredictionModel::Always, b"$ ");
        eng.new_user_byte(b'x', &fb, 100);
        // The screen already shows the char but the echo ack hasn't reached
        // the prediction's expiration offset: still pending, not retired.
        let mut t = Terminal::with_scrollback(5, 20, 0);
        t.process(b"$ qx");
        let echoed = Snapshot::from_term(&t);
        eng.set_local_frame_late_acked(1); // expiration is 2
        eng.cull(&echoed, 150);
        assert!(eng.active(), "still pending without the echo ack");
        eng.set_local_frame_late_acked(2);
        eng.cull(&echoed, 160);
        let still_cell_active = eng
            .buf()
            .overlays
            .iter()
            .any(|r| r.cells.iter().any(|c| c.active));
        assert!(!still_cell_active, "ack retires the cell prediction");
    }

    // ---------------------------------------------------------------------
    // Deterministic confirmation-cycle harness (github prediction-latency).
    // Drives the engine through the *real* client cycle via the shared
    // PredictHarness (test_support); see its docs.

    #[test]
    fn typed_char_is_credited_through_the_dump_vt_roundtrip() {
        // The core local-echo cycle: at a prompt, type 'l'; the server echoes
        // it; after culling against the dump_vt-reconstructed frame the engine
        // must CREDIT the prediction (confirmed_epoch catches up), or local echo
        // never un-hides (the field-observed shown=0 / nocredit-dominant bug).
        let mut h = PredictHarness::new(24, 80, b"$ ");
        let before = h.eng.confirmed_epoch();
        h.type_byte(b'l');
        h.server_echo(b"l");
        h.deliver();
        let stats = h.eng.stats();
        let (correct, nocredit, incorrect) = stats.outcomes;
        assert!(
            h.eng.confirmed_epoch() > before,
            "typed char not credited: confirmed_epoch stuck at {} \
             (correct={correct} nocredit={nocredit} incorrect={incorrect}, epoch_lag={})",
            h.eng.confirmed_epoch(),
            stats.epoch_lag,
        );
    }

    #[test]
    fn typing_along_a_suggestion_starves_prediction_credit() {
        // Fish (and other shells) show an autosuggestion: the grey character
        // you are about to type, sitting at the cursor. The prediction captures
        // that glyph in `original_contents`; `contents_match` ignores style, so
        // when the server echoes the (now solid) char the prediction "matches
        // what was already there" and is scored CorrectNoCredit. confirmed_epoch
        // never advances => predictions stay tentative => local echo is never
        // displayed. This is the field bug (shown=0, nocredit-dominant).
        //
        // Setup: the displayed frame already shows a GREY (SGR 90) 'x' at the
        // cursor — a fish autosuggestion — then the user types that same 'x'.
        // The committed echo is default-styled, so it differs from the grey
        // suggestion in rendition: the keystroke really did cause a change and
        // must be credited.
        let mut h = PredictHarness::new(24, 80, b"$ \x1b[90mx\x1b[0m\x1b[3G");
        let before = h.eng.confirmed_epoch();
        h.type_byte(b'x'); // type the already-suggested char
        h.server_echo(b"x"); // shell commits it (cursor advances)
        h.deliver();
        let (correct, nocredit, incorrect) = h.eng.stats().outcomes;
        assert!(
            h.eng.confirmed_epoch() > before,
            "typing the suggested char earned no credit: confirmed_epoch stuck at {} \
             (correct={correct} nocredit={nocredit} incorrect={incorrect}) -> local echo can \
             never un-hide",
            h.eng.confirmed_epoch(),
        );
    }
}
