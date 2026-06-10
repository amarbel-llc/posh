//! Serialization: plain-text dump and VT escape-stream dump used for
//! session attach/replay and remote state sync.

use std::fmt::Write;

use crate::cell::{Color, Style, UnderlineStyle};
use crate::screen::{Row, Screen, SemanticMark};
use crate::terminal::{Charset, Terminal};

/// SGR parameter string (without the `m`) that reproduces `style` from a
/// reset state. Always begins with `0`.
pub(crate) fn sgr_params(style: &Style) -> String {
    let mut s = String::from("0");
    if style.bold {
        s.push_str(";1");
    }
    if style.dim {
        s.push_str(";2");
    }
    if style.italic {
        s.push_str(";3");
    }
    match style.underline {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => s.push_str(";4"),
        UnderlineStyle::Double => s.push_str(";4:2"),
        UnderlineStyle::Curly => s.push_str(";4:3"),
        UnderlineStyle::Dotted => s.push_str(";4:4"),
        UnderlineStyle::Dashed => s.push_str(";4:5"),
    }
    if style.blink {
        s.push_str(";5");
    }
    if style.inverse {
        s.push_str(";7");
    }
    if style.invisible {
        s.push_str(";8");
    }
    if style.strikethrough {
        s.push_str(";9");
    }
    match style.fg {
        Color::Default => {}
        Color::Indexed(i) if i < 8 => {
            let _ = write!(s, ";{}", 30 + u16::from(i));
        }
        Color::Indexed(i) if i < 16 => {
            let _ = write!(s, ";{}", 90 + u16::from(i) - 8);
        }
        Color::Indexed(i) => {
            let _ = write!(s, ";38:5:{i}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(s, ";38:2:{r}:{g}:{b}");
        }
    }
    match style.bg {
        Color::Default => {}
        Color::Indexed(i) if i < 8 => {
            let _ = write!(s, ";{}", 40 + u16::from(i));
        }
        Color::Indexed(i) if i < 16 => {
            let _ = write!(s, ";{}", 100 + u16::from(i) - 8);
        }
        Color::Indexed(i) => {
            let _ = write!(s, ";48:5:{i}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(s, ";48:2:{r}:{g}:{b}");
        }
    }
    match style.underline_color {
        Color::Default => {}
        Color::Indexed(i) => {
            let _ = write!(s, ";58:5:{i}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(s, ";58:2:{r}:{g}:{b}");
        }
    }
    s
}

/// Pen state tracked while emitting cells, for minimal transitions.
struct EmitState {
    style: Style,
    hyperlink: u32,
}

impl Terminal {
    /// Plain text of the scrollback plus the visible screen, with
    /// trailing whitespace trimmed; soft-wrapped rows are joined.
    pub fn dump_text(&self) -> String {
        let mut out = String::new();
        let mut line = String::new();
        let flush = |line: &mut String, out: &mut String| {
            line.truncate(line.trim_end().len());
            out.push_str(line);
            out.push('\n');
            line.clear();
        };
        for i in 0..self.primary.scrollback_len() {
            let row = self.primary.scrollback_row(i).unwrap();
            line.push_str(&row.text(false));
            if !row.wrapped() {
                flush(&mut line, &mut out);
            }
        }
        let grid = self.scr();
        for r in 0..grid.rows() {
            let row = grid.row(r).unwrap();
            line.push_str(&row.text(false));
            if !row.wrapped() {
                flush(&mut line, &mut out);
            }
        }
        if !line.is_empty() {
            flush(&mut line, &mut out);
        }
        out
    }

    /// VT escape stream that reproduces the terminal state (contents,
    /// attributes, cursor, modes, title, scroll region) on a fresh
    /// terminal of the same size.
    pub fn dump_vt(&self) -> Vec<u8> {
        let mut out = String::new();
        let mut st = EmitState {
            style: Style::default(),
            hyperlink: 0,
        };

        self.dump_colors(&mut out);
        if !self.title.is_empty() {
            let _ = write!(out, "\x1b]2;{}\x07", self.title);
        }
        if !self.pwd.is_empty() {
            let _ = write!(out, "\x1b]7;file://{}\x1b\\", self.pwd);
        }

        // Primary screen: replay scrollback by printing and scrolling it
        // off, then draw the visible grid in flow order (preserving soft
        // wrap flags).
        let sb_len = self.primary.scrollback_len();
        let mut last_wrapped = false;
        for i in 0..sb_len {
            let row = self.primary.scrollback_row(i).unwrap();
            self.emit_row(&mut out, row, &mut st);
            last_wrapped = row.wrapped();
            if !last_wrapped {
                out.push_str("\r\n");
            }
        }
        if sb_len > 0 {
            self.reset_pen(&mut out, &mut st);
            // Pad with newlines until every replayed line has scrolled
            // into the target's scrollback.
            let pad = self.rows() - if last_wrapped { 0 } else { 1 };
            for _ in 0..pad {
                out.push('\n');
            }
        }
        out.push_str("\x1b[H");
        self.draw_grid(&mut out, &self.primary, &mut st);

        if self.alt_active {
            // Park the primary cursor where 1049's save expects it, then
            // switch and draw the alternate screen.
            self.reset_pen(&mut out, &mut st);
            let saved = self.saved_primary;
            let _ = write!(
                out,
                "\x1b[{};{}H",
                saved.cursor.row + 1,
                saved.cursor.col + 1
            );
            let pf = self.kitty_primary.flags().0;
            if pf != 0 {
                let _ = write!(out, "\x1b[={pf};1u");
            }
            out.push_str("\x1b[?1049h");
            self.draw_grid(&mut out, &self.alt, &mut st);
        }

        self.dump_modes(&mut out);
        self.dump_cursor(&mut out, &mut st);
        out.into_bytes()
    }

    fn dump_colors(&self, out: &mut String) {
        for (i, &(r, g, b)) in self.palette.iter().enumerate() {
            if (r, g, b) != crate::cell::default_palette_entry(i as u8) {
                let _ = write!(out, "\x1b]4;{i};{}\x1b\\", spec(r, g, b));
            }
        }
        if let Some((r, g, b)) = self.fg_color {
            let _ = write!(out, "\x1b]10;{}\x1b\\", spec(r, g, b));
        }
        if let Some((r, g, b)) = self.bg_color {
            let _ = write!(out, "\x1b]11;{}\x1b\\", spec(r, g, b));
        }
        if let Some((r, g, b)) = self.cursor_color {
            let _ = write!(out, "\x1b]12;{}\x1b\\", spec(r, g, b));
        }
    }

    /// Draws a grid from the home position in flow order so that soft
    /// wrap flags regenerate naturally.
    fn draw_grid(&self, out: &mut String, grid: &Screen, st: &mut EmitState) {
        for r in 0..grid.rows() {
            let row = grid.row(r).unwrap();
            self.emit_row(out, row, st);
            if !row.wrapped() && r + 1 < grid.rows() {
                out.push_str("\r\n");
            }
        }
    }

    /// Emits one row's cells with minimal SGR/hyperlink transitions.
    /// Wrapped rows print every cell (they are full width by
    /// construction); otherwise trailing skippable cells are dropped.
    fn emit_row(&self, out: &mut String, row: &Row, st: &mut EmitState) {
        if let Some(mark) = row.mark() {
            let m = match mark {
                SemanticMark::PromptStart => "A",
                SemanticMark::InputStart => "B",
                SemanticMark::OutputStart => "C",
                SemanticMark::CommandEnd => "D",
            };
            let _ = write!(out, "\x1b]133;{m}\x1b\\");
        }
        let cells = row.cells();
        let end = if row.wrapped() {
            cells.len()
        } else {
            cells
                .iter()
                .rposition(|c| !c.is_dump_skippable())
                .map(|i| i + 1)
                .unwrap_or(0)
        };
        for cell in &cells[..end] {
            if cell.width == 0 {
                continue; // wide spacer
            }
            if cell.style != st.style {
                let _ = write!(out, "\x1b[{}m", sgr_params(&cell.style));
                st.style = cell.style;
            }
            if cell.hyperlink != st.hyperlink {
                self.emit_hyperlink(out, cell.hyperlink);
                st.hyperlink = cell.hyperlink;
            }
            out.push(if cell.ch == '\0' { ' ' } else { cell.ch });
            out.extend(cell.extra.iter());
        }
    }

    fn emit_hyperlink(&self, out: &mut String, id: u32) {
        match self.hyperlinks.get(&id) {
            Some(h) if id != 0 => {
                let params = if h.id.is_empty() {
                    String::new()
                } else {
                    format!("id={}", h.id)
                };
                let _ = write!(out, "\x1b]8;{params};{}\x1b\\", h.uri);
            }
            _ => out.push_str("\x1b]8;;\x1b\\"),
        }
    }

    fn reset_pen(&self, out: &mut String, st: &mut EmitState) {
        if st.style != Style::default() {
            out.push_str("\x1b[0m");
            st.style = Style::default();
        }
        if st.hyperlink != 0 {
            out.push_str("\x1b]8;;\x1b\\");
            st.hyperlink = 0;
        }
    }

    fn dump_modes(&self, out: &mut String) {
        let (top, bot) = self.region();
        if top != 0 || bot != self.rows() - 1 {
            let _ = write!(out, "\x1b[{};{}r", top + 1, bot + 1);
        }
        if self.modes.origin {
            out.push_str("\x1b[?6h");
        }
        if self.cursor.g0 == Charset::DecSpecial {
            out.push_str("\x1b(0");
        }
        if self.cursor.g1 == Charset::DecSpecial {
            out.push_str("\x1b)0");
        }
        if self.cursor.shift == 1 {
            out.push('\x0e');
        }
        if self.modes.cursor_keys {
            out.push_str("\x1b[?1h");
        }
        if !self.modes.autowrap {
            out.push_str("\x1b[?7l");
        }
        if self.modes.reverse_video {
            out.push_str("\x1b[?5h");
        }
        if !self.modes.autorepeat {
            out.push_str("\x1b[?8l");
        }
        if self.modes.cursor_blink {
            out.push_str("\x1b[?12h");
        }
        if self.modes.bracketed_paste {
            out.push_str("\x1b[?2004h");
        }
        if self.modes.focus_reporting {
            out.push_str("\x1b[?1004h");
        }
        if self.modes.insert {
            out.push_str("\x1b[4h");
        }
        if self.modes.lnm {
            out.push_str("\x1b[20h");
        }
        if self.modes.keypad_app {
            out.push_str("\x1b=");
        }
        let mouse = match self.modes.mouse_mode {
            crate::modes::MouseMode::None => None,
            crate::modes::MouseMode::X10 => Some(9),
            crate::modes::MouseMode::Normal => Some(1000),
            crate::modes::MouseMode::ButtonEvent => Some(1002),
            crate::modes::MouseMode::AnyEvent => Some(1003),
        };
        if let Some(m) = mouse {
            let _ = write!(out, "\x1b[?{m}h");
        }
        let proto = match self.modes.mouse_protocol {
            crate::modes::MouseProtocol::Normal => None,
            crate::modes::MouseProtocol::Utf8 => Some(1005),
            crate::modes::MouseProtocol::Sgr => Some(1006),
            crate::modes::MouseProtocol::SgrPixel => Some(1016),
        };
        if let Some(p) = proto {
            let _ = write!(out, "\x1b[?{p}h");
        }
        let kf = self.kitty_flags().0;
        if kf != 0 {
            let _ = write!(out, "\x1b[={kf};1u");
        }
    }

    fn dump_cursor(&self, out: &mut String, st: &mut EmitState) {
        if self.cursor_style_raw != 0 {
            let _ = write!(out, "\x1b[{} q", self.cursor_style_raw);
        }
        // Restore the application's pen and hyperlink state.
        if self.cursor.style != st.style {
            let _ = write!(out, "\x1b[{}m", sgr_params(&self.cursor.style));
            st.style = self.cursor.style;
        }
        if self.cursor.hyperlink != st.hyperlink {
            self.emit_hyperlink(out, self.cursor.hyperlink);
            st.hyperlink = self.cursor.hyperlink;
        }
        // Position (origin-relative when DECOM is active, since the CUP we
        // emit is itself interpreted in origin mode).
        let top = if self.modes.origin {
            self.region().0
        } else {
            0
        };
        let _ = write!(
            out,
            "\x1b[{};{}H",
            self.cursor.row - top + 1,
            self.cursor.col + 1
        );
        if self.cursor.pending_wrap {
            // Re-print the final cell to regenerate the pending-wrap state.
            if let Some(cell) = self.scr().cell(self.cursor.row, self.cursor.col) {
                if cell.width == 1 {
                    if cell.style != st.style {
                        let _ = write!(out, "\x1b[{}m", sgr_params(&cell.style));
                    }
                    out.push(if cell.ch == '\0' { ' ' } else { cell.ch });
                    out.extend(cell.extra.iter());
                    if cell.style != st.style {
                        let _ = write!(out, "\x1b[{}m", sgr_params(&st.style));
                    }
                }
            }
        }
        if !self.modes.cursor_visible {
            out.push_str("\x1b[?25l");
        }
    }
}

fn spec(r: u8, g: u8, b: u8) -> String {
    format!("rgb:{r:02x}/{g:02x}/{b:02x}")
}
