//! Speculative local echo (port of mosh's PredictionEngine from
//! terminaloverlay.cc): keystrokes are echoed locally as overlay cells that
//! belong to epochs, displayed according to adaptive RTT/glitch triggers,
//! and confirmed or culled against acknowledged server frames.
//!
//! Frame numbers from mosh map onto the reliable input stream's byte
//! offsets: a prediction made for the byte at offset B expires at B+1
//! (the server's ack of B+1 means it consumed that byte), the "acked"
//! counter is the frame's `input_ack`, and the "late acked" counter is the
//! frame's `echo_ack` (state reflecting the application's echo).

use posh_term::{wcwidth, Cell, Style, UnderlineStyle};

use crate::remote::display::{blank_cell, Snapshot};

// Timing constants, verbatim from mosh terminaloverlay.h.
const SRTT_TRIGGER_LOW: u64 = 20; // <= ms cures the SRTT trigger
const SRTT_TRIGGER_HIGH: u64 = 30; // > ms starts the SRTT trigger
const FLAG_TRIGGER_LOW: u64 = 50; // <= ms cures flagging
const FLAG_TRIGGER_HIGH: u64 = 80; // > ms starts flagging
pub const GLITCH_THRESHOLD: u64 = 250; // prediction outstanding this long is a glitch
pub const GLITCH_REPAIR_COUNT: u32 = 10; // non-glitches required to cure the trigger
const GLITCH_REPAIR_MININTERVAL: u64 = 150; // ms between counted non-glitches
pub const GLITCH_FLAG_THRESHOLD: u64 = 5000; // outstanding this long => underline

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPreference {
    Always,
    Never,
    Adaptive,
    Experimental,
}

