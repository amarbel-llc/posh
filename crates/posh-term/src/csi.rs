//! CSI sequence dispatch.

use crate::cell::{Cell, Color, Style, UnderlineStyle};
use crate::modes::{MouseMode, MouseProtocol};
use crate::parser::{param, param_or};
use crate::terminal::{ColorStackEntry, Terminal};

impl Terminal {
    pub(crate) fn csi_dispatch(
        &mut self,
        params: &[Vec<u16>],
        intermediates: &[u8],
        private: u8,
        final_byte: u8,
    ) {
        let im = intermediates.first().copied();
        match (private, im, final_byte) {
            (0, None, b'A') => self.cursor_up(param_or(params, 0, 1)),
            (0, None, b'B') => self.cursor_down(param_or(params, 0, 1)),
            (0, None, b'C') => self.cursor_right(param_or(params, 0, 1)),
            (0, None, b'D') => self.cursor_left(param_or(params, 0, 1)),
            (0, None, b'E') => {
                self.cursor_down(param_or(params, 0, 1));
                self.cursor.col = 0;
            }
            (0, None, b'F') => {
                self.cursor_up(param_or(params, 0, 1));
                self.cursor.col = 0;
            }
            (0, None, b'G') | (0, None, b'`') => {
                // CHA / HPA
                self.cursor.col = (param_or(params, 0, 1) - 1).min(self.cols() - 1);
                self.cursor.pending_wrap = false;
                self.touch();
            }
            (0, None, b'H') | (0, None, b'f') => {
                self.move_to(param_or(params, 0, 1) - 1, param_or(params, 1, 1) - 1)
            }
            (0, None, b'I') => {
                for _ in 0..param_or(params, 0, 1) {
                    self.horizontal_tab();
                }
            }
            (0, None, b'J') => self.erase_display(param(params, 0, 0), false),
            (0, None, b'K') => self.erase_line(param(params, 0, 0), false),
            // DECSED / DECSEL: selective erase honors DECSCA protection.
            (b'?', None, b'J') => self.erase_display(param(params, 0, 0), true),
            (b'?', None, b'K') => self.erase_line(param(params, 0, 0), true),
            (0, None, b'L') => self.insert_lines(param_or(params, 0, 1)),
            (0, None, b'M') => self.delete_lines(param_or(params, 0, 1)),
            (0, None, b'P') => self.delete_chars(param_or(params, 0, 1)),
            (0, None, b'S') => self.scroll_up_n(param_or(params, 0, 1)),
            (0, None, b'T') => self.scroll_down_n(param_or(params, 0, 1)),
            (0, None, b'X') => self.erase_chars(param_or(params, 0, 1)),
            (0, None, b'Z') => {
                for _ in 0..param_or(params, 0, 1) {
                    self.back_tab();
                }
            }
            (0, None, b'@') => self.insert_chars(param_or(params, 0, 1)),
            (0, None, b'a') => {
                // HPR
                let n = param_or(params, 0, 1);
                self.cursor.col = self.cursor.col.saturating_add(n).min(self.cols() - 1);
                self.cursor.pending_wrap = false;
                self.touch();
            }
            (0, None, b'b') => self.repeat(param_or(params, 0, 1)),
            (0, None, b'c') => {
                if param(params, 0, 0) == 0 {
                    // DA1: VT220-class with ANSI color (like a modern xterm).
                    self.respond("\x1b[?62;22c");
                }
            }
            (b'>', None, b'c') => self.respond("\x1b[>1;10;0c"),
            (0, None, b'd') => {
                // VPA
                let row = param_or(params, 0, 1) - 1;
                let row = if self.modes.origin {
                    self.region().0.saturating_add(row)
                } else {
                    row
                };
                self.cursor.row = row.min(self.rows() - 1);
                self.clamp_to_region_if_origin();
                self.cursor.pending_wrap = false;
                self.touch();
            }
            (0, None, b'e') => {
                // VPR
                let n = param_or(params, 0, 1);
                self.cursor.row = self.cursor.row.saturating_add(n).min(self.rows() - 1);
                self.cursor.pending_wrap = false;
                self.touch();
            }
            (0, None, b'g') => {
                // TBC
                match param(params, 0, 0) {
                    0 => {
                        let col = self.cursor.col as usize;
                        if col < self.tabs.len() {
                            self.tabs[col] = false;
                        }
                    }
                    3 => self.tabs.iter_mut().for_each(|t| *t = false),
                    _ => {}
                }
            }
            (0, None, b'h') => self.set_ansi_mode(params, true),
            (0, None, b'l') => self.set_ansi_mode(params, false),
            (b'?', None, b'h') => self.set_dec_modes(params, true),
            (b'?', None, b'l') => self.set_dec_modes(params, false),
            (0, None, b'm') => self.sgr(params),
            (b'>', None, b'm') => {} // XTMODKEYS: ignored
            (0, None, b'n') => match param(params, 0, 0) {
                5 => self.respond("\x1b[0n"),
                6 => {
                    let top = if self.modes.origin {
                        self.region().0
                    } else {
                        0
                    };
                    let resp = format!(
                        "\x1b[{};{}R",
                        self.cursor.row.saturating_sub(top) + 1,
                        self.cursor.col + 1
                    );
                    self.respond(&resp);
                }
                _ => {}
            },
            (b'?', None, b'n') => {
                if param(params, 0, 0) == 6 {
                    // DECXCPR
                    let resp = format!("\x1b[?{};{}R", self.cursor.row + 1, self.cursor.col + 1);
                    self.respond(&resp);
                }
            }
            (b'?', Some(b'$'), b'p') => self.decrqm_dec(param(params, 0, 0)),
            (0, Some(b'$'), b'p') => self.decrqm_ansi(param(params, 0, 0)),
            (0, Some(b'!'), b'p') => self.soft_reset(),
            (0, Some(b'"'), b'q') => {
                // DECSCA: 1 = protected, 0/2 = unprotected.
                self.cursor.style.protected = param(params, 0, 0) == 1;
            }
            (0, Some(b'#'), b'P') => self.push_colors(),
            (0, Some(b'#'), b'Q') => self.pop_colors(),
            (0, Some(b'#'), b'R') => self.report_colors(),
            (0, Some(b' '), b'q') => {
                // DECSCUSR
                let n = param(params, 0, 0);
                if n <= 6 {
                    self.cursor_style_raw = n;
                    self.touch();
                }
            }
            (b'>', None, b'q') => {
                // XTVERSION
                self.respond("\x1bP>|posh-term 0.1.0\x1b\\");
            }
            (0, None, b'r') => {
                // DECSTBM. An oversized bottom (a common "to the end" idiom,
                // e.g. `CSI 5;999r`) clamps to the last row rather than
                // voiding the whole region, matching xterm.
                let rows = self.rows();
                let top = param_or(params, 0, 1) - 1;
                let bot = (param_or(params, 1, rows) - 1).min(rows - 1);
                if top < bot {
                    self.scroll_top = top;
                    self.scroll_bot = bot;
                    self.move_to(0, 0);
                }
            }
            (0, None, b's') => self.save_cursor(),
            (0, None, b'u') => self.restore_cursor(),
            (b'?', None, b'u') => {
                let flags = self.kitty_flags().0;
                let resp = format!("\x1b[?{flags}u");
                self.respond(&resp);
            }
            (b'>', None, b'u') => {
                let flags = param(params, 0, 0) as u8;
                self.kitty_stack_mut().push(flags);
            }
            (b'<', None, b'u') => {
                let n = param_or(params, 0, 1);
                self.kitty_stack_mut().pop(n);
            }
            (b'=', None, b'u') => {
                let flags = param(params, 0, 0) as u8;
                let mode = param_or(params, 1, 1);
                self.kitty_stack_mut().set(flags, mode);
            }
            (0, None, b't') => self.window_ops(params),
            _ => {}
        }
    }

