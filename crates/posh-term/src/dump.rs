//! Serialization: plain-text dump and VT escape-stream dump used for
//! session attach/replay and remote state sync.

use std::fmt::Write;

use crate::cell::{Color, Style, UnderlineStyle};
use crate::screen::{Row, Screen, SemanticMark};
use crate::terminal::{Charset, Terminal};

/// SGR parameter string (without the `m`) that reproduces `style` from a
/// reset state. Always begins with `0`.
pub fn sgr_params(style: &Style) -> String {
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
/// `style.protected` mirrors the target's DECSCA state, which is toggled
/// with `CSI " q` (SGR emissions never change it).
struct EmitState {
    style: Style,
    hyperlink: u32,
}

/// Emits the DECSCA toggle reaching `protected`.
fn emit_protect(out: &mut String, protected: bool) {
    out.push_str(if protected { "\x1b[1\"q" } else { "\x1b[0\"q" });
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
        if !self.title.is_empty() && self.title == self.icon_title {
            // OSC 0 sets window and icon title together.
            let _ = write!(out, "\x1b]0;{}\x07", self.title);
        } else {
            if !self.title.is_empty() {
                let _ = write!(out, "\x1b]2;{}\x07", self.title);
            }
            if !self.icon_title.is_empty() {
                let _ = write!(out, "\x1b]1;{}\x07", self.icon_title);
            }
        }
        if !self.pwd.is_empty() {
            let _ = write!(out, "\x1b]7;file://{}\x1b\\", self.pwd);
        }

        // Seed the inactive alternate screen's kitty keyboard stack (the
        // active screen's stack is replayed by dump_modes, and the alt stack
        // when alt is active is replayed in the alt block below). When the
        // primary is active, briefly enter the alt screen — which does not
        // reset the kitty stack — to replay its pushes, then return so the
        // app finds the right flags after a later `?1049h`.
        if !self.alt_active && !self.kitty_alt.entries().is_empty() {
            out.push_str("\x1b[?1049h");
            for &f in self.kitty_alt.entries() {
                let _ = write!(out, "\x1b[>{f}u");
            }
            out.push_str("\x1b[?1049l");
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
            // Replay the primary screen's kitty keyboard stack before
            // switching (the alt screen's own stack is emitted with modes).
            for &f in self.kitty_primary.entries() {
                let _ = write!(out, "\x1b[>{f}u");
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
            // Sync DECSCA first so the SGR comparison below sees styles
            // that differ only in real SGR attributes.
            if cell.style.protected != st.style.protected {
                emit_protect(out, cell.style.protected);
                st.style.protected = cell.style.protected;
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
        if st.style.protected {
            emit_protect(out, false);
            st.style.protected = false;
        }
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
        // DECCOLM family first: replaying DECSET 3 homes the cursor and
        // resets the margins, so it must precede the region and cursor
        // dumps. DECNCSM is forced on around it to keep the drawn grid.
        if self.modes.allow_deccolm || self.modes.deccolm {
            out.push_str("\x1b[?40h");
        }
        if self.modes.deccolm && self.cols() == 132 {
            out.push_str("\x1b[?95h\x1b[?3h");
            if !self.modes.no_clear_on_deccolm {
                out.push_str("\x1b[?95l");
            }
        } else if self.modes.no_clear_on_deccolm {
            out.push_str("\x1b[?95h");
        }
        if self.modes.deccolm && !self.modes.allow_deccolm {
            out.push_str("\x1b[?40l");
        }
        let (top, bot) = self.region();
        if top != 0 || bot != self.rows() - 1 {
            let _ = write!(out, "\x1b[{};{}r", top + 1, bot + 1);
        }
        if self.modes.origin {
            out.push_str("\x1b[?6h");
        }
        self.dump_tabs(out);
        match self.cursor.g0 {
            Charset::DecSpecial => out.push_str("\x1b(0"),
            Charset::Uk => out.push_str("\x1b(A"),
            Charset::Ascii => {}
        }
        match self.cursor.g1 {
            Charset::DecSpecial => out.push_str("\x1b)0"),
            Charset::Uk => out.push_str("\x1b)A"),
            Charset::Ascii => {}
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
        if self.modes.synchronized {
            out.push_str("\x1b[?2026h");
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
        if let Some(m) = self.modes.mouse_mode.decset() {
            let _ = write!(out, "\x1b[?{m}h");
        }
        if let Some(p) = self.modes.mouse_protocol.decset() {
            let _ = write!(out, "\x1b[?{p}h");
        }
        // Replay the active screen's kitty keyboard stack push by push so
        // later pops on the target find the same entries.
        for &f in self.kitty_stack().entries() {
            let _ = write!(out, "\x1b[>{f}u");
        }
    }

    /// Re-creates non-default tab stops: clear all, then HTS at each.
    fn dump_tabs(&self, out: &mut String) {
        if self.tabs.iter().enumerate().all(|(i, &t)| t == (i % 8 == 0)) {
            return;
        }
        out.push_str("\x1b[3g");
        for (i, &t) in self.tabs.iter().enumerate() {
            if t {
                let _ = write!(out, "\x1b[1;{}H\x1bH", i + 1);
            }
        }
    }

    fn dump_cursor(&self, out: &mut String, st: &mut EmitState) {
        if self.cursor_style_raw != 0 {
            let _ = write!(out, "\x1b[{} q", self.cursor_style_raw);
        }
        // Restore the application's pen and hyperlink state.
        if self.cursor.style.protected != st.style.protected {
            emit_protect(out, self.cursor.style.protected);
            st.style.protected = self.cursor.style.protected;
        }
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
        // When a wide glyph armed pending-wrap, the cursor sits on the
        // width-0 spacer in the last column; the glyph that re-arms the wrap
        // is the wide head one column to the left, so aim the CUP there.
        let wide_pending = self.cursor.pending_wrap
            && self.cursor.col > 0
            && self
                .scr()
                .cell(self.cursor.row, self.cursor.col)
                .is_some_and(|cell| cell.width == 0);
        let print_col = if wide_pending {
            self.cursor.col - 1
        } else {
            self.cursor.col
        };
        let _ = write!(
            out,
            "\x1b[{};{}H",
            self.cursor.row.saturating_sub(top) + 1,
            print_col + 1
        );
        if self.cursor.pending_wrap {
            // Re-print the final cell (a width-1 cell, or a width-2 head whose
            // spacer regenerates) to regenerate pending-wrap, restoring the
            // pen (SGR and DECSCA independently) around it.
            if let Some(cell) = self.scr().cell(self.cursor.row, print_col) {
                if cell.width == 1 || cell.width == 2 {
                    let pen = st.style;
                    let toggle = cell.style.protected != pen.protected;
                    let mut sgr_want = cell.style;
                    sgr_want.protected = pen.protected;
                    if toggle {
                        emit_protect(out, cell.style.protected);
                    }
                    if sgr_want != pen {
                        let _ = write!(out, "\x1b[{}m", sgr_params(&cell.style));
                    }
                    out.push(if cell.ch == '\0' { ' ' } else { cell.ch });
                    out.extend(cell.extra.iter());
                    if sgr_want != pen {
                        let _ = write!(out, "\x1b[{}m", sgr_params(&pen));
                    }
                    if toggle {
                        emit_protect(out, pen.protected);
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
