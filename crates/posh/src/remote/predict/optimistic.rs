//! FDR 0006 optimistic local echo: write echoes immediately (no epoch/credit
//! gating) and let the next server paint correct them. The client gates this
//! on the remote PTY's ECHO flag and alt-screen via [`set_echo_safe`]; when
//! echo is unsafe the overlay is dropped so passwords/full-screen apps stay
//! correct.
//!
//! [`set_echo_safe`]: super::Predictor::set_echo_safe

use crate::remote::display::Snapshot;

use super::overlay::{OverlayBuffer, Validity};
use super::{PredictionRenderer, Predictor, PredictorStats};

pub struct OptimisticPredictor {
    buf: OverlayBuffer,
    local_frame_acked: u64,
    local_frame_late_acked: u64,
    /// Whether optimistic echo is currently safe (primary screen + remote PTY
    /// echoing). Set false to suppress; doing so resets the overlay.
    echo_safe: bool,
    /// Cursor-prediction chains dropped because the server contradicted an
    /// acked one (reported as `mispredict_resets`; the FDR 0006 A/B gauge for
    /// "how often did optimistic's cursor argue with the server").
    cursor_resets: u64,
}

impl OptimisticPredictor {
    pub fn new(predict_overwrite: bool) -> OptimisticPredictor {
        OptimisticPredictor {
            // Optimistic is not Experimental: become_tentative bumps the epoch.
            buf: OverlayBuffer::new(predict_overwrite, true),
            local_frame_acked: 0,
            local_frame_late_acked: 0,
            echo_safe: false,
            cursor_resets: 0,
        }
    }

    /// Cells `render()` would actually paint right now: optimistic draws every
    /// active cell (no tentative gate), so this is just the active count.
    fn shown_cells(&self) -> u64 {
        self.buf
            .overlays
            .iter()
            .flat_map(|row| row.cells.iter())
            .filter(|c| c.active && !c.tentative(u64::MAX))
            .count() as u64
    }

    /// FDR 0006 optimistic retirement: drop overlay cells once the server
    /// frame has echoed past them (`local_frame_late_acked >=
    /// expiration_frame`), so the authoritative paint takes over. No epoch /
    /// credit / glitch logic for CELLS — a gated ECHO means the echo always
    /// arrives, so the ack reliably retires the overlay, and a wrong cell is
    /// simply overpainted.
    ///
    /// The CURSOR is held to a stricter rule. Cursor predictions chain (each
    /// new one continues from the last predicted position), so when an acked
    /// prediction turns out wrong — the server landed the cursor elsewhere: a
    /// prompt redraw, an autosuggestion, the post-Enter prompt — every newer
    /// prediction was built on the wrong spot and the visible cursor would sit
    /// there until each was acked in turn (the lingering-offset shape on a
    /// slow link). So once the server has spoken and disagreed, the whole
    /// chain is dropped and the next keystroke re-seeds from the frame; an
    /// acked prediction the server agrees with retires quietly. Optimistic
    /// never argues with the server about where the cursor is.
    fn cull_optimistic(&mut self, fb: &Snapshot) {
        let late_ack = self.local_frame_late_acked;
        for row in self.buf.overlays.iter_mut() {
            for cell in row.cells.iter_mut() {
                if cell.active && late_ack >= cell.expiration_frame {
                    cell.reset();
                }
            }
        }
        self.buf
            .overlays
            .retain(|row| row.cells.iter().any(|c| c.active));
        // Judge the NEWEST acked prediction only (mosh's cull judges the
        // last entry): older chain entries are frozen intermediate positions
        // (only the newest cursor ever moves; an epoch bump freezes it and
        // pushes a successor), so when one echo ack covers several epochs at
        // once — routine on the slow links optimistic targets — the frame's
        // cursor legitimately sits at the newest spot and every older entry
        // "mismatches". Only the newest acked entry carries a verdict.
        let newest_acked = self
            .buf
            .cursors
            .iter()
            .rev()
            .map(|c| c.get_validity(fb, late_ack))
            .find(|v| !matches!(v, Validity::Pending | Validity::Inactive));
        if newest_acked == Some(Validity::IncorrectOrExpired) {
            self.cursor_resets += 1;
            self.buf.cursors.clear();
        } else {
            self.buf
                .cursors
                .retain(|c| c.get_validity(fb, late_ack) == Validity::Pending);
        }
    }
}

impl Predictor for OptimisticPredictor {
    fn set_frame_sent(&mut self, offset: u64) {
        self.buf.set_local_frame_sent(offset);
    }

    fn on_user_byte(&mut self, byte: u8, fb: &Snapshot, now: u64) {
        self.cull(fb, now);
        self.buf.input(byte, fb, now);
    }