    // --- cursor movement (margin-aware) -------------------------------------

    fn cursor_up(&mut self, n: u16) {
        let (top, _) = self.region();
        let limit = if self.cursor.row >= top { top } else { 0 };
        self.cursor.row = self.cursor.row.saturating_sub(n).max(limit);
        self.cursor.pending_wrap = false;
        self.touch();
    }

    fn cursor_down(&mut self, n: u16) {
        let (_, bot) = self.region();
        let limit = if self.cursor.row <= bot {
            bot
        } else {
            self.rows() - 1
        };
        self.cursor.row = self.cursor.row.saturating_add(n).min(limit);
        self.cursor.pending_wrap = false;
        self.touch();
    }

    fn cursor_right(&mut self, n: u16) {
        self.cursor.col = self.cursor.col.saturating_add(n).min(self.cols() - 1);
        self.cursor.pending_wrap = false;
        self.touch();
    }

    fn cursor_left(&mut self, n: u16) {
        self.cursor.col = self.cursor.col.saturating_sub(n);
        self.cursor.pending_wrap = false;
        self.touch();
    }

    /// CUP/HVP: origin-mode-aware absolute positioning (0-based args).
    pub(crate) fn move_to(&mut self, row: u16, col: u16) {
        let (top, bot) = self.region();
        let row = if self.modes.origin {
            top.saturating_add(row).min(bot)
        } else {
            row.min(self.rows() - 1)
        };
        self.cursor.row = row;
        self.cursor.col = col.min(self.cols() - 1);
        self.cursor.pending_wrap = false;
        self.touch();
    }

