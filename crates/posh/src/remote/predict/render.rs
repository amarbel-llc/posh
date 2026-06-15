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

        // But the render styles differ: dim marks the cell faint; replace does
        // not (and optimistic never underlines).
        let replaced_style = replaced.cell(0, col).unwrap().style;
        let dimmed_style = dimmed.cell(0, col).unwrap().style;
        assert!(!replaced_style.dim, "replace renderer leaves dim off");
        assert_eq!(replaced_style.underline, UnderlineStyle::None);
        assert!(dimmed_style.dim, "dim renderer marks the predicted cell dim");
        assert_ne!(
            replaced_style, dimmed_style,
            "the two render styles must produce distinct cell styles"
        );
    }
}
