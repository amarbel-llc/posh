//! Prediction render styles: how a cell the model has decided to show is
//! painted onto the framebuffer. The model walks the visible predictions and
//! calls these for each shown cell + the cursor.

use posh_term::{Cell, UnderlineStyle};

use crate::remote::display::Snapshot;

use super::{CellHint, PredictionRenderer};

/// The default look, byte-for-byte today's `OverlayCell::apply` /
/// `CursorPrediction::apply`: replace the glyph when it differs from what is
/// on screen, and underline it when the slow-link flag is set.
pub struct ReplaceRenderer;

impl PredictionRenderer for ReplaceRenderer {
    fn paint_cell(&self, fb: &mut Snapshot, row: u16, col: u16, replacement: &Cell, hint: CellHint) {
        if hint.unknown {
            // Unknown-position cell: the model only offers it here when the
            // slow-link flag is on and it is not the last column, so all that
            // is painted is the underline (no glyph).
            if let Some(cell) = fb.cell_mut(row, col) {
                cell.style.underline = UnderlineStyle::Single;
            }
            return;
        }
        let differs = fb.cell(row, col) != Some(replacement);
        if differs {
            if let Some(cell) = fb.cell_mut(row, col) {
                *cell = replacement.clone();
                if hint.flagged {
                    cell.style.underline = UnderlineStyle::Single;
                }
            }
        }
    }

    fn paint_cursor(&self, fb: &mut Snapshot, row: u16, col: u16) {
        fb.cursor_row = row;
        fb.cursor_col = col;
    }
}

/// An alternate look that proves the render axis: replace the glyph the same
/// way, but mark predicted cells with a dim/faint rendition (SGR 2) instead of
/// an underline. The model's visibility decisions are untouched — only the
/// visual treatment differs.
pub struct DimRenderer;

impl PredictionRenderer for DimRenderer {
    fn paint_cell(&self, fb: &mut Snapshot, row: u16, col: u16, replacement: &Cell, hint: CellHint) {
        if hint.unknown {
            // Unknown-position cell: no glyph to draw, so mark the existing
            // cell dim when flagged (the dim analogue of the underline branch).
            if let Some(cell) = fb.cell_mut(row, col) {
                cell.style.dim = true;
            }
            return;
        }
        let differs = fb.cell(row, col) != Some(replacement);
        if differs {
            if let Some(cell) = fb.cell_mut(row, col) {
                *cell = replacement.clone();
                cell.style.dim = true;
            }
        }
    }