    pub(crate) fn clamp_to_region_if_origin(&mut self) {
        if self.modes.origin {
            let (top, bot) = self.region();
            self.cursor.row = self.cursor.row.clamp(top, bot);
        }
    }

    // --- erase / edit -----------------------------------------------------------

    /// One cell of ED/EL; the selective forms (DECSED/DECSEL) skip cells
    /// protected by DECSCA.
    fn erase_cell(&mut self, row: u16, col: u16, style: Style, selective: bool) {
        let cell = self.scr_mut().cell_mut(row, col);
        if selective && cell.style.protected {
            return;
        }
        *cell = Cell::blank(style);
    }

    fn erase_row(&mut self, r: u16, style: Style, selective: bool) {
        if selective {
            for c in 0..self.cols() {
                self.erase_cell(r, c, style, true);
            }
            self.scr_mut().row_mut(r).wrapped = false;
        } else {
            let cols = self.cols() as usize;
            *self.scr_mut().row_mut(r) = crate::screen::Row::blank(cols, style);
        }
    }

    fn erase_display(&mut self, mode: u16, selective: bool) {
        let style = self.blank_style();
        let (rows, cols) = (self.rows(), self.cols());
        let (row, col) = (self.cursor.row, self.cursor.col);
        self.cursor.pending_wrap = false;
        match mode {
            0 => {
                for c in col..cols {
                    self.erase_cell(row, c, style, selective);
                }
                self.scr_mut().row_mut(row).wrapped = false;
                for r in row + 1..rows {
                    self.erase_row(r, style, selective);
                }
            }
            1 => {
                for r in 0..row {
                    self.erase_row(r, style, selective);
                }
                for c in 0..=col {
                    self.erase_cell(row, c, style, selective);
                }
            }
            2 => {
                for r in 0..rows {
                    self.erase_row(r, style, selective);
                }
            }
            3 => self.scr_mut().clear_scrollback(),
            _ => {}
        }
        self.touch();
    }

    fn erase_line(&mut self, mode: u16, selective: bool) {
        let style = self.blank_style();
        let cols = self.cols();
        let (row, col) = (self.cursor.row, self.cursor.col);
        self.cursor.pending_wrap = false;
        let (from, to) = match mode {
            0 => (col, cols - 1),
            1 => (0, col),
            2 => (0, cols - 1),
            _ => return,
        };
        for c in from..=to {
            self.erase_cell(row, c, style, selective);
        }
        if mode == 0 || mode == 2 {
            self.scr_mut().row_mut(row).wrapped = false;
        }
        self.touch();
    }

    fn insert_lines(&mut self, n: u16) {
        let (top, bot) = self.region();
        let row = self.cursor.row;
        if row < top || row > bot {
            return;
        }
        let style = self.blank_style();
        self.scr_mut().scroll_down(row, bot, n, style);
        self.cursor.col = 0;
        self.cursor.pending_wrap = false;
        self.touch();
    }

    fn delete_lines(&mut self, n: u16) {
        let (top, bot) = self.region();
        let row = self.cursor.row;
        if row < top || row > bot {
            return;
        }
        let style = self.blank_style();
        self.scr_mut().scroll_up(row, bot, n, false, style);
        self.cursor.col = 0;
        self.cursor.pending_wrap = false;
        self.touch();
    }

