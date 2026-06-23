//! Shared keystroke→overlay machinery reused by every prediction model.
//!
//! `OverlayBuffer` owns the parts of mosh's PredictionEngine that are common
//! to both the adaptive port and the optimistic echo: the input byte parser,
//! the overlay/cursor prediction cells, the keystroke handlers, and the
//! tentative/confirmed-epoch render walk. The model layer (`mosh`/`optimistic`)
//! wraps a buffer and adds its own validation lifecycle (cull) and display
//! policy on top.

use posh_term::{wcwidth, Cell, Style};

use crate::remote::display::{blank_cell, Snapshot};

use super::{CellHint, PredictionRenderer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Validity {
    Pending,
    Correct,
    CorrectNoCredit(NoCreditReason),
    IncorrectOrExpired,
    Inactive,
}

/// Why a `CorrectNoCredit` prediction earned no credit, so the field
/// "nocredit-dominant" credit starvation can be attributed to a specific branch
/// instead of an opaque aggregate (#predict-echo debuggability).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NoCreditReason {
    /// The cell is still `unknown` — the last cell of a print (the cursor
    /// cell), whose final glyph the server has not pinned down yet.
    Unknown,
    /// The prediction replaces the cell with blank — too weak to credit
    /// ("too easy to trigger falsely").
    Blank,
    /// The predicted glyph AND rendition were already on screen: a genuine no-op
    /// prediction (e.g. typing along a byte-identical autosuggestion).
    MatchedOriginal,
}

/// mosh Cell::contents_match: glyphs match, ignoring renditions.
fn contents_match(a: &Cell, b: &Cell) -> bool {
    (a.is_blank() && b.is_blank()) || (a.ch == b.ch && a.extra == b.extra)
}

#[derive(Debug, Clone)]
pub(super) struct OverlayCell {
    pub active: bool,
    pub tentative_until_epoch: u64,
    pub expiration_frame: u64,
    pub prediction_time: u64,
    pub col: u16,
    pub unknown: bool,
    pub replacement: Cell,
    /// No credit for predictions that match what was already there.
    pub original_contents: Vec<Cell>,
}

impl OverlayCell {
    fn new(col: u16, tentative: u64) -> OverlayCell {
        OverlayCell {
            active: false,
            tentative_until_epoch: tentative,
            expiration_frame: 0,
            prediction_time: u64::MAX,
            col,
            unknown: false,
            replacement: blank_cell(),
            original_contents: Vec::new(),
        }
    }

    pub fn tentative(&self, confirmed_epoch: u64) -> bool {
        self.tentative_until_epoch > confirmed_epoch
    }

    pub fn reset(&mut self) {
        self.active = false;
        self.unknown = false;
        self.tentative_until_epoch = u64::MAX;
        self.expiration_frame = u64::MAX;
        self.original_contents.clear();
    }

    fn reset_with_orig(&mut self) {
        if !self.active || self.unknown {
            self.reset();
            return;
        }
        let kept = self.replacement.clone();
        let mut orig = std::mem::take(&mut self.original_contents);
        orig.push(kept);
        self.reset();
        self.original_contents = orig;
    }

    fn expire(&mut self, expiration_frame: u64, now: u64) {
        self.expiration_frame = expiration_frame;
        self.prediction_time = now;
    }