    fn paint_cursor(&self, fb: &mut Snapshot, row: u16, col: u16) {
        fb.cursor_row = row;
        fb.cursor_col = col;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::predict::{OptimisticPredictor, Predictor};
    use posh_term::{Terminal, UnderlineStyle};

    fn snapshot(rows: u16, cols: u16, bytes: &[u8]) -> Snapshot {
        let mut t = Terminal::with_scrollback(rows, cols, 0);
        t.process(bytes);
        Snapshot::from_term(&t)
    }

    #[test]
    fn render_style_changes_predicted_cell_style() {
        // The render axis: the SAME prediction painted by ReplaceRenderer vs
        // DimRenderer must produce different cell styles. Optimistic shows the
        // typed char immediately (no epoch gate), so one keystroke suffices.
        let fb = snapshot(5, 20, b"$ ");
        let mut eng = OptimisticPredictor::new(false);
        eng.set_echo_safe(true);
        eng.set_frame_sent(0);
        eng.on_user_byte(b'z', &fb, 100);

        let col = fb.cursor_col;

        let mut replaced = fb.clone();
        eng.render(&mut replaced, &ReplaceRenderer);
        let mut dimmed = fb.clone();
        eng.render(&mut dimmed, &DimRenderer);

        // Both paint the predicted glyph.
        assert_eq!(replaced.cell(0, col).unwrap().ch, 'z');
        assert_eq!(dimmed.cell(0, col).unwrap().ch, 'z');

        // But the render styles differ: dim marks the cell faint; replace
        // underlines. Both mark the cell — `always_flags` — even though the
        // optimistic MODEL raises no slow-link flag (the render-axis split).
        let replaced_style = replaced.cell(0, col).unwrap().style;
        let dimmed_style = dimmed.cell(0, col).unwrap().style;
        assert!(!replaced_style.dim, "replace renderer leaves dim off");
        assert_eq!(replaced_style.underline, UnderlineStyle::Single);
        assert!(dimmed_style.dim, "dim renderer marks the predicted cell dim");
        assert_ne!(
            replaced_style, dimmed_style,
            "the two render styles must produce distinct cell styles"
        );
    }

    /// The advice channel end to end: a `Policed` renderer under `Advised`
    /// honors the mosh model's recommendations (fast link ⇒ don't show; flag
    /// off ⇒ no underline; tentative ⇒ held), while `Always` disregards them.
    #[test]
    fn policed_renderer_honors_or_disregards_the_models_advice() {
        use crate::remote::predict::{MoshPredictor, Policed, PredictionModel, ShowPolicy};
        let fb = snapshot(5, 20, b"$ ");
        let col = fb.cursor_col;
        let mut eng = MoshPredictor::new(PredictionModel::Always, false);
        eng.set_send_interval(10); // fast: flag off; first keystroke tentative
        eng.set_frame_sent(0);
        eng.on_user_byte(b'z', &fb, 100);

        let advised = Policed::new(ReplaceRenderer, ShowPolicy::Advised);
        let mut out = fb.clone();
        eng.render(&mut out, &advised);
        assert_eq!(out.cell(0, col).unwrap().ch, ' ', "advised: tentative cell held");

        let always = Policed::new(ReplaceRenderer, ShowPolicy::Always);
        let mut out = fb.clone();
        eng.render(&mut out, &always);
        let cell = out.cell(0, col).unwrap();
        assert_eq!(cell.ch, 'z', "always: painted despite the hold");
        assert_eq!(cell.style.underline, UnderlineStyle::Single, "always: marked despite flag off");

        // Once the model confirms the epoch, an advised renderer paints the
        // next keystroke but, with the flag still off, leaves it unmarked.
        let confirmed = snapshot(5, 20, b"$ z");
        eng.set_local_frame_late_acked(1);
        eng.cull(&confirmed, 150);
        eng.set_frame_sent(1);
        eng.on_user_byte(b'y', &confirmed, 200);
        let mut out = confirmed.clone();
        eng.render(&mut out, &advised);
        let cell = out.cell(0, col + 1).unwrap();
        assert_eq!(cell.ch, 'y', "advised: confirmed epoch, painted");
        assert_eq!(cell.style.underline, UnderlineStyle::None, "advised: flag off, unmarked");
    }

    /// The renderer walks the model's offer and reports the screen-side truth:
    /// under `always` the tentative cell is painted+marked; under `advised` it
    /// is held (counted, not painted); a `never` model offers nothing.
    #[test]
    fn render_outcome_reports_what_the_renderer_did() {
        use crate::remote::predict::{
            MoshPredictor, Policed, PredictionModel, Predictor as _, RenderOutcome, ShowPolicy,
        };
        let fb = snapshot(5, 20, b"$ ");
        let mut eng = MoshPredictor::new(PredictionModel::Always, false);
        eng.set_send_interval(10);
        eng.set_frame_sent(0);
        eng.on_user_byte(b'z', &fb, 100);
        let step = eng.offer().expect("the mosh model always offers");
        assert!(step.advice.show && !step.advice.flag);

        let mut out = fb.clone();
        let always = Policed::new(ReplaceRenderer, ShowPolicy::Always).render_step(&mut out, &step);
        // An insert shifts the rest of the row, so the overlay holds one cell
        // per column to the right of the cursor (blank over blank, unmarked):
        // `painted_cells` counts everything handed to `paint_cell`, and
        // `marked_cells` is the one glyph the user sees as a prediction.
        assert!(always.painted_cells >= 1, "{always:?}");
        assert_eq!(always.marked_cells, 1, "{always:?}");
        assert_eq!(always.held_cells, 0);
        assert!(always.cursor_painted && !always.step_skipped);
        let mut out = fb.clone();
        let advised = Policed::new(ReplaceRenderer, ShowPolicy::Advised).render_step(&mut out, &step);
        assert_eq!(advised.painted_cells, 0, "advised: nothing painted while tentative");
        assert!(advised.held_cells >= 1, "advised: the tentative cells are held");
        assert!(!advised.cursor_painted);

        let never = MoshPredictor::new(PredictionModel::Never, false);
        assert!(never.offer().is_some(), "mosh offers even when empty");
        let mut out = fb.clone();
        assert_eq!(never.render(&mut out, &ReplaceRenderer), RenderOutcome::default());
    }

    /// The predictor/renderer split (2026-08-25): a prediction the mosh model
    /// still holds as TENTATIVE (first keystroke of a new epoch on a fast
    /// link, flag off) is painted immediately and marked — the renderer, not
    /// the model, decides when and how. The model's own `never` stays: it
    /// records nothing, so there is nothing to paint.
    #[test]
    fn renderer_paints_tentative_predictions_immediately_and_marked() {
        use crate::remote::predict::{MoshPredictor, PredictionModel};
        let fb = snapshot(5, 20, b"$ ");
        let mut eng = MoshPredictor::new(PredictionModel::Always, false);
        eng.set_send_interval(10); // fast link: no srtt trigger, no flagging
        eng.set_frame_sent(0);
        // A fresh predictor's first keystroke is tentative until an epoch
        // confirms — mosh would hide it for a round trip.
        eng.on_user_byte(b'z', &fb, 100);
        assert!(!eng.flagging(), "the MODEL raises no flag on a fast link");
        let col = fb.cursor_col;
        let mut out = fb.clone();
        eng.render(&mut out, &ReplaceRenderer);
        let cell = out.cell(0, col).unwrap();
        assert_eq!(cell.ch, 'z', "painted despite being tentative");
        assert_eq!(cell.style.underline, UnderlineStyle::Single, "always marked");

        let mut never = MoshPredictor::new(PredictionModel::Never, false);
        never.set_frame_sent(0);
        never.on_user_byte(b'z', &fb, 100);
        let mut out = fb.clone();
        never.render(&mut out, &ReplaceRenderer);
        assert_eq!(out.cell(0, col).unwrap().ch, ' ', "`never` is the model's call");
    }
}
