//! FDR 0006 optimistic local echo: write echoes immediately (no epoch/credit
//! gating) and let the next server paint correct them. The client gates this
//! on the remote PTY's ECHO flag and alt-screen via [`set_echo_safe`]; when
//! echo is unsafe the overlay is dropped so passwords/full-screen apps stay
//! correct.
//!
//! [`set_echo_safe`]: super::Predictor::set_echo_safe

use crate::remote::display::Snapshot;

use super::overlay::OverlayBuffer;
use super::{PredictionRenderer, Predictor, PredictorStats};

pub struct OptimisticPredictor {
    buf: OverlayBuffer,
    local_frame_acked: u64,
    local_frame_late_acked: u64,
    /// Whether optimistic echo is currently safe (primary screen + remote PTY
    /// echoing). Set false to suppress; doing so resets the overlay.
    echo_safe: bool,
}

impl OptimisticPredictor {
    pub fn new(predict_overwrite: bool) -> OptimisticPredictor {
        OptimisticPredictor {
            // Optimistic is not Experimental: become_tentative bumps the epoch.
            buf: OverlayBuffer::new(predict_overwrite, true),
            local_frame_acked: 0,
            local_frame_late_acked: 0,
            echo_safe: false,
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

    /// FDR 0006 optimistic retirement: drop overlay cells and cursor
    /// predictions once the server frame has echoed past them
    /// (`local_frame_late_acked >= expiration_frame`), so the authoritative
    /// paint takes over. No epoch / credit / glitch logic — a gated ECHO means
    /// the echo always arrives, so the ack reliably retires the overlay.
    fn cull_optimistic(&mut self) {
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
        self.buf.cursors.retain(|c| late_ack < c.expiration_frame);
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

    fn cull(&mut self, _fb: &Snapshot, _now: u64) {
        self.cull_optimistic();
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
            mispredict_resets: 0,
            outcomes: (0, 0, 0),
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