    pub fn get_validity(&self, fb: &Snapshot, row: u16, late_ack: u64) -> Validity {
        if !self.active {
            return Validity::Inactive;
        }
        if row >= fb.rows || self.col >= fb.cols {
            return Validity::IncorrectOrExpired;
        }
        if late_ack < self.expiration_frame {
            return Validity::Pending;
        }
        if self.unknown {
            return Validity::CorrectNoCredit(NoCreditReason::Unknown);
        }
        if self.replacement.is_blank() {
            // Too easy for this to trigger falsely.
            return Validity::CorrectNoCredit(NoCreditReason::Blank);
        }
        let current = fb.cell(row, self.col).expect("cell in range");
        if contents_match(current, &self.replacement) {
            // Withhold credit only when the prediction is byte-identical (glyph
            // AND rendition) to what was already on screen — a genuinely no-op
            // prediction. A faint autosuggestion (fish) shares the glyph but not
            // the style, so typing along it still earns credit. Using
            // style-insensitive contents_match here scored every such keystroke
            // CorrectNoCredit, starving confirmed_epoch and hiding all local
            // echo (shown=0, nocredit-dominant).
            if self.original_contents.iter().any(|c| c == &self.replacement) {
                return Validity::CorrectNoCredit(NoCreditReason::MatchedOriginal);
            }
            return Validity::Correct;
        }
        Validity::IncorrectOrExpired
    }

    /// Paints this cell through `renderer` if it is shown: active, in bounds,
    /// and past the tentative-epoch gate. `flag` is the slow-link underline
    /// policy; the blank-over-blank case clears it (matching mosh).
    fn render(
        &self,
        fb: &mut Snapshot,
        renderer: &dyn PredictionRenderer,
        confirmed_epoch: u64,
        row: u16,
        mut flag: bool,
    ) {
        if !self.active || row >= fb.rows || self.col >= fb.cols {
            return;
        }
        if self.tentative(confirmed_epoch) {
            return;
        }
        let current_blank = fb.cell(row, self.col).map(|c| c.is_blank()).unwrap_or(true);
        if self.replacement.is_blank() && current_blank {
            flag = false;
        }
        if self.unknown {
            // An unknown-position cell paints no glyph; only the slow-link
            // flag (away from the last column) is offered to the renderer.
            if flag && self.col != fb.cols - 1 {
                renderer.paint_cell(
                    fb,
                    row,
                    self.col,
                    &self.replacement,
                    CellHint {
                        flagged: true,
                        unknown: true,
                    },
                );
            }
            return;
        }
        renderer.paint_cell(
            fb,
            row,
            self.col,
            &self.replacement,
            CellHint {
                flagged: flag,
                unknown: false,
            },
        );
    }
}

#[derive(Debug, Clone)]
pub(super) struct OverlayRow {
    pub row_num: u16,
    pub cells: Vec<OverlayCell>,
}

#[derive(Debug, Clone)]
pub(super) struct CursorPrediction {
    pub active: bool,
    pub tentative_until_epoch: u64,
    pub expiration_frame: u64,
    pub row: u16,
    pub col: u16,
}

impl CursorPrediction {
    fn tentative(&self, confirmed_epoch: u64) -> bool {
        self.tentative_until_epoch > confirmed_epoch
    }

    pub fn get_validity(&self, fb: &Snapshot, late_ack: u64) -> Validity {
        if !self.active {
            return Validity::Inactive;
        }
        if self.row >= fb.rows || self.col >= fb.cols {
            return Validity::IncorrectOrExpired;
        }
        if late_ack >= self.expiration_frame {
            if fb.cursor_row == self.row && fb.cursor_col == self.col {
                return Validity::Correct;
            }
            return Validity::IncorrectOrExpired;
        }
        Validity::Pending
    }

    fn render(&self, fb: &mut Snapshot, renderer: &dyn PredictionRenderer, confirmed_epoch: u64) {
        if !self.active || self.tentative(confirmed_epoch) {
            return;
        }
        if self.row < fb.rows && self.col < fb.cols {
            renderer.paint_cursor(fb, self.row, self.col);
        }
    }
}