    fn insert_chars(&mut self, n: u16) {
        let style = self.blank_style();
        let (row, col) = (self.cursor.row, self.cursor.col);
        let r = self.scr_mut().row_mut(row);
        for _ in 0..n.min(r.cells.len() as u16) {
            r.cells.pop();
            r.cells.insert(col as usize, Cell::blank(style));
        }
        self.repair_wide_halves(row);
        self.cursor.pending_wrap = false;
        self.touch();
    }

    fn delete_chars(&mut self, n: u16) {
        let style = self.blank_style();
        let (row, col) = (self.cursor.row, self.cursor.col);
        let r = self.scr_mut().row_mut(row);
        for _ in 0..n.min(r.cells.len() as u16) {
            if (col as usize) < r.cells.len() {
                r.cells.remove(col as usize);
                r.cells.push(Cell::blank(style));
            }
        }
        self.repair_wide_halves(row);
        self.cursor.pending_wrap = false;
        self.touch();
    }

    fn erase_chars(&mut self, n: u16) {
        let style = self.blank_style();
        let cols = self.cols();
        let (row, col) = (self.cursor.row, self.cursor.col);
        for c in col..col.saturating_add(n).min(cols) {
            *self.scr_mut().cell_mut(row, c) = Cell::blank(style);
        }
        self.repair_wide_halves(row);
        self.cursor.pending_wrap = false;
        self.touch();
    }

    /// REP: repeat the preceding graphic character.
    fn repeat(&mut self, n: u16) {
        if let Some(c) = self.last_printed {
            let cap = u32::from(self.cols()) * u32::from(self.rows());
            for _ in 0..u32::from(n).min(cap) {
                self.print(c);
            }
        }
    }

    // --- modes ---------------------------------------------------------------

    fn set_ansi_mode(&mut self, params: &[Vec<u16>], set: bool) {
        for p in params {
            match p.first().copied().unwrap_or(0) {
                4 => self.modes.insert = set,
                20 => self.modes.lnm = set,
                _ => {}
            }
        }
        self.touch();
    }

    fn set_dec_modes(&mut self, params: &[Vec<u16>], set: bool) {
        for p in params {
            let n = p.first().copied().unwrap_or(0);
            self.set_dec_mode(n, set);
        }
    }

    pub(crate) fn set_dec_mode(&mut self, n: u16, set: bool) {
        match n {
            1 => self.modes.cursor_keys = set,
            3 => self.set_deccolm(set),
            5 => self.modes.reverse_video = set,
            6 => {
                self.modes.origin = set;
                self.move_to(0, 0);
            }
            7 => {
                self.modes.autowrap = set;
                if !set {
                    self.cursor.pending_wrap = false;
                }
            }
            8 => self.modes.autorepeat = set,
            9 => self.modes.mouse_mode = if set { MouseMode::X10 } else { MouseMode::None },
            12 => self.modes.cursor_blink = set,
            25 => self.modes.cursor_visible = set,
            40 => self.modes.allow_deccolm = set,
            95 => self.modes.no_clear_on_deccolm = set,
            47 | 1047 | 1049 => self.set_alt_screen(n, set),
            66 => self.modes.keypad_app = set,
            1000 => {
                self.modes.mouse_mode = if set {
                    MouseMode::Normal
                } else {
                    MouseMode::None
                }
            }
            1002 => {
                self.modes.mouse_mode = if set {
                    MouseMode::ButtonEvent
                } else {
                    MouseMode::None
                }
            }
            1003 => {
                self.modes.mouse_mode = if set {
                    MouseMode::AnyEvent
                } else {
                    MouseMode::None
                }
            }
            1004 => self.modes.focus_reporting = set,
            1007 => self.modes.alternate_scroll = set,
            1005 => {
                self.modes.mouse_protocol = if set {
                    MouseProtocol::Utf8
                } else {
                    MouseProtocol::Normal
                }
            }
            1006 => {
                self.modes.mouse_protocol = if set {
                    MouseProtocol::Sgr
                } else {
                    MouseProtocol::Normal
                }
            }
            1016 => {
                self.modes.mouse_protocol = if set {
                    MouseProtocol::SgrPixel
                } else {
                    MouseProtocol::Normal
                }
            }
            1048 => {
                if set {
                    self.save_cursor()
                } else {
                    self.restore_cursor()
                }
            }
            2004 => self.modes.bracketed_paste = set,
            2026 => self.modes.synchronized = set,
            _ => {}
        }
        self.touch();
    }

