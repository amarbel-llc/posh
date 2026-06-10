//! The terminal facade: parser -> dispatch -> screen.

use std::collections::HashMap;

use crate::cell::{default_palette, Cell, Style};
use crate::graphics::{AnimationState, Frame, GraphicsState, Image, Placement};
use crate::kitty_keys::{KittyFlags, KittyKeyStack};
use crate::modes::{Modes, MouseMode, MouseProtocol};
use crate::parser::{Action, Parser};
use crate::screen::{Screen, SemanticMark};
use crate::wcwidth::wcwidth;

/// Cursor shape as set by DECSCUSR (blink variants are reported separately
/// via [`Terminal::cursor_blink`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

/// Public cursor snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
    pub shape: CursorShape,
}

/// G0/G1 designated character set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Charset {
    #[default]
    Ascii,
    /// DEC Special Graphics (line drawing), `ESC ( 0`.
    DecSpecial,
    /// UK national, `ESC ( A`.
    Uk,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CursorState {
    pub row: u16,
    pub col: u16,
    pub pending_wrap: bool,
    pub style: Style,
    pub hyperlink: u32,
    pub g0: Charset,
    pub g1: Charset,
    /// 0 = G0 active (SI), 1 = G1 active (SO).
    pub shift: u8,
}

/// One XTPUSHCOLORS stack entry: palette plus dynamic colors.
#[derive(Debug, Clone)]
pub(crate) struct ColorStackEntry {
    pub palette: [(u8, u8, u8); 256],
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
    pub cursor: Option<(u8, u8, u8)>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SavedCursor {
    pub cursor: CursorState,
    pub origin: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Hyperlink {
    pub id: String,
    pub uri: String,
}

#[derive(Debug)]
pub struct Terminal {
    parser: Parser,
    action_buf: Vec<Action>,
    pub(crate) primary: Screen,
    pub(crate) alt: Screen,
    pub(crate) alt_active: bool,
    pub(crate) cursor: CursorState,
    pub(crate) saved_primary: SavedCursor,
    pub(crate) saved_alt: SavedCursor,
    pub(crate) modes: Modes,
    /// Scroll region, 0-based inclusive.
    pub(crate) scroll_top: u16,
    pub(crate) scroll_bot: u16,
    pub(crate) tabs: Vec<bool>,
    pub(crate) title: String,
    pub(crate) icon_title: String,
    pub(crate) generation: u64,
    pub(crate) responses: Vec<u8>,
    pub(crate) bell_count: u64,
    pub(crate) pwd: String,
    pub(crate) palette: [(u8, u8, u8); 256],
    pub(crate) fg_color: Option<(u8, u8, u8)>,
    pub(crate) bg_color: Option<(u8, u8, u8)>,
    pub(crate) cursor_color: Option<(u8, u8, u8)>,
    pub(crate) clipboard: Vec<u8>,
    /// OSC 52 `p` (primary) and `s` (select) slots; `clipboard` is `c`.
    pub(crate) primary_selection: Vec<u8>,
    pub(crate) select_selection: Vec<u8>,
    pub(crate) color_stack: Vec<ColorStackEntry>,
    /// Raw metadata + text of the last OSC 66 (kitty text sizing).
    pub(crate) last_text_size: Option<String>,
    pub(crate) hyperlinks: HashMap<u32, Hyperlink>,
    pub(crate) next_hyperlink: u32,
    /// Raw DECSCUSR parameter (0..=6).
    pub(crate) cursor_style_raw: u16,
    pub(crate) kitty_primary: KittyKeyStack,
    pub(crate) kitty_alt: KittyKeyStack,
    pub(crate) graphics: GraphicsState,
    pub(crate) last_notification: Option<String>,
    pub(crate) pointer_shape: String,
    pub(crate) last_printed: Option<char>,
}

impl Terminal {
    pub fn new(rows: u16, cols: u16) -> Terminal {
        Terminal::with_scrollback(rows, cols, 10_000)
    }

    pub fn with_scrollback(rows: u16, cols: u16, scrollback: usize) -> Terminal {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Terminal {
            parser: Parser::new(),
            action_buf: Vec::new(),
            primary: Screen::new(rows, cols, scrollback),
            alt: Screen::new(rows, cols, 0),
            alt_active: false,
            cursor: CursorState::default(),
            saved_primary: SavedCursor::default(),
            saved_alt: SavedCursor::default(),
            modes: Modes::default(),
            scroll_top: 0,
            scroll_bot: rows - 1,
            tabs: default_tabs(cols),
            title: String::new(),
            icon_title: String::new(),
            generation: 0,
            responses: Vec::new(),
            bell_count: 0,
            pwd: String::new(),
            palette: default_palette(),
            fg_color: None,
            bg_color: None,
            cursor_color: None,
            clipboard: Vec::new(),
            primary_selection: Vec::new(),
            select_selection: Vec::new(),
            color_stack: Vec::new(),
            last_text_size: None,
            hyperlinks: HashMap::new(),
            next_hyperlink: 0,
            cursor_style_raw: 0,
            kitty_primary: KittyKeyStack::default(),
            kitty_alt: KittyKeyStack::default(),
            graphics: GraphicsState::default(),
            last_notification: None,
            pointer_shape: String::new(),
            last_printed: None,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        let mut parser = std::mem::take(&mut self.parser);
        let mut actions = std::mem::take(&mut self.action_buf);
        for &b in bytes {
            parser.advance(b, &mut actions);
            for action in actions.drain(..) {
                self.dispatch(action);
            }
        }
        self.parser = parser;
        self.action_buf = actions;
    }

    fn dispatch(&mut self, action: Action) {
        match action {
            Action::Print(c) => self.print(c),
            Action::Execute(b) => self.execute(b),
            Action::Csi {
                params,
                intermediates,
                private,
                final_byte,
            } => self.csi_dispatch(&params, &intermediates, private, final_byte),
            Action::Esc {
                intermediates,
                final_byte,
            } => self.esc_dispatch(&intermediates, final_byte),
            Action::Osc { data, bel } => self.osc_dispatch(&data, bel),
            Action::Dcs {
                params,
                intermediates,
                final_byte,
                data,
            } => self.dcs_dispatch(&params, &intermediates, final_byte, &data),
            Action::Apc { data } => self.apc_dispatch(&data),
        }
    }

    // --- screen plumbing ---------------------------------------------------

    pub(crate) fn scr(&self) -> &Screen {
        if self.alt_active {
            &self.alt
        } else {
            &self.primary
        }
    }

    pub(crate) fn scr_mut(&mut self) -> &mut Screen {
        if self.alt_active {
            &mut self.alt
        } else {
            &mut self.primary
        }
    }

    /// Erase style: background-color-erase keeps the pen's background.
    pub(crate) fn blank_style(&self) -> Style {
        Style {
            bg: self.cursor.style.bg,
            ..Style::default()
        }
    }

    pub(crate) fn touch(&mut self) {
        self.generation += 1;
    }

    pub(crate) fn respond(&mut self, s: &str) {
        self.responses.extend_from_slice(s.as_bytes());
    }

    /// Scroll region clamped to the current grid.
    pub(crate) fn region(&self) -> (u16, u16) {
        let rows = self.rows();
        (self.scroll_top.min(rows - 1), self.scroll_bot.min(rows - 1))
    }

    // --- printing ----------------------------------------------------------

    pub(crate) fn print(&mut self, c: char) {
        let cp = c as u32;
        // Decoded C1 codepoints (e.g. via 0xC2 0x85) act as controls.
        if (0x80..0xA0).contains(&cp) {
            self.execute(cp as u8);
            return;
        }
        let c = self.map_charset(c);
        let w = wcwidth(c);
        if w == 0 {
            self.combine(c);
            return;
        }
        self.touch();
        self.last_printed = Some(c);
        let cols = self.cols();

        if self.cursor.pending_wrap {
            self.cursor.pending_wrap = false;
            if self.modes.autowrap {
                let row = self.cursor.row;
                self.scr_mut().row_mut(row).wrapped = true;
                self.cursor.col = 0;
                self.index();
            }
        }
        if w == 2 && self.cursor.col + 1 >= cols {
            // Wide char does not fit: blank the last cell (acts as a spacer
            // head) and wrap, or back up when autowrap is off.
            if self.modes.autowrap {
                let style = self.blank_style();
                let (row, col) = (self.cursor.row, self.cursor.col);
                *self.scr_mut().cell_mut(row, col) = Cell::blank(style);
                self.scr_mut().row_mut(row).wrapped = true;
                self.cursor.col = 0;
                self.index();
            } else if cols >= 2 {
                self.cursor.col = cols - 2;
            } else {
                return;
            }
        }

        let (row, col) = (self.cursor.row, self.cursor.col);
        if self.modes.insert {
            let blank = Cell::blank(self.blank_style());
            let r = self.scr_mut().row_mut(row);
            for _ in 0..w {
                r.cells.pop();
                r.cells.insert(col as usize, blank.clone());
            }
        }
        self.clean_wide(row, col, w);
        let style = self.cursor.style;
        let link = self.cursor.hyperlink;
        *self.scr_mut().cell_mut(row, col) = Cell {
            ch: c,
            style,
            width: w,
            extra: Vec::new(),
            hyperlink: link,
        };
        if w == 2 {
            *self.scr_mut().cell_mut(row, col + 1) = Cell {
                ch: '\0',
                style,
                width: 0,
                extra: Vec::new(),
                hyperlink: link,
            };
        }
        let new_col = col + u16::from(w);
        if new_col >= cols {
            self.cursor.col = cols - 1;
            if self.modes.autowrap {
                self.cursor.pending_wrap = true;
            }
        } else {
            self.cursor.col = new_col;
        }
    }

    /// Blank out halves of wide characters that a write at (row, col..col+w)
    /// would corrupt.
    fn clean_wide(&mut self, row: u16, col: u16, w: u8) {
        let cols = self.cols();
        if self
            .scr()
            .cell(row, col)
            .map(|c| c.width == 0)
            .unwrap_or(false)
            && col > 0
        {
            let style = self.blank_style();
            *self.scr_mut().cell_mut(row, col - 1) = Cell::blank(style);
        }
        for i in 0..u16::from(w) {
            let c = col + i;
            if c >= cols {
                break;
            }
            if self
                .scr()
                .cell(row, c)
                .map(|cell| cell.width == 2)
                .unwrap_or(false)
                && c + 1 < cols
            {
                let style = self.blank_style();
                *self.scr_mut().cell_mut(row, c + 1) = Cell::blank(style);
            }
        }
    }

    /// Attach a zero-width (combining) char to the previously printed cell.
    fn combine(&mut self, c: char) {
        let row = self.cursor.row;
        let mut col = if self.cursor.pending_wrap {
            self.cursor.col
        } else if self.cursor.col > 0 {
            self.cursor.col - 1
        } else {
            return;
        };
        if self
            .scr()
            .cell(row, col)
            .map(|cell| cell.width == 0)
            .unwrap_or(false)
            && col > 0
        {
            col -= 1; // skip a wide spacer back to its head
        }
        self.scr_mut().cell_mut(row, col).extra.push(c);
        self.touch();
    }

    fn map_charset(&self, c: char) -> char {
        let cs = if self.cursor.shift == 1 {
            self.cursor.g1
        } else {
            self.cursor.g0
        };
        match cs {
            Charset::Ascii => c,
            Charset::Uk => {
                if c == '#' {
                    '£'
                } else {
                    c
                }
            }
            Charset::DecSpecial => dec_special(c),
        }
    }

    // --- C0/C1 controls ------------------------------------------------------

    fn execute(&mut self, b: u8) {
        match b {
            0x07 => {
                self.bell_count += 1;
                self.touch();
            }
            0x08 => {
                if self.cursor.col > 0 {
                    self.cursor.col -= 1;
                }
                self.cursor.pending_wrap = false;
                self.touch();
            }
            0x09 => self.horizontal_tab(),
            0x0A..=0x0C => self.linefeed(),
            0x0D => {
                self.cursor.col = 0;
                self.cursor.pending_wrap = false;
                self.touch();
            }
            0x0E => self.cursor.shift = 1,
            0x0F => self.cursor.shift = 0,
            0x84 => self.index(),
            0x85 => {
                self.cursor.col = 0;
                self.index();
            }
            0x88 => {
                let col = self.cursor.col as usize;
                if col < self.tabs.len() {
                    self.tabs[col] = true;
                }
            }
            0x8D => self.reverse_index(),
            _ => {}
        }
    }

    pub(crate) fn linefeed(&mut self) {
        self.index();
        if self.modes.lnm {
            self.cursor.col = 0;
        }
    }

    /// IND: move down one line, scrolling at the bottom margin.
    pub(crate) fn index(&mut self) {
        self.cursor.pending_wrap = false;
        let (_, bot) = self.region();
        if self.cursor.row == bot {
            self.scroll_up_n(1);
        } else if self.cursor.row + 1 < self.rows() {
            self.cursor.row += 1;
        }
        self.touch();
    }

    /// RI: move up one line, scrolling at the top margin.
    pub(crate) fn reverse_index(&mut self) {
        self.cursor.pending_wrap = false;
        let (top, _) = self.region();
        if self.cursor.row == top {
            self.scroll_down_n(1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
        self.touch();
    }

    pub(crate) fn scroll_up_n(&mut self, n: u16) {
        let (top, bot) = self.region();
        let save = !self.alt_active && top == 0 && bot == self.rows() - 1;
        let style = self.blank_style();
        self.scr_mut().scroll_up(top, bot, n, save, style);
        self.touch();
    }

    pub(crate) fn scroll_down_n(&mut self, n: u16) {
        let (top, bot) = self.region();
        let style = self.blank_style();
        self.scr_mut().scroll_down(top, bot, n, style);
        self.touch();
    }

    pub(crate) fn horizontal_tab(&mut self) {
        let cols = self.cols();
        let mut col = self.cursor.col;
        while col + 1 < cols {
            col += 1;
            if self.tabs.get(col as usize).copied().unwrap_or(false) {
                break;
            }
        }
        self.cursor.col = col;
        self.cursor.pending_wrap = false;
        self.touch();
    }

    pub(crate) fn back_tab(&mut self) {
        let mut col = self.cursor.col;
        while col > 0 {
            col -= 1;
            if self.tabs.get(col as usize).copied().unwrap_or(false) {
                break;
            }
        }
        self.cursor.col = col;
        self.cursor.pending_wrap = false;
        self.touch();
    }

    // --- ESC dispatch ---------------------------------------------------------

    fn esc_dispatch(&mut self, intermediates: &[u8], final_byte: u8) {
        match (intermediates.first().copied(), final_byte) {
            (None, b'7') => self.save_cursor(),
            (None, b'8') => self.restore_cursor(),
            (None, b'D') => self.index(),
            (None, b'E') => {
                self.cursor.col = 0;
                self.index();
            }
            (None, b'H') => {
                let col = self.cursor.col as usize;
                if col < self.tabs.len() {
                    self.tabs[col] = true;
                }
            }
            (None, b'M') => self.reverse_index(),
            (None, b'c') => self.full_reset(),
            (None, b'=') => self.modes.keypad_app = true,
            (None, b'>') => self.modes.keypad_app = false,
            (None, b'Z') => self.respond("\x1b[?62;22c"),
            (Some(b'#'), b'8') => self.decaln(),
            (Some(b'('), f) => self.cursor.g0 = charset_for(f),
            (Some(b')'), f) => self.cursor.g1 = charset_for(f),
            _ => {}
        }
    }

    pub(crate) fn save_cursor(&mut self) {
        let saved = SavedCursor {
            cursor: self.cursor,
            origin: self.modes.origin,
        };
        if self.alt_active {
            self.saved_alt = saved;
        } else {
            self.saved_primary = saved;
        }
    }

    pub(crate) fn restore_cursor(&mut self) {
        let saved = if self.alt_active {
            self.saved_alt
        } else {
            self.saved_primary
        };
        self.cursor = saved.cursor;
        self.modes.origin = saved.origin;
        self.cursor.row = self.cursor.row.min(self.rows() - 1);
        self.cursor.col = self.cursor.col.min(self.cols() - 1);
        self.touch();
    }

    fn decaln(&mut self) {
        self.scroll_top = 0;
        self.scroll_bot = self.rows() - 1;
        self.modes.origin = false;
        self.cursor.row = 0;
        self.cursor.col = 0;
        self.cursor.pending_wrap = false;
        let (rows, cols) = (self.rows(), self.cols());
        for r in 0..rows {
            for c in 0..cols {
                *self.scr_mut().cell_mut(r, c) = Cell {
                    ch: 'E',
                    style: Style::default(),
                    width: 1,
                    extra: Vec::new(),
                    hyperlink: 0,
                };
            }
        }
        self.touch();
    }

    pub(crate) fn full_reset(&mut self) {
        let (rows, cols) = (self.rows(), self.cols());
        self.alt_active = false;
        self.primary.clear_grid(Style::default());
        self.alt.clear_grid(Style::default());
        self.cursor = CursorState::default();
        self.saved_primary = SavedCursor::default();
        self.saved_alt = SavedCursor::default();
        self.modes = Modes::default();
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        self.tabs = default_tabs(cols);
        self.palette = default_palette();
        self.fg_color = None;
        self.bg_color = None;
        self.cursor_color = None;
        self.cursor_style_raw = 0;
        self.kitty_primary.reset();
        self.kitty_alt.reset();
        self.graphics.reset();
        self.last_printed = None;
        self.color_stack.clear();
        self.last_text_size = None;
        self.touch();
    }

    /// DECSTR (CSI ! p) per xterm ctlseqs: shows the cursor (DECTCEM),
    /// replace mode (IRM), absolute origin (DECOM), no autowrap (DECAWM),
    /// normal cursor keys (DECCKM), numeric keypad (DECNKM), full-screen
    /// margins (DECSTBM), normal SGR, unprotected (DECSCA), default
    /// charsets, and clears the DECSC saved-cursor state. Unlike RIS it
    /// does not move the cursor, clear the screen, or touch other modes.
    pub(crate) fn soft_reset(&mut self) {
        self.modes.cursor_visible = true;
        self.modes.origin = false;
        self.modes.insert = false;
        self.modes.autowrap = false;
        self.modes.cursor_keys = false;
        self.modes.keypad_app = false;
        self.scroll_top = 0;
        self.scroll_bot = self.rows() - 1;
        // Style::default() also drops DECSCA protection from the pen.
        self.cursor.style = Style::default();
        self.cursor.hyperlink = 0;
        self.cursor.pending_wrap = false;
        self.cursor.g0 = Charset::Ascii;
        self.cursor.g1 = Charset::Ascii;
        self.cursor.shift = 0;
        self.cursor_style_raw = 0;
        if self.alt_active {
            self.saved_alt = SavedCursor::default();
        } else {
            self.saved_primary = SavedCursor::default();
        }
        // The kitty keyboard spec resets flags and stack on DECSTR too.
        self.kitty_stack_mut().reset();
        self.touch();
    }

    /// DECCOLM (DECSET/DECRST 3): switch between 132 and 80 columns. Only
    /// honored after DECSET 40 (allow DECCOLM), matching xterm. Resets the
    /// margins, homes the cursor, and clears the screen unless DECNCSM
    /// (mode 95) is set.
    pub(crate) fn set_deccolm(&mut self, set: bool) {
        if !self.modes.allow_deccolm {
            return;
        }
        self.modes.deccolm = set;
        let cols = if set { 132 } else { 80 };
        self.resize(self.rows(), cols);
        self.scroll_top = 0;
        self.scroll_bot = self.rows() - 1;
        if !self.modes.no_clear_on_deccolm {
            let style = self.blank_style();
            self.scr_mut().clear_grid(style);
        }
        self.cursor.row = 0;
        self.cursor.col = 0;
        self.cursor.pending_wrap = false;
        self.touch();
    }

    /// Alt-screen switching for modes 47 / 1047 / 1049.
    pub(crate) fn set_alt_screen(&mut self, mode: u16, on: bool) {
        if on && !self.alt_active {
            if mode == 1049 {
                self.save_cursor();
            }
            self.alt_active = true;
            if mode == 1049 {
                let style = self.blank_style();
                self.alt.clear_grid(style);
            }
        } else if !on && self.alt_active {
            if mode == 1047 {
                let style = self.blank_style();
                self.alt.clear_grid(style);
            }
            self.alt_active = false;
            if mode == 1049 {
                self.restore_cursor();
            }
        }
        self.cursor.pending_wrap = false;
        self.cursor.row = self.cursor.row.min(self.rows() - 1);
        self.touch();
    }

    pub(crate) fn kitty_stack_mut(&mut self) -> &mut KittyKeyStack {
        if self.alt_active {
            &mut self.kitty_alt
        } else {
            &mut self.kitty_primary
        }
    }

    fn apc_dispatch(&mut self, data: &[u8]) {
        // Kitty graphics: APC payload starting with 'G'.
        let Some((&b'G', rest)) = data.split_first() else {
            return;
        };
        let cursor = (self.cursor.row, self.cursor.col);
        let (resp, advance) = self.graphics.dispatch(rest, cursor);
        if let Some(resp) = resp {
            self.respond(&resp);
        }
        if let Some((cols, rows)) = advance {
            // Kitty moves the cursor to the cell after the image's
            // bottom-right corner (suppressed by C=1); clamped to the grid.
            let down = rows.saturating_sub(1).min(u32::from(u16::MAX)) as u16;
            let right = cols.min(u32::from(u16::MAX)) as u16;
            self.cursor.row = self.cursor.row.saturating_add(down).min(self.rows() - 1);
            self.cursor.col = self.cursor.col.saturating_add(right).min(self.cols() - 1);
            self.cursor.pending_wrap = false;
        }
        self.touch();
    }

    // --- frozen public API ----------------------------------------------------

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows() && cols == self.cols() {
            return;
        }
        let here = (self.cursor.row, self.cursor.col);
        let mut primary_cur = if self.alt_active { (0, 0) } else { here };
        let mut alt_cur = if self.alt_active { here } else { (0, 0) };
        // Primary reflows on width changes; the alt screen truncates/pads
        // (kitty behavior).
        self.primary.resize(rows, cols, &mut primary_cur, true);
        self.alt.resize(rows, cols, &mut alt_cur, false);
        let cur = if self.alt_active {
            alt_cur
        } else {
            primary_cur
        };
        self.cursor.row = cur.0.min(rows - 1);
        self.cursor.col = cur.1.min(cols - 1);
        self.cursor.pending_wrap = false;
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        let old = std::mem::take(&mut self.tabs);
        let mut tabs = default_tabs(cols);
        for (i, &t) in old.iter().enumerate().take(tabs.len()) {
            // Preserve custom stops within the previous width.
            tabs[i] = t;
        }
        self.tabs = tabs;
        self.touch();
    }

    pub fn rows(&self) -> u16 {
        self.primary.rows()
    }

    pub fn cols(&self) -> u16 {
        self.primary.cols()
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn cursor(&self) -> Cursor {
        Cursor {
            row: self.cursor.row,
            col: self.cursor.col,
            visible: self.modes.cursor_visible,
            shape: match self.cursor_style_raw {
                3 | 4 => CursorShape::Underline,
                5 | 6 => CursorShape::Bar,
                _ => CursorShape::Block,
            },
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    pub fn screen(&self) -> &Screen {
        self.scr()
    }

    // --- extra getters ----------------------------------------------------------

    pub fn bell_count(&self) -> u64 {
        self.bell_count
    }

    /// Working directory reported via OSC 7.
    pub fn pwd(&self) -> &str {
        &self.pwd
    }

    pub fn is_alt_screen(&self) -> bool {
        self.alt_active
    }

    pub fn mouse_mode(&self) -> MouseMode {
        self.modes.mouse_mode
    }

    pub fn mouse_protocol(&self) -> MouseProtocol {
        self.modes.mouse_protocol
    }

    pub fn bracketed_paste(&self) -> bool {
        self.modes.bracketed_paste
    }

    pub fn app_cursor_keys(&self) -> bool {
        self.modes.cursor_keys
    }

    pub fn app_keypad(&self) -> bool {
        self.modes.keypad_app
    }

    pub fn focus_reporting(&self) -> bool {
        self.modes.focus_reporting
    }

    pub fn reverse_video(&self) -> bool {
        self.modes.reverse_video
    }

    /// Kitty keyboard stack for the active screen.
    pub(crate) fn kitty_stack(&self) -> &KittyKeyStack {
        if self.alt_active {
            &self.kitty_alt
        } else {
            &self.kitty_primary
        }
    }

    /// Current kitty keyboard flags for the active screen.
    pub fn kitty_flags(&self) -> KittyFlags {
        self.kitty_stack().flags()
    }

    pub fn synchronized_output(&self) -> bool {
        self.modes.synchronized
    }

    pub fn cursor_color(&self) -> Option<(u8, u8, u8)> {
        self.cursor_color
    }

    pub fn fg_color(&self) -> Option<(u8, u8, u8)> {
        self.fg_color
    }

    pub fn bg_color(&self) -> Option<(u8, u8, u8)> {
        self.bg_color
    }

    pub fn palette(&self) -> &[(u8, u8, u8); 256] {
        &self.palette
    }

    /// Resolve a cell's hyperlink id to its URI.
    pub fn hyperlink(&self, id: u32) -> Option<&str> {
        self.hyperlinks.get(&id).map(|h| h.uri.as_str())
    }

    /// OSC 133 semantic mark on a visible row of the active screen.
    pub fn row_mark(&self, row: u16) -> Option<SemanticMark> {
        self.scr().row(row).and_then(|r| r.mark())
    }

    /// Last OSC 9 / OSC 99 desktop notification body.
    pub fn last_notification(&self) -> Option<&str> {
        self.last_notification.as_deref()
    }

    /// Last OSC 22 pointer shape.
    pub fn pointer_shape(&self) -> &str {
        &self.pointer_shape
    }

    /// Last OSC 52 clipboard payload (decoded).
    pub fn clipboard(&self) -> &[u8] {
        &self.clipboard
    }

    /// OSC 52 selection slot: `'c'` clipboard, `'p'` primary, `'s'` select.
    pub fn selection(&self, kind: char) -> &[u8] {
        match kind {
            'p' => &self.primary_selection,
            's' => &self.select_selection,
            _ => &self.clipboard,
        }
    }

    pub(crate) fn selection_slot_mut(&mut self, kind: char) -> &mut Vec<u8> {
        match kind {
            'p' => &mut self.primary_selection,
            's' => &mut self.select_selection,
            _ => &mut self.clipboard,
        }
    }

    /// Raw `metadata;text` payload of the last OSC 66 (kitty text-sizing
    /// protocol). Only the `w:` width key affects layout; other keys are
    /// preserved here for callers.
    pub fn last_text_size(&self) -> Option<&str> {
        self.last_text_size.as_deref()
    }

    /// DECSTBM scroll region, 0-based inclusive.
    pub fn scroll_region(&self) -> (u16, u16) {
        self.region()
    }

    /// Whether the cursor blinks (DECSCUSR odd styles, or DECSET 12).
    pub fn cursor_blink(&self) -> bool {
        matches!(self.cursor_style_raw, 0 | 1 | 3 | 5) || self.modes.cursor_blink
    }

    /// Stored kitty graphics images, keyed by image id.
    pub fn images(&self) -> &HashMap<u32, Image> {
        self.graphics.images()
    }

    /// Active kitty graphics placements.
    pub fn placements(&self) -> &[Placement] {
        self.graphics.placements()
    }

    /// Animation frames stored for a kitty graphics image (a=f).
    pub fn image_frames(&self, image_id: u32) -> &[Frame] {
        self.graphics.frames(image_id)
    }

    /// Animation play state set with kitty graphics a=a.
    pub fn animation_state(&self, image_id: u32) -> Option<AnimationState> {
        self.graphics.animation(image_id)
    }

    /// Full RGBA canvas of a kitty graphics animation frame, composed onto
    /// its base chain (`c=` frames, alpha blended or replaced per `X=1`).
    /// Frame 0 is the root image itself.
    pub fn composed_frame(&self, image_id: u32, frame_no: u32) -> Option<Vec<u8>> {
        self.graphics.composed_frame(image_id, frame_no)
    }
}

pub(crate) fn default_tabs(cols: u16) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0).collect()
}

fn charset_for(f: u8) -> Charset {
    match f {
        b'0' => Charset::DecSpecial,
        b'A' => Charset::Uk,
        _ => Charset::Ascii,
    }
}

/// DEC Special Graphics (line drawing) mapping for `ESC ( 0`.
fn dec_special(c: char) -> char {
    match c {
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        '_' => ' ',
        _ => c,
    }
}