// ---------------------------------------------------------------------------
// Byte-stream parser for the user's keystrokes (a tiny subset of mosh's
// UTF8Parser/Transition machinery: print vs. control vs. ESC/CSI dispatch).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    Ground,
    Esc,
    Csi,
    Utf8 { need: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputAction {
    Print(char),
    Execute(u8),
    EscDispatch,
    CsiDispatch(u8),
    None,
}

#[derive(Debug)]
struct InputParser {
    state: ParseState,
    utf8: [u8; 4],
    utf8_len: u8,
}

impl InputParser {
    fn new() -> InputParser {
        InputParser {
            state: ParseState::Ground,
            utf8: [0; 4],
            utf8_len: 0,
        }
    }

    fn input(&mut self, b: u8) -> InputAction {
        match self.state {
            ParseState::Ground => match b {
                0x1b => {
                    self.state = ParseState::Esc;
                    InputAction::None
                }
                0x00..=0x1a | 0x1c..=0x1f => InputAction::Execute(b),
                0x20..=0x7f => InputAction::Print(b as char),
                0xc2..=0xdf => {
                    self.start_utf8(b, 1);
                    InputAction::None
                }
                0xe0..=0xef => {
                    self.start_utf8(b, 2);
                    InputAction::None
                }
                0xf0..=0xf4 => {
                    self.start_utf8(b, 3);
                    InputAction::None
                }
                _ => InputAction::Execute(b), // invalid lead byte
            },
            ParseState::Esc => match b {
                // mosh translates application-cursor ESC O into CSI.
                b'[' | b'O' => {
                    self.state = ParseState::Csi;
                    InputAction::None
                }
                _ => {
                    self.state = ParseState::Ground;
                    InputAction::EscDispatch
                }
            },
            ParseState::Csi => match b {
                0x20..=0x3f => InputAction::None, // params/intermediates
                0x40..=0x7e => {
                    self.state = ParseState::Ground;
                    InputAction::CsiDispatch(b)
                }
                _ => {
                    self.state = ParseState::Ground;
                    InputAction::Execute(b)
                }
            },
            ParseState::Utf8 { need } => {
                if (0x80..0xc0).contains(&b) {
                    self.utf8[self.utf8_len as usize] = b;
                    self.utf8_len += 1;
                    if need == 1 {
                        self.state = ParseState::Ground;
                        let s = &self.utf8[..self.utf8_len as usize];
                        match std::str::from_utf8(s) {
                            Ok(s) => InputAction::Print(s.chars().next().unwrap_or(' ')),
                            Err(_) => InputAction::Execute(0),
                        }
                    } else {
                        self.state = ParseState::Utf8 { need: need - 1 };
                        InputAction::None
                    }
                } else {
                    // Broken sequence: reprocess this byte from ground.
                    self.state = ParseState::Ground;
                    self.input(b)
                }
            }
        }
    }

    fn start_utf8(&mut self, b: u8, need: u8) {
        self.utf8[0] = b;
        self.utf8_len = 1;
        self.state = ParseState::Utf8 { need };
    }
}

// ---------------------------------------------------------------------------
// The shared overlay buffer: parser + overlay/cursor cells + keystroke
// handlers + render walk. Holds the model-independent epoch/frame counters
// the handlers and cull pass need.

pub(super) struct OverlayBuffer {
    parser: InputParser,
    pub overlays: Vec<OverlayRow>,
    pub cursors: Vec<CursorPrediction>,

    pub local_frame_sent: u64,

    pub prediction_epoch: u64,
    pub confirmed_epoch: u64,

    predict_overwrite: bool,
    /// `become_tentative` bumps the prediction epoch unless this is false
    /// (the Experimental model keeps a single epoch).
    bump_epoch_on_tentative: bool,
}

impl OverlayBuffer {
    pub fn new(predict_overwrite: bool, bump_epoch_on_tentative: bool) -> OverlayBuffer {
        OverlayBuffer {
            parser: InputParser::new(),
            overlays: Vec::new(),
            cursors: Vec::new(),
            local_frame_sent: 0,
            prediction_epoch: 1,
            confirmed_epoch: 0,
            predict_overwrite,
            bump_epoch_on_tentative,
        }
    }

    pub fn set_local_frame_sent(&mut self, x: u64) {
        self.local_frame_sent = x;
    }

    /// Any prediction outstanding at all?
    pub fn active(&self) -> bool {
        !self.cursors.is_empty()
            || self
                .overlays
                .iter()
                .any(|row| row.cells.iter().any(|c| c.active))
    }

    pub fn reset(&mut self) {
        self.cursors.clear();
        self.overlays.clear();
        self.become_tentative();
    }

    pub fn become_tentative(&mut self) {
        if self.bump_epoch_on_tentative {
            self.prediction_epoch += 1;
        }
    }

    fn cursor(&mut self) -> &mut CursorPrediction {
        self.cursors.last_mut().expect("cursor prediction exists")
    }

    fn init_cursor(&mut self, fb: &Snapshot) {
        let fresh_epoch = self
            .cursors
            .last()
            .is_some_and(|c| c.tentative_until_epoch == self.prediction_epoch);
        if fresh_epoch {
            return;
        }
        // Continue from the last predicted position, or seed from the frame.
        let (row, col) = self
            .cursors
            .last()
            .map_or((fb.cursor_row, fb.cursor_col), |c| (c.row, c.col));
        self.cursors.push(CursorPrediction {
            active: true,
            tentative_until_epoch: self.prediction_epoch,
            expiration_frame: self.local_frame_sent + 1,
            row,
            col,
        });
    }

    fn get_or_make_row(&mut self, row_num: u16, num_cols: u16) -> &mut OverlayRow {
        if let Some(idx) = self.overlays.iter().position(|r| r.row_num == row_num) {
            return &mut self.overlays[idx];
        }
        let epoch = self.prediction_epoch;
        self.overlays.push(OverlayRow {
            row_num,
            cells: (0..num_cols).map(|i| OverlayCell::new(i, epoch)).collect(),
        });
        self.overlays.last_mut().unwrap()
    }

    pub fn kill_epoch(&mut self, epoch: u64, fb: &Snapshot) {
        self.cursors.retain(|c| !c.tentative(epoch - 1));
        self.cursors.push(CursorPrediction {
            active: true,
            tentative_until_epoch: self.prediction_epoch,
            expiration_frame: self.local_frame_sent + 1,
            row: fb.cursor_row,
            col: fb.cursor_col,
        });
        for row in self.overlays.iter_mut() {
            for cell in row.cells.iter_mut() {
                if cell.tentative(epoch - 1) {
                    cell.reset();
                }
            }
        }
        self.become_tentative();
    }

    /// Walks the surviving predictions and paints each shown cell + the
    /// cursor through `renderer`. `confirmed_epoch` gates the tentative cells
    /// (the model passes `u64::MAX` to draw everything); `flag` is the
    /// slow-link underline policy.
    pub fn render(
        &self,
        fb: &mut Snapshot,
        renderer: &dyn PredictionRenderer,
        confirmed_epoch: u64,
        flag: bool,
    ) {
        for cursor in &self.cursors {
            cursor.render(fb, renderer, confirmed_epoch);
        }
        for row in &self.overlays {
            for cell in &row.cells {
                cell.render(fb, renderer, confirmed_epoch, row.row_num, flag);
            }
        }
    }

    /// Parses one keystroke byte and applies it to the overlay. Returns true
    /// for a control/escape input that the model may want to treat specially;
    /// the buffer has already bumped the tentative epoch for those.
    ///
    /// The print/backspace/arrow handlers move predictions verbatim from
    /// mosh's PredictionEngine::new_user_byte body.
    pub fn input(&mut self, byte: u8, fb: &Snapshot, now: u64) {
        match self.parser.input(byte) {
            InputAction::Print(ch) => self.handle_print(ch, fb, now),
            InputAction::Execute(b) => {
                if b == 0x0d {
                    self.become_tentative();
                    self.newline_carriage_return(fb, now);
                } else {
                    self.become_tentative();
                }
            }
            InputAction::EscDispatch => self.become_tentative(),
            InputAction::CsiDispatch(final_byte) => match final_byte {
                b'C' => {
                    // Right arrow.
                    self.init_cursor(fb);
                    if self.cursor().col + 1 < fb.cols {
                        let expiration = self.local_frame_sent + 1;
                        let c = self.cursor();
                        c.col += 1;
                        c.expiration_frame = expiration;
                    }
                }
                b'D' => {
                    // Left arrow.
                    self.init_cursor(fb);
                    if self.cursor().col > 0 {
                        let expiration = self.local_frame_sent + 1;
                        let c = self.cursor();
                        c.col -= 1;
                        c.expiration_frame = expiration;
                    }
                }
                _ => self.become_tentative(),
            },
            InputAction::None => {}
        }
    }

    fn handle_print(&mut self, ch: char, fb: &Snapshot, now: u64) {
        if ch == '\u{7f}' {
            self.handle_backspace(fb, now);
            return;
        }
        if (ch as u32) < 0x20 || wcwidth(ch) != 1 {
            // Unknown or wide print: don't try to predict it.
            self.become_tentative();
            return;
        }

        self.init_cursor(fb);
        let (cur_row, cur_col) = {
            let c = self.cursor();
            (c.row, c.col)
        };
        if cur_row >= fb.rows || cur_col >= fb.cols {
            return;
        }

        let expiration = self.local_frame_sent + 1;
        let width = fb.cols;

        if cur_col + 1 >= width {
            // Prediction in the last column is tricky (wrap behavior
            // differs between applications).
            self.become_tentative();
        }
        let epoch_after = self.prediction_epoch;

        let predict_overwrite = self.predict_overwrite;
        let row = self.get_or_make_row(cur_row, width);

        // Shift the rest of the row right (insert), unless overwriting.
        let rightmost = if predict_overwrite {
            cur_col
        } else {
            width - 1
        };
        let mut i = rightmost;
        while i > cur_col {
            let prev_state = {
                let prev_cell = &row.cells[(i - 1) as usize];
                if prev_cell.active {
                    if prev_cell.unknown {
                        None // unknown propagates
                    } else {
                        Some(prev_cell.replacement.clone())
                    }
                } else {
                    fb.cell(cur_row, i - 1).cloned()
                }
            };
            let orig = fb.cell(cur_row, i).cloned().unwrap_or_else(blank_cell);
            let cell = &mut row.cells[i as usize];
            cell.reset_with_orig();
            cell.active = true;
            cell.tentative_until_epoch = epoch_after;
            cell.expire(expiration, now);
            cell.original_contents.push(orig);
            if i == width - 1 {
                cell.unknown = true;
            } else {
                match prev_state {
                    None => cell.unknown = true,
                    Some(replacement) => {
                        cell.unknown = false;
                        cell.replacement = replacement;
                    }
                }
            }
            i -= 1;
        }

        // The predicted glyph itself; renditions copy the left neighbor.
        let style = if cur_col > 0 {
            let prev_overlay = &row.cells[(cur_col - 1) as usize];
            if prev_overlay.active && !prev_overlay.unknown {
                prev_overlay.replacement.style
            } else {
                fb.cell(cur_row, cur_col - 1)
                    .map(|c| c.style)
                    .unwrap_or_default()
            }
        } else {
            Style::default()
        };
        let orig = fb
            .cell(cur_row, cur_col)
            .cloned()
            .unwrap_or_else(blank_cell);
        let cell = &mut row.cells[cur_col as usize];
        cell.reset_with_orig();
        cell.active = true;
        cell.tentative_until_epoch = epoch_after;
        cell.expire(expiration, now);
        cell.unknown = false;
        cell.replacement = Cell {
            ch,
            style,
            width: 1,
            ..Cell::default()
        };
        cell.original_contents.push(orig);

        self.cursor().expiration_frame = expiration;

        // Advance (or wrap) the predicted cursor.
        if cur_col + 1 < width {
            self.cursor().col = cur_col + 1;
        } else {
            self.become_tentative();
            self.newline_carriage_return(fb, now);
        }
    }

    fn handle_backspace(&mut self, fb: &Snapshot, now: u64) {
        self.init_cursor(fb);
        let (cur_row, cur_col) = {
            let c = self.cursor();
            (c.row, c.col)
        };
        if cur_col == 0 {
            return;
        }
        let expiration = self.local_frame_sent + 1;
        {
            let c = self.cursor();
            c.col -= 1;
            c.expiration_frame = expiration;
        }
        let new_col = cur_col - 1;
        let epoch = self.prediction_epoch;
        let width = fb.cols;
        let predict_overwrite = self.predict_overwrite;
        let row = self.get_or_make_row(cur_row, width);

        if predict_overwrite {
            let orig = fb
                .cell(cur_row, new_col)
                .cloned()
                .unwrap_or_else(blank_cell);
            let cell = &mut row.cells[new_col as usize];
            cell.reset_with_orig();
            cell.active = true;
            cell.tentative_until_epoch = epoch;
            cell.expire(expiration, now);
            let mut replacement = orig.clone();
            cell.original_contents.push(orig);
            replacement.ch = ' ';
            replacement.extra.clear();
            replacement.width = 1;
            cell.replacement = replacement;
            return;
        }

        // Shift the rest of the row left.
        for i in new_col..width {
            let next_state = if i + 2 < width {
                let next_cell = &row.cells[(i + 1) as usize];
                if next_cell.active {
                    if next_cell.unknown {
                        None
                    } else {
                        Some(next_cell.replacement.clone())
                    }
                } else {
                    fb.cell(cur_row, i + 1).cloned()
                }
            } else {
                None // last columns are unknown
            };
            let orig = fb.cell(cur_row, i).cloned().unwrap_or_else(blank_cell);
            let cell = &mut row.cells[i as usize];
            cell.reset_with_orig();
            cell.active = true;
            cell.tentative_until_epoch = epoch;
            cell.expire(expiration, now);
            cell.original_contents.push(orig);
            match next_state {
                None => cell.unknown = true,
                Some(replacement) => {
                    cell.unknown = false;
                    cell.replacement = replacement;
                }
            }
        }
    }

    fn newline_carriage_return(&mut self, fb: &Snapshot, now: u64) {
        self.init_cursor(fb);
        self.cursor().col = 0;
        if self.cursor().row == fb.rows - 1 {
            // Don't predict the scroll; make a blank prediction for the
            // last row instead.
            let epoch = self.prediction_epoch;
            let expiration = self.local_frame_sent + 1;
            let cur_row = self.cursor().row;
            let row = self.get_or_make_row(cur_row, fb.cols);
            for cell in row.cells.iter_mut() {
                cell.active = true;
                cell.tentative_until_epoch = epoch;
                cell.expire(expiration, now);
                cell.unknown = false;
                cell.replacement = blank_cell();
            }
        } else {
            self.cursor().row += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An active prediction past its expiration frame, so `late_ack = 0` makes
    /// it eligible for crediting (not `Pending`).
    fn ready_cell(col: u16) -> OverlayCell {
        let mut c = OverlayCell::new(col, 1);
        c.active = true;
        c.expiration_frame = 0;
        c
    }

    #[test]
    fn nocredit_attributes_unknown_cell() {
        // The cursor cell (last cell of a print) is `unknown` => no credit,
        // attributed to the Unknown branch (#predict-echo).
        let fb = Snapshot::blank(24, 80);
        let mut c = ready_cell(2);
        c.unknown = true;
        assert_eq!(
            c.get_validity(&fb, 0, 0),
            Validity::CorrectNoCredit(NoCreditReason::Unknown)
        );
    }

    #[test]
    fn nocredit_attributes_blank_replacement() {
        // A blank prediction is too weak to credit => Blank branch.
        let fb = Snapshot::blank(24, 80);
        let mut c = ready_cell(2);
        c.unknown = false;
        c.replacement = blank_cell();
        assert_eq!(
            c.get_validity(&fb, 0, 0),
            Validity::CorrectNoCredit(NoCreditReason::Blank)
        );
    }
}