    /// DECRQM reply value: 1 = set, 2 = reset, 0 = not recognized.
    fn dec_mode_status(&self, n: u16) -> u8 {
        let b = |v: bool| if v { 1 } else { 2 };
        match n {
            1 => b(self.modes.cursor_keys),
            3 => b(self.modes.deccolm),
            5 => b(self.modes.reverse_video),
            6 => b(self.modes.origin),
            7 => b(self.modes.autowrap),
            8 => b(self.modes.autorepeat),
            9 => b(self.modes.mouse_mode == MouseMode::X10),
            12 => b(self.modes.cursor_blink),
            25 => b(self.modes.cursor_visible),
            40 => b(self.modes.allow_deccolm),
            95 => b(self.modes.no_clear_on_deccolm),
            47 | 1047 | 1049 => b(self.alt_active),
            66 => b(self.modes.keypad_app),
            1000 => b(self.modes.mouse_mode == MouseMode::Normal),
            1002 => b(self.modes.mouse_mode == MouseMode::ButtonEvent),
            1003 => b(self.modes.mouse_mode == MouseMode::AnyEvent),
            1004 => b(self.modes.focus_reporting),
            1007 => b(self.modes.alternate_scroll),
            1005 => b(self.modes.mouse_protocol == MouseProtocol::Utf8),
            1006 => b(self.modes.mouse_protocol == MouseProtocol::Sgr),
            1016 => b(self.modes.mouse_protocol == MouseProtocol::SgrPixel),
            // DECSC save/restore (1048) carries no queryable state: it is
            // reported as reset, matching a terminal that never blocks it.
            1048 => 2,
            2004 => b(self.modes.bracketed_paste),
            2026 => b(self.modes.synchronized),
            _ => 0,
        }
    }

    fn decrqm_dec(&mut self, n: u16) {
        let status = self.dec_mode_status(n);
        let resp = format!("\x1b[?{n};{status}$y");
        self.respond(&resp);
    }

    fn decrqm_ansi(&mut self, n: u16) {
        let status = match n {
            4 => {
                if self.modes.insert {
                    1
                } else {
                    2
                }
            }
            20 => {
                if self.modes.lnm {
                    1
                } else {
                    2
                }
            }
            _ => 0,
        };
        let resp = format!("\x1b[{n};{status}$y");
        self.respond(&resp);
    }

    /// XTWINOPS reports. Pixel sizes use the crate's placeholder cell size.
    fn window_ops(&mut self, params: &[Vec<u16>]) {
        match param(params, 0, 0) {
            14 => {
                let resp = format!(
                    "\x1b[4;{};{}t",
                    u32::from(self.rows()) * crate::CELL_H,
                    u32::from(self.cols()) * crate::CELL_W
                );
                self.respond(&resp);
            }
            16 => self.respond(&format!("\x1b[6;{};{}t", crate::CELL_H, crate::CELL_W)),
            18 => {
                let resp = format!("\x1b[8;{};{}t", self.rows(), self.cols());
                self.respond(&resp);
            }
            _ => {}
        }
    }

    // --- color stack -----------------------------------------------------------

    /// XTPUSHCOLORS (CSI # P): saves the palette and dynamic colors.
    /// xterm keeps up to 10 entries; pushing past that drops the oldest.
    fn push_colors(&mut self) {
        if self.color_stack.len() >= 10 {
            self.color_stack.remove(0);
        }
        self.color_stack.push(ColorStackEntry {
            palette: self.palette,
            fg: self.fg_color,
            bg: self.bg_color,
            cursor: self.cursor_color,
        });
    }

    /// XTPOPCOLORS (CSI # Q).
    fn pop_colors(&mut self) {
        if let Some(e) = self.color_stack.pop() {
            self.palette = e.palette;
            self.fg_color = e.fg;
            self.bg_color = e.bg;
            self.cursor_color = e.cursor;
            self.touch();
        }
    }

    /// XTREPORTCOLORS (CSI # R): replies `CSI ? Pi ; Ps # Q` with the
    /// current stack entry and the number of entries stored.
    fn report_colors(&mut self) {
        let n = self.color_stack.len();
        let resp = format!("\x1b[?{n};{n}#Q");
        self.respond(&resp);
    }

    // --- SGR -----------------------------------------------------------------

