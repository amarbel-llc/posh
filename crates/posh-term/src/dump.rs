//! Serialization: plain-text dump and VT escape-stream dump used for
//! session attach/replay and remote state sync.

use std::fmt::Write;

use crate::cell::{Color, Style, UnderlineStyle};
use crate::graphics::ImageFormat;
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
    // 256-color and truecolor use the semicolon-delimited form (`38;5;n`,
    // `38;2;r;g;b`) — what mosh emits and what every color terminal accepts.
    // The colon subparameter form (`38:5:n`, `38:2:r:g:b`) is ITU-T T.416 and
    // accepted by far fewer terminals; some also read the first colon arg after
    // `2` as a colorspace id, mangling truecolor. Underline *style* below stays
    // colon (`4:2`) because it has no semicolon equivalent.
    match style.fg {
        Color::Default => {}
        Color::Indexed(i) if i < 8 => {
            let _ = write!(s, ";{}", 30 + u16::from(i));
        }
        Color::Indexed(i) if i < 16 => {
            let _ = write!(s, ";{}", 90 + u16::from(i) - 8);
        }
        Color::Indexed(i) => {
            let _ = write!(s, ";38;5;{i}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(s, ";38;2;{r};{g};{b}");
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
            let _ = write!(s, ";48;5;{i}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(s, ";48;2;{r};{g};{b}");
        }
    }
    // Underline color (SGR 58) is a non-universal extension, so it keeps the
    // colon form: a terminal that doesn't grok it skips `58:…` as one opaque
    // parameter, whereas `58;…` would be misread as separate SGR attributes.
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
        self.dump_vt_impl(false)
    }

    /// Like [`Terminal::dump_vt`] but single-screen: draws only the active
    /// grid and never switches the target's screen buffers (no scrollback
    /// replay, no 1049 enter/exit, no inactive-screen kitty seeding). For
    /// attach clients that pin the outer terminal to its own alternate
    /// screen, where the target's primary buffer belongs to the user's
    /// shell. Assumes a fresh, cleared target screen like `dump_vt`.
    pub fn dump_vt_flat(&self) -> Vec<u8> {
        self.dump_vt_impl(true)
    }

    fn dump_vt_impl(&self, flat: bool) -> Vec<u8> {
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
        if !flat && !self.alt_active && !self.kitty_alt.entries().is_empty() {
            out.push_str("\x1b[?1049h");
            for &f in self.kitty_alt.entries() {
                let _ = write!(out, "\x1b[>{f}u");
            }
            out.push_str("\x1b[?1049l");
        }

        if flat {
            // Single-screen target: just the active grid from home.
            out.push_str("\x1b[H");
            self.draw_grid(&mut out, self.scr(), &mut st);
            self.dump_graphics(&mut out);
            self.dump_modes(&mut out);
            self.dump_cursor(&mut out, &mut st);
            return out.into_bytes();
        }

        // Primary screen. With scrollback, replay it and the visible grid
        // as ONE continuous flow: every soft-wrapped row — including the
        // last scrollback row, whose seam continues into grid row 0 —
        // regenerates its wrap flag by actually autowrapping on the
        // target, and the grid rows printed at the bottom push each
        // scrollback line up into the target's ring (no padding or homing
        // needed). The pen drops to default before each scroll-opening
        // newline because scrolled-in blank lines inherit the pen's
        // background (BCE). github #22.
        let sb_len = self.primary.scrollback_len();
        if sb_len > 0 {
            for i in 0..sb_len {
                let row = self.primary.scrollback_row(i).unwrap();
                self.emit_row(&mut out, row, &mut st);
                if !row.wrapped() {
                    self.reset_pen(&mut out, &mut st);
                    out.push_str("\r\n");
                }
            }
            for r in 0..self.primary.rows() {
                let row = self.primary.row(r).unwrap();
                self.emit_row(&mut out, row, &mut st);
                if !row.wrapped() && r + 1 < self.primary.rows() {
                    self.reset_pen(&mut out, &mut st);
                    out.push_str("\r\n");
                }
            }
        } else {
            out.push_str("\x1b[H");
            self.draw_grid(&mut out, &self.primary, &mut st);
        }

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

        self.dump_graphics(&mut out);
        self.dump_modes(&mut out);
        self.dump_cursor(&mut out, &mut st);
        out.into_bytes()
    }

    /// Escape stream that morphs a mode-synced terminal showing the
    /// previously active screen into one showing the newly active screen,
    /// without ever switching the target's own buffers.
    ///
    /// The session daemon substitutes this for the application's own
    /// alt-screen switches (DECSET/DECRST 47/1047/1049, RIS) in the raw
    /// attach broadcast: clients pin the outer terminal to its alternate
    /// screen, so the inner switch has to repaint in place. Everything the
    /// raw passthrough keeps in sync (shared modes, colors, title) is left
    /// alone; only screen content, the cursor, and drawing-relevant modes
    /// that the repaint itself must normalize (region, origin, charsets,
    /// insert, autowrap) are emitted.
    pub fn dump_screen_switch(&self) -> Vec<u8> {
        let mut out = String::new();
        let mut st = EmitState {
            style: Style::default(),
            hyperlink: 0,
        };
        // Hide the cursor for the repaint, and normalize the target to a
        // drawable state: full-screen region, absolute addressing, ASCII
        // G0 shifted in, replace mode, autowrap on (draw_grid regenerates
        // soft-wrap flags by autowrapping), default pen, no open link.
        out.push_str("\x1b[?25l\x1b[r\x1b[?6l\x1b(B\x0f\x1b[4l\x1b[?7h\x1b[0m\x1b[0\"q\x1b]8;;\x1b\\");
        out.push_str("\x1b[2J\x1b[H");
        self.draw_grid(&mut out, self.scr(), &mut st);
        // Re-assert what normalization may have pushed away from the model.
        let (top, bot) = self.region();
        if top != 0 || bot != self.rows() - 1 {
            let _ = write!(out, "\x1b[{};{}r", top + 1, bot + 1);
        }
        if self.modes.origin {
            out.push_str("\x1b[?6h");
        }
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
        if self.modes.insert {
            out.push_str("\x1b[4h");
        }
        if !self.modes.autowrap {
            out.push_str("\x1b[?7l");
        }
        // The clear deleted any visible kitty placements on the target;
        // re-place from the model (image data already lives in the target
        // from the original raw transmission).
        self.dump_placements(&mut out);
        self.dump_cursor(&mut out, &mut st);
        if self.modes.cursor_visible {
            out.push_str("\x1b[?25h");
        }
        out.into_bytes()
    }

    /// Replays kitty graphics: stored images as (chunked) APC
    /// transmissions, animation frames and play state, then placements.
    /// Absolute placements are anchored by cursor positioning; relative
    /// ones re-emit their `P=`/`Q=`/`H=`/`V=` linkage — the parent was
    /// created (and so replays) first, re-resolving to the same cells
    /// while keeping the parent fields in the replayed model. `q=2` keeps
    /// the replay response-quiet. PNG transmissions are stored decoded, so
    /// they replay as raw RGBA.
    fn dump_graphics(&self, out: &mut String) {
        let fmt_key = |f: ImageFormat| if f == ImageFormat::Rgb { 24 } else { 32 };
        let mut ids: Vec<u32> = self.graphics.images().keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let img = &self.graphics.images()[&id];
            let mut keys = format!(
                "a=t,q=2,f={},s={},v={},i={id}",
                fmt_key(img.format),
                img.width,
                img.height
            );
            if img.number != 0 {
                let _ = write!(keys, ",I={}", img.number);
            }
            emit_apc_chunks(out, &keys, &img.data);
            for fr in self.graphics.frames(id) {
                let mut keys = format!(
                    "a=f,q=2,i={id},r={},f={},s={},v={},x={},y={},z={},c={}",
                    fr.number,
                    fmt_key(fr.format),
                    fr.width,
                    fr.height,
                    fr.x,
                    fr.y,
                    fr.gap_ms,
                    fr.base_frame
                );
                if fr.replace {
                    keys.push_str(",X=1");
                }
                emit_apc_chunks(out, &keys, &fr.data);
            }
            if let Some(anim) = self.graphics.animation(id) {
                let _ = write!(
                    out,
                    "\x1b_Ga=a,q=2,i={id},s={},v={},c={}\x1b\\",
                    anim.state, anim.loops, anim.current_frame
                );
            }
        }
        self.dump_placements(out);
    }

    /// Replays kitty graphics placements only (cheap `a=p` references to
    /// image ids the target already stores), without re-transmitting data.
    fn dump_placements(&self, out: &mut String) {
        for p in self.graphics.placements() {
            // A relative placement whose parent has since vanished (quota
            // eviction) would be rejected on replay; anchor it absolutely
            // at its resolved cell instead.
            let parent_alive = p.parent_image != 0
                && self.graphics.placements().iter().any(|q| {
                    q.image_id == p.parent_image
                        && (p.parent_placement == 0 || q.placement_id == p.parent_placement)
                });
            if !parent_alive && !p.unicode {
                let _ = write!(out, "\x1b[{};{}H", p.row + 1, p.col + 1);
            }
            let _ = write!(
                out,
                "\x1b_Ga=p,q=2,i={},p={},x={},y={},w={},h={},c={},r={},z={},X={},Y={}",
                p.image_id,
                p.placement_id,
                p.src_x,
                p.src_y,
                p.src_w,
                p.src_h,
                p.cols,
                p.rows,
                p.z,
                p.cell_x,
                p.cell_y
            );
            if parent_alive {
                let _ = write!(
                    out,
                    ",P={},Q={},H={},V={}",
                    p.parent_image, p.parent_placement, p.h_off, p.v_off
                );
            }
            if p.unicode {
                out.push_str(",U=1");
            }
            out.push_str("\x1b\\");
        }
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
        if self.modes.alternate_scroll {
            out.push_str("\x1b[?1007h");
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

/// Emits one data-bearing kitty APC command, splitting the base64 payload
/// into ≤4096-byte chunks (`m=1` continuations) per the kitty spec so the
/// stream is valid for a real terminal, not just our own parser.
fn emit_apc_chunks(out: &mut String, keys: &str, data: &[u8]) {
    const CHUNK: usize = 4096;
    let payload = crate::base64::encode(data);
    if payload.len() <= CHUNK {
        let _ = write!(out, "\x1b_G{keys};{payload}\x1b\\");
        return;
    }
    let mut chunks = payload.as_bytes().chunks(CHUNK).peekable();
    // base64 output is ASCII, so chunk boundaries stay valid UTF-8.
    let first = std::str::from_utf8(chunks.next().unwrap()).unwrap();
    let _ = write!(out, "\x1b_G{keys},m=1;{first}\x1b\\");
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        let chunk = std::str::from_utf8(chunk).unwrap();
        let _ = write!(out, "\x1b_Gm={more};{chunk}\x1b\\");
    }
}