    fn on_server_frame(&mut self, input_ack: u64, echo_ack: u64, send_interval: u64) {
        // The ack setters clamp with max: callers feed them from every decoded
        // frame, including reordered/stale retransmissions whose acks are older
        // than what we already processed.
        self.local_frame_acked = self.local_frame_acked.max(input_ack);
        self.local_frame_late_acked = self.local_frame_late_acked.max(echo_ack);
        // Optimistic ignores the send interval (no adaptive trigger).
        let _ = send_interval;
    }

    fn set_echo_safe(&mut self, safe: bool) {
        if !safe {
            // Optimistic echo gated off (password prompt / full-screen app):
            // drop the overlay so it is not shown; the authoritative paint
            // stands.
            self.buf.reset();
        }
        self.echo_safe = safe;
    }

    fn cull(&mut self, fb: &Snapshot, _now: u64) {
        self.cull_optimistic(fb);
    }

    fn render(&self, fb: &mut Snapshot, renderer: &dyn PredictionRenderer) {
        // Optimistic draws every active prediction immediately, with no
        // tentative/confirmed-epoch gate and no slow-link underline: force the
        // confirmed epoch to u64::MAX (so `tentative()` is always false) and
        // suppress flagging (FDR 0006).
        self.buf.render(fb, renderer, u64::MAX, false);
    }

    fn reset(&mut self) {
        self.buf.reset();
    }

    fn active(&self) -> bool {
        self.buf.active()
    }

    fn needs_timer(&self) -> bool {
        // Verbatim from the old engine's needs_timer for optimistic: glitch
        // triggers never fire here, so `!(glitch>0 && flagging)` is always
        // true, leaving `active()`.
        self.buf.active()
    }

    fn stats(&self) -> PredictorStats {
        PredictorStats {
            active: self.buf.active(),
            shown_cells: self.shown_cells(),
            epoch_lag: self
                .buf
                .prediction_epoch
                .saturating_sub(self.buf.confirmed_epoch),
            mispredict_resets: self.cursor_resets,
            outcomes: (0, 0, 0),
            nocredit_reasons: (0, 0, 0),
            srtt_trigger: false,
        }
    }
}

#[cfg(test)]
impl OptimisticPredictor {
    pub fn shown_cells_count(&self) -> u64 {
        self.shown_cells()
    }

    pub fn confirmed_epoch(&self) -> u64 {
        self.buf.confirmed_epoch
    }
}

#[cfg(test)]
mod tests {
    use crate::remote::predict::test_support::{shown_char, PredictHarness};
    use crate::remote::predict::PredictionModel;

    #[test]
    fn optimistic_echo_shows_the_first_char_immediately() {
        // FDR 0006: unlike adaptive (which hides the first prediction until an
        // epoch confirms a round-trip later), optimistic draws the keystroke at
        // once — no tentative/confirmed-epoch gate.
        let mut h = PredictHarness::with_pref(24, 80, b"$ ", PredictionModel::Optimistic);
        assert_eq!(h.eng.shown_cells(), 0, "nothing typed yet");
        h.type_byte(b'l');
        assert!(
            h.eng.shown_cells() >= 1,
            "optimistic must show the typed char immediately (shown={})",
            h.eng.shown_cells(),
        );
    }

    #[test]
    fn optimistic_echo_retires_after_the_server_paint() {
        // Once the server frame has echoed the char (echo-ack past expiration),
        // the overlay retires and the authoritative paint stands — no lingering.
        let mut h = PredictHarness::with_pref(24, 80, b"$ ", PredictionModel::Optimistic);
        h.type_byte(b'l');
        assert!(h.eng.shown_cells() >= 1);
        h.server_echo(b"l");
        h.deliver();
        assert_eq!(
            h.eng.shown_cells(),
            0,
            "echoed char's overlay must retire after the paint",
        );
        assert_eq!(shown_char(&h.display, 0, 2), 'l', "the real paint stands");
    }