impl DisplayPreference {
    /// Parses $POSH_PREDICTION (mosh: $MOSH_PREDICTION_DISPLAY).
    pub fn parse(value: Option<&str>) -> Result<DisplayPreference, String> {
        match value {
            None | Some("") | Some("adaptive") => Ok(DisplayPreference::Adaptive),
            Some("always") => Ok(DisplayPreference::Always),
            Some("never") => Ok(DisplayPreference::Never),
            Some("experimental") => Ok(DisplayPreference::Experimental),
            Some(other) => Err(format!("unknown POSH_PREDICTION setting ({other})")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Validity {
    Pending,
    Correct,
    CorrectNoCredit,
    IncorrectOrExpired,
    Inactive,
}

/// mosh Cell::contents_match: glyphs match, ignoring renditions.
fn contents_match(a: &Cell, b: &Cell) -> bool {
    (a.is_blank() && b.is_blank()) || (a.ch == b.ch && a.extra == b.extra)
}

#[derive(Debug, Clone)]
struct OverlayCell {
    active: bool,
    tentative_until_epoch: u64,
    expiration_frame: u64,
    prediction_time: u64,
    col: u16,
    unknown: bool,
    replacement: Cell,
    /// No credit for predictions that match what was already there.
    original_contents: Vec<Cell>,
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

    fn tentative(&self, confirmed_epoch: u64) -> bool {
        self.tentative_until_epoch > confirmed_epoch
    }

    fn reset(&mut self) {
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

    fn get_validity(&self, fb: &Snapshot, row: u16, late_ack: u64) -> Validity {
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
            return Validity::CorrectNoCredit;
        }
        if self.replacement.is_blank() {
            // Too easy for this to trigger falsely.
            return Validity::CorrectNoCredit;
        }
        let current = fb.cell(row, self.col).expect("cell in range");
        if contents_match(current, &self.replacement) {
            if self
                .original_contents
                .iter()
                .any(|c| contents_match(c, &self.replacement))
            {
                return Validity::CorrectNoCredit;
            }
            return Validity::Correct;
        }
        Validity::IncorrectOrExpired
    }

    fn apply(&self, fb: &mut Snapshot, confirmed_epoch: u64, row: u16, mut flag: bool) {
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
            if flag && self.col != fb.cols - 1 {
                if let Some(cell) = fb.cell_mut(row, self.col) {
                    cell.style.underline = UnderlineStyle::Single;
                }
            }
            return;
        }
        let differs = fb.cell(row, self.col) != Some(&self.replacement);
        if differs {
            if let Some(cell) = fb.cell_mut(row, self.col) {
                *cell = self.replacement.clone();
                if flag {
                    cell.style.underline = UnderlineStyle::Single;
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct OverlayRow {
    row_num: u16,
    cells: Vec<OverlayCell>,
}

#[derive(Debug, Clone)]
struct CursorPrediction {
    active: bool,
    tentative_until_epoch: u64,
    expiration_frame: u64,
    row: u16,
    col: u16,
}

impl CursorPrediction {
    fn tentative(&self, confirmed_epoch: u64) -> bool {
        self.tentative_until_epoch > confirmed_epoch
    }

    fn get_validity(&self, fb: &Snapshot, late_ack: u64) -> Validity {
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

    fn apply(&self, fb: &mut Snapshot, confirmed_epoch: u64) {
        if !self.active || self.tentative(confirmed_epoch) {
            return;
        }
        if self.row < fb.rows && self.col < fb.cols {
            fb.cursor_row = self.row;
            fb.cursor_col = self.col;
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
// The engine.

pub struct PredictionEngine {
    parser: InputParser,
    overlays: Vec<OverlayRow>,
    cursors: Vec<CursorPrediction>,

    local_frame_sent: u64,
    local_frame_acked: u64,
    local_frame_late_acked: u64,

    prediction_epoch: u64,
    confirmed_epoch: u64,

    flagging: bool,      // underline displayed predictions
    srtt_trigger: bool,  // show predictions because of slow RTT
    glitch_trigger: u32, // show predictions because one took too long
    last_quick_confirmation: u64,
    send_interval: u64,

    last_height: u16,
    last_width: u16,

    display_preference: DisplayPreference,
    predict_overwrite: bool,
}

impl PredictionEngine {
    pub fn new(display_preference: DisplayPreference, predict_overwrite: bool) -> PredictionEngine {
        PredictionEngine {
            parser: InputParser::new(),
            overlays: Vec::new(),
            cursors: Vec::new(),
            local_frame_sent: 0,
            local_frame_acked: 0,
            local_frame_late_acked: 0,
            prediction_epoch: 1,
            confirmed_epoch: 0,
            flagging: false,
            srtt_trigger: false,
            glitch_trigger: 0,
            last_quick_confirmation: 0,
            send_interval: 250,
            last_height: 0,
            last_width: 0,
            display_preference,
            predict_overwrite,
        }
    }

    pub fn set_local_frame_sent(&mut self, x: u64) {
        self.local_frame_sent = x;
    }

    pub fn set_local_frame_acked(&mut self, x: u64) {
        self.local_frame_acked = x;
    }

    pub fn set_local_frame_late_acked(&mut self, x: u64) {
        self.local_frame_late_acked = x;
    }

    pub fn set_send_interval(&mut self, x: u64) {
        self.send_interval = x;
    }

    #[cfg(test)]
    pub fn flagging(&self) -> bool {
        self.flagging
    }

    #[cfg(test)]
    pub fn glitch_trigger(&self) -> u32 {
        self.glitch_trigger
    }

    #[cfg(test)]
    pub fn srtt_trigger_on(&self) -> bool {
        self.srtt_trigger
    }

    #[cfg(test)]
    pub fn confirmed_epoch(&self) -> u64 {
        self.confirmed_epoch
    }

    /// Any prediction outstanding at all?
    pub fn active(&self) -> bool {
        !self.cursors.is_empty()
            || self
                .overlays
                .iter()
                .any(|row| row.cells.iter().any(|c| c.active))
    }

    /// True when timing-based triggers may still fire: the caller should
    /// poll with a short (50ms) timeout so glitches get detected.
    pub fn needs_timer(&self) -> bool {
        self.active() && !(self.glitch_trigger > 0 && self.flagging)
    }

    fn shown(&self) -> bool {
        match self.display_preference {
            DisplayPreference::Never => false,
            DisplayPreference::Always | DisplayPreference::Experimental => true,
            DisplayPreference::Adaptive => self.srtt_trigger || self.glitch_trigger > 0,
        }
    }

    pub fn reset(&mut self) {
        self.cursors.clear();
        self.overlays.clear();
        self.become_tentative();
    }

    fn become_tentative(&mut self) {
        if self.display_preference != DisplayPreference::Experimental {
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

    fn kill_epoch(&mut self, epoch: u64, fb: &Snapshot) {
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

    /// Overlays the surviving predictions onto a framebuffer snapshot.
    pub fn apply(&self, fb: &mut Snapshot) {
        if !self.shown() {
            return;
        }
        for cursor in &self.cursors {
            cursor.apply(fb, self.confirmed_epoch);
        }
        for row in &self.overlays {
            for cell in &row.cells {
                cell.apply(fb, self.confirmed_epoch, row.row_num, self.flagging);
            }
        }
    }

    /// Validates predictions against the latest server framebuffer:
    /// confirms (retiring), culls mismatches, and updates the adaptive
    /// display triggers. Port of PredictionEngine::cull.
    pub fn cull(&mut self, fb: &Snapshot, now: u64) {
        if self.display_preference == DisplayPreference::Never {
            return;
        }

        if self.last_height != fb.rows || self.last_width != fb.cols {
            self.last_height = fb.rows;
            self.last_width = fb.cols;
            self.reset();
        }

        // SRTT trigger with hysteresis.
        if self.send_interval > SRTT_TRIGGER_HIGH {
            self.srtt_trigger = true;
        } else if self.srtt_trigger && self.send_interval <= SRTT_TRIGGER_LOW && !self.active() {
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
        let mut confirmed_epoch = self.confirmed_epoch;
        let mut glitch_trigger = self.glitch_trigger;
        let mut last_quick = self.last_quick_confirmation;
        let experimental = self.display_preference == DisplayPreference::Experimental;

        self.overlays.retain(|row| row.row_num < fb.rows);
        'rows: for row in self.overlays.iter_mut() {
            let row_num = row.row_num;
            for j in 0..row.cells.len() {
                let validity = row.cells[j].get_validity(fb, row_num, late_ack);
                match validity {
                    Validity::IncorrectOrExpired => {
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
                    Validity::CorrectNoCredit => {
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

        self.confirmed_epoch = confirmed_epoch;
        self.glitch_trigger = glitch_trigger;
        self.last_quick_confirmation = last_quick;

        if do_reset {
            self.reset();
            return;
        }
        for epoch in kill_epochs {
            self.kill_epoch(epoch, fb);
        }

        // Cursor predictions.
        let cursor_wrong = self
            .cursors
            .last()
            .map(|c| c.get_validity(fb, late_ack) == Validity::IncorrectOrExpired)
            .unwrap_or(false);
        if cursor_wrong {
            if experimental {
                self.cursors.clear();
            } else {
                self.reset();
                return;
            }
        }
        self.cursors
            .retain(|c| c.get_validity(fb, late_ack) == Validity::Pending);
    }

    /// Feeds one user keystroke byte; `fb` is the locally displayed frame.
    /// Port of PredictionEngine::new_user_byte.
    pub fn new_user_byte(&mut self, byte: u8, fb: &Snapshot, now: u64) {
        if self.display_preference == DisplayPreference::Never {
            return;
        }
        if self.display_preference == DisplayPreference::Experimental {
            self.prediction_epoch = self.confirmed_epoch;
        }

        self.cull(fb, now);

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
    use posh_term::Terminal;

    fn snapshot(rows: u16, cols: u16, bytes: &[u8]) -> Snapshot {
        let mut t = Terminal::with_scrollback(rows, cols, 0);
        t.process(bytes);
        Snapshot::from_term(&t)
    }

    fn engine(pref: DisplayPreference) -> PredictionEngine {
        PredictionEngine::new(pref, false)
    }

    fn shown_char(fb: &Snapshot, row: u16, col: u16) -> char {
        let c = fb.cell(row, col).unwrap();
        if c.ch == '\0' {
            ' '
        } else {
            c.ch
        }
    }

    #[test]
    fn display_preference_parsing() {
        assert_eq!(
            DisplayPreference::parse(None),
            Ok(DisplayPreference::Adaptive)
        );
        assert_eq!(
            DisplayPreference::parse(Some("always")),
            Ok(DisplayPreference::Always)
        );
        assert_eq!(
            DisplayPreference::parse(Some("never")),
            Ok(DisplayPreference::Never)
        );
        assert_eq!(
            DisplayPreference::parse(Some("experimental")),
            Ok(DisplayPreference::Experimental)
        );
        assert!(DisplayPreference::parse(Some("sometimes")).is_err());
    }

    #[test]
    fn never_preference_predicts_nothing() {
        let mut eng = engine(DisplayPreference::Never);
        let fb = snapshot(5, 20, b"$ ");
        eng.new_user_byte(b'x', &fb, 100);
        assert!(!eng.active());
    }

    #[test]
    fn prediction_is_tentative_until_epoch_confirmed() {
        // mosh starts in prediction epoch 1 with confirmed epoch 0: brand-new
        // predictions stay hidden until the server confirms one.
        let mut eng = engine(DisplayPreference::Always);
        let fb = snapshot(5, 20, b"$ ");
        eng.set_local_frame_sent(0);
        eng.new_user_byte(b'x', &fb, 100);
        assert!(eng.active());

        let mut overlaid = fb.clone();
        eng.apply(&mut overlaid);
        assert_eq!(shown_char(&overlaid, 0, 2), ' ', "tentative: not drawn");

        // Server confirms: echo ack covers the byte and the cell matches.
        let confirmed = snapshot(5, 20, b"$ x");
        eng.set_local_frame_late_acked(1);
        eng.cull(&confirmed, 150);
        assert_eq!(
            eng.confirmed_epoch(),
            eng.prediction_epoch,
            "confirmation caught the prediction epoch up"
        );

        // The next prediction in the confirmed epoch is displayed.
        eng.set_local_frame_sent(1);
        eng.new_user_byte(b'y', &confirmed, 200);
        let mut overlaid = confirmed.clone();
        eng.apply(&mut overlaid);
        assert_eq!(shown_char(&overlaid, 0, 3), 'y');
        // ... and the cursor prediction advanced with it.
        assert_eq!(overlaid.cursor_col, 4);
    }

    /// Drives an engine to a confirmed epoch so subsequent predictions
    /// render immediately.
    fn confirmed_engine(pref: DisplayPreference, fb_bytes: &[u8]) -> (PredictionEngine, Snapshot) {
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
            eng.prediction_epoch,
            "warmup prediction confirmed"
        );
        eng.set_local_frame_sent(1);
        (eng, confirmed)
    }

    #[test]
    fn mismatch_culls_all_predictions() {
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Always, b"$ ");
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
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Always, b"$ ");
        eng.new_user_byte(b'a', &fb, 100);
        let mut overlaid = fb.clone();
        eng.apply(&mut overlaid);
        assert_eq!(shown_char(&overlaid, 0, 3), 'a', "epoch-1 prediction shown");

        // Ctrl-T (random control byte) bumps the tentative epoch.
        eng.new_user_byte(0x14, &fb, 110);
        eng.new_user_byte(b'b', &fb, 120);
        let mut overlaid = fb.clone();
        eng.apply(&mut overlaid);
        assert_eq!(shown_char(&overlaid, 0, 3), 'a', "old epoch still shown");
        assert_eq!(
            shown_char(&overlaid, 0, 4),
            ' ',
            "post-control prediction is tentative and hidden"
        );
    }

    #[test]
    fn escape_sequence_bumps_epoch() {
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Always, b"$ ");
        let before = eng.prediction_epoch;
        // Up arrow: ESC [ A.
        for b in b"\x1b[A" {
            eng.new_user_byte(*b, &fb, 100);
        }
        assert!(eng.prediction_epoch > before, "CSI A made input tentative");
    }

    #[test]
    fn arrow_keys_move_predicted_cursor() {
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Always, b"$ abc");
        let start_col = fb.cursor_col;
        for b in b"\x1b[D" {
            eng.new_user_byte(*b, &fb, 100);
        }
        let mut overlaid = fb.clone();
        eng.apply(&mut overlaid);
        assert_eq!(overlaid.cursor_col, start_col - 1, "left arrow predicted");
        // ESC O C (application mode right arrow) is translated like CSI C.
        for b in b"\x1bOC" {
            eng.new_user_byte(*b, &fb, 110);
        }
        let mut overlaid = fb.clone();
        eng.apply(&mut overlaid);
        assert_eq!(overlaid.cursor_col, start_col, "right arrow predicted");
    }

    #[test]
    fn backspace_predicts_erase() {
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Always, b"$ ab");
        // fb shows "$ abq" with cursor after 'q'.
        let col = fb.cursor_col;
        eng.new_user_byte(0x7f, &fb, 100);
        let mut overlaid = fb.clone();
        eng.apply(&mut overlaid);
        assert_eq!(overlaid.cursor_col, col - 1, "cursor moved back");
        assert_eq!(
            shown_char(&overlaid, 0, col - 1),
            ' ',
            "erased cell predicted blank (shifted from the right)"
        );
    }

    #[test]
    fn correct_prediction_is_retired_without_glitch() {
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Always, b"$ ");
        eng.new_user_byte(b'x', &fb, 100);
        let mut t = Terminal::with_scrollback(5, 20, 0);
        t.process(b"$ qx");
        let echoed = Snapshot::from_term(&t);
        eng.set_local_frame_late_acked(2);
        eng.cull(&echoed, 150);
        assert_eq!(eng.glitch_trigger(), 0);
        // The cell prediction is retired; only nothing or cursor remains.
        let mut overlaid = echoed.clone();
        eng.apply(&mut overlaid);
        assert_eq!(shown_char(&overlaid, 0, 3), 'x', "real cell, no overlay");
    }

    #[test]
    fn glitch_trigger_fires_after_250ms_pending() {
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Adaptive, b"$ ");
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
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Adaptive, b"$ ");
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
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Adaptive, b"$ ");
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
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Adaptive, b"$ ");
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
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Always, b"$ ");
        eng.set_send_interval(100); // flagging on
        eng.new_user_byte(b'z', &fb, 100);
        let mut overlaid = fb.clone();
        eng.apply(&mut overlaid);
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
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Adaptive, b"$ ");
        eng.set_send_interval(10);
        eng.cull(&fb, 50);
        eng.new_user_byte(b'x', &fb, 100);
        let mut overlaid = fb.clone();
        eng.apply(&mut overlaid);
        assert_eq!(
            shown_char(&overlaid, 0, fb.cursor_col),
            ' ',
            "fast link: predictions exist but are not displayed"
        );
        assert!(eng.active());
    }

    #[test]
    fn newline_predicts_cursor_motion() {
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Always, b"$ hi");
        eng.new_user_byte(0x0d, &fb, 100);
        // CR predictions are tentative (epoch bumped); confirm the epoch by
        // checking the internal cursor moved.
        let c = eng.cursors.last().unwrap();
        assert_eq!(c.col, 0);
        assert_eq!(c.row, fb.cursor_row + 1);
    }

    #[test]
    fn newline_on_last_row_predicts_blank_row_not_scroll() {
        let (mut eng, _) = confirmed_engine(DisplayPreference::Always, b"$ ");
        let mut t = Terminal::with_scrollback(5, 20, 0);
        t.process(b"1\r\n2\r\n3\r\n4\r\n$ q");
        let fb = Snapshot::from_term(&t);
        assert_eq!(fb.cursor_row, fb.rows - 1);
        eng.new_user_byte(0x0d, &fb, 100);
        let c = eng.cursors.last().unwrap();
        assert_eq!(c.row, fb.rows - 1, "no scroll prediction");
        assert_eq!(c.col, 0);
        // The last row has a blank prediction registered.
        assert!(eng
            .overlays
            .iter()
            .any(|r| r.row_num == fb.rows - 1 && r.cells.iter().all(|c| c.active)));
    }

    #[test]
    fn resize_resets_predictions() {
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Always, b"$ ");
        eng.new_user_byte(b'x', &fb, 100);
        assert!(eng.active());
        let bigger = snapshot(10, 40, b"$ ");
        eng.cull(&bigger, 200);
        assert!(!eng.active(), "size change resets the engine");
    }

    #[test]
    fn utf8_input_predicted_as_single_char() {
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Always, b"$ ");
        for b in "é".as_bytes() {
            eng.new_user_byte(*b, &fb, 100);
        }
        let mut overlaid = fb.clone();
        eng.apply(&mut overlaid);
        assert_eq!(shown_char(&overlaid, 0, fb.cursor_col), 'é');
    }

    #[test]
    fn insert_shifts_existing_text_right() {
        // Cursor placed in the middle of existing text.
        let (mut eng, _) = confirmed_engine(DisplayPreference::Always, b"$ ");
        let mut t = Terminal::with_scrollback(5, 20, 0);
        t.process(b"$ qworld\x1b[1;4H"); // cursor on 'w'
        let fb = Snapshot::from_term(&t);
        eng.new_user_byte(b'X', &fb, 100);
        let mut overlaid = fb.clone();
        eng.apply(&mut overlaid);
        assert_eq!(shown_char(&overlaid, 0, 3), 'X');
        assert_eq!(shown_char(&overlaid, 0, 4), 'w', "text shifted right");
        assert_eq!(shown_char(&overlaid, 0, 5), 'o');
    }

    #[test]
    fn correct_confirmation_requires_late_ack() {
        let (mut eng, fb) = confirmed_engine(DisplayPreference::Always, b"$ ");
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
            .overlays
            .iter()
            .any(|r| r.cells.iter().any(|c| c.active));
        assert!(!still_cell_active, "ack retires the cell prediction");
    }
}