    pub(crate) fn sgr(&mut self, params: &[Vec<u16>]) {
        // DECSCA protection rides in Style but is not an SGR attribute:
        // SGR 0 must not clear it.
        let protected = self.cursor.style.protected;
        let style = &mut self.cursor.style;
        if params.is_empty() {
            *style = Style::default();
            style.protected = protected;
            return;
        }
        let mut i = 0;
        while i < params.len() {
            let p = &params[i];
            match p.first().copied().unwrap_or(0) {
                0 => *style = Style::default(),
                1 => style.bold = true,
                2 => style.dim = true,
                3 => style.italic = true,
                4 => {
                    // SGR 4 with colon subparameter selects underline style.
                    style.underline = match p.get(1) {
                        None => UnderlineStyle::Single,
                        Some(0) => UnderlineStyle::None,
                        Some(1) => UnderlineStyle::Single,
                        Some(2) => UnderlineStyle::Double,
                        Some(3) => UnderlineStyle::Curly,
                        Some(4) => UnderlineStyle::Dotted,
                        Some(5) => UnderlineStyle::Dashed,
                        Some(_) => UnderlineStyle::Single,
                    };
                }
                5 | 6 => style.blink = true,
                7 => style.inverse = true,
                8 => style.invisible = true,
                9 => style.strikethrough = true,
                21 => style.underline = UnderlineStyle::Double,
                22 => {
                    style.bold = false;
                    style.dim = false;
                }
                23 => style.italic = false,
                24 => style.underline = UnderlineStyle::None,
                25 => style.blink = false,
                27 => style.inverse = false,
                28 => style.invisible = false,
                29 => style.strikethrough = false,
                30..=37 => style.fg = Color::Indexed((p[0] - 30) as u8),
                38 => {
                    let (color, consumed) = extended_color(params, i);
                    if let Some(c) = color {
                        style.fg = c;
                    }
                    i += consumed;
                }
                39 => style.fg = Color::Default,
                40..=47 => style.bg = Color::Indexed((p[0] - 40) as u8),
                48 => {
                    let (color, consumed) = extended_color(params, i);
                    if let Some(c) = color {
                        style.bg = c;
                    }
                    i += consumed;
                }
                49 => style.bg = Color::Default,
                58 => {
                    let (color, consumed) = extended_color(params, i);
                    if let Some(c) = color {
                        style.underline_color = c;
                    }
                    i += consumed;
                }
                59 => style.underline_color = Color::Default,
                90..=97 => style.fg = Color::Indexed((p[0] - 90 + 8) as u8),
                100..=107 => style.bg = Color::Indexed((p[0] - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
        self.cursor.style.protected = protected;
        self.touch();
    }
}

/// Parses 38/48/58 extended colors in both colon (one param with
/// subparameters, optionally including a colorspace id) and semicolon
/// (consuming following params) forms. Returns the color and how many extra
/// params were consumed.
fn extended_color(params: &[Vec<u16>], i: usize) -> (Option<Color>, usize) {
    let p = &params[i];
    let clamp = |v: u16| v.min(255) as u8;
    if p.len() > 1 {
        match p[1] {
            5 if p.len() >= 3 => (Some(Color::Indexed(clamp(p[2]))), 0),
            2 if p.len() >= 6 => {
                // 38:2:<colorspace>:r:g:b
                (Some(Color::Rgb(clamp(p[3]), clamp(p[4]), clamp(p[5]))), 0)
            }
            2 if p.len() == 5 => (Some(Color::Rgb(clamp(p[2]), clamp(p[3]), clamp(p[4]))), 0),
            _ => (None, 0),
        }
    } else {
        match params.get(i + 1).and_then(|p| p.first()).copied() {
            Some(5) => {
                let idx = params
                    .get(i + 2)
                    .and_then(|p| p.first())
                    .copied()
                    .unwrap_or(0);
                (Some(Color::Indexed(clamp(idx))), 2)
            }
            Some(2) => {
                let get = |k: usize| {
                    params
                        .get(i + k)
                        .and_then(|p| p.first())
                        .copied()
                        .unwrap_or(0)
                };
                (
                    Some(Color::Rgb(clamp(get(2)), clamp(get(3)), clamp(get(4)))),
                    4,
                )
            }
            _ => (None, 0),
        }
    }
}