    #[test]
    fn optimistic_drops_every_cursor_prediction_once_the_server_contradicts_one() {
        // The lingering-offset bug: cursor predictions CHAIN (each new one
        // continues from the last predicted position), so when the server's
        // echo lands the cursor somewhere else — a prompt redraw, an
        // autosuggestion, the post-Enter prompt — retiring only the acked
        // prediction leaves the newer ones painted where the chain went,
        // and the visible cursor sits there until every one is acked. Once
        // the server has spoken about a prediction and disagreed, the whole
        // chain is wrong: drop it, and let the next keystroke re-seed from
        // the frame. Cells stay optimistic; only the cursor defers.
        use crate::remote::display::Snapshot;
        use crate::remote::predict::test_support::reparse;
        use crate::remote::predict::{Predictor, ReplaceRenderer};
        use posh_term::Terminal;

        let mut server = Terminal::with_scrollback(24, 80, 0);
        server.process(b"$ ");
        let fb0 = Snapshot::from_term(&reparse(24, 80, &server.dump_vt()));
        assert_eq!((fb0.cursor_row, fb0.cursor_col), (0, 2));

        // Within one epoch a single cursor prediction just moves; an epoch
        // bump (Enter, any control key) pushes a NEW one that continues from
        // the last predicted spot — that is the chain.
        let type_chain = |eng: &mut super::OptimisticPredictor| {
            eng.set_echo_safe(true);
            eng.set_frame_sent(0);
            eng.on_user_byte(b'a', &fb0, 1000); // cursor (0,3), expires at frame 1
            eng.set_frame_sent(1);
            eng.on_user_byte(b'\r', &fb0, 1005); // epoch bump: new cursor (1,0), exp 2
            eng.set_frame_sent(2);
            eng.on_user_byte(b'b', &fb0, 1010); // that cursor moves to (1,1), exp 3
        };
        let mut eng = super::OptimisticPredictor::new(false);
        type_chain(&mut eng);
        let mut shown = fb0.clone();
        eng.render(&mut shown, &ReplaceRenderer);
        assert_eq!((shown.cursor_row, shown.cursor_col), (1, 1), "the chain's head is painted");

        // The server echoes 'a' but leaves the cursor at column 10 (a prompt
        // redraw); its echo ack covers 'a' only — the CR and 'b' are in flight.
        server.process(b"a\x1b[11G");
        let fb1 = Snapshot::from_term(&reparse(24, 80, &server.dump_vt()));
        assert_eq!((fb1.cursor_row, fb1.cursor_col), (0, 10));
        eng.on_server_frame(1, 1, 50);
        eng.cull(&fb1, 1100);
        let mut next = fb1.clone();
        eng.render(&mut next, &ReplaceRenderer);
        assert_eq!(
            (next.cursor_row, next.cursor_col),
            (0, 10),
            "the server contradicted the acked prediction: the chained one must go too"
        );
        assert_eq!(eng.stats().mispredict_resets, 1, "the reset is counted");

        // A subsequent keystroke re-seeds from the frame, not the dead chain.
        eng.set_frame_sent(3);
        eng.on_user_byte(b'c', &fb1, 1200);
        let mut again = fb1.clone();
        eng.render(&mut again, &ReplaceRenderer);
        assert_eq!((again.cursor_row, again.cursor_col), (0, 11));

        // And an acked prediction the server AGREES with retires quietly,
        // leaving the newer chained one in place.
        let mut agree = super::OptimisticPredictor::new(false);
        type_chain(&mut agree);
        let mut server2 = Terminal::with_scrollback(24, 80, 0);
        server2.process(b"$ a");
        let fb2 = Snapshot::from_term(&reparse(24, 80, &server2.dump_vt()));
        agree.on_server_frame(1, 1, 50);
        agree.cull(&fb2, 1100);
        let mut ok = fb2.clone();
        agree.render(&mut ok, &ReplaceRenderer);
        assert_eq!((ok.cursor_row, ok.cursor_col), (1, 1), "chain survives agreement");
        assert_eq!(agree.stats().mispredict_resets, 0);

        // Batched acks: one frame's echo ack covering the WHOLE chain judges
        // only the newest entry. The older entry's frozen intermediate spot
        // (0,3) no longer matches the frame — that is not a contradiction,
        // the chain simply completed; the frame's cursor agrees with its
        // newest prediction.
        let mut batch = super::OptimisticPredictor::new(false);
        type_chain(&mut batch);
        let mut server3 = Terminal::with_scrollback(24, 80, 0);
        server3.process(b"$ a\r\nb");
        let fb3 = Snapshot::from_term(&reparse(24, 80, &server3.dump_vt()));
        assert_eq!((fb3.cursor_row, fb3.cursor_col), (1, 1));
        batch.on_server_frame(3, 3, 50);
        batch.cull(&fb3, 1100);
        let mut done = fb3.clone();
        batch.render(&mut done, &ReplaceRenderer);
        assert_eq!((done.cursor_row, done.cursor_col), (1, 1));
        assert_eq!(
            batch.stats().mispredict_resets,
            0,
            "a completed chain is not a contradiction (the A/B gauge stays honest)"
        );
    }

    #[test]
    fn optimistic_shows_typing_along_a_suggestion_immediately() {
        // The adaptive credit-starvation scenario (grey autosuggestion at the
        // cursor): optimistic has no credit concept, so it just echoes the char.
        let mut h = PredictHarness::with_pref(
            24,
            80,
            b"$ \x1b[90mx\x1b[0m\x1b[3G",
            PredictionModel::Optimistic,
        );
        h.type_byte(b'x');
        assert!(
            h.eng.shown_cells() >= 1,
            "optimistic must echo a char typed along a suggestion (shown={})",
            h.eng.shown_cells(),
        );
    }
}
