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

/// Resets the target's **rendering** state to defaults, so a replay onto a
/// terminal carrying leftovers from whatever ran before it lands correctly.
///
/// Needed because a dump re-emits only the modes that are NON-DEFAULT IN THE
/// SOURCE ([`Terminal::dump_modes`]): a non-default mode stranded on the TARGET
/// that the source holds at its default would otherwise never be corrected, and
/// erasing the screen clears CELLS, not MODES. Margins and origin mode come
/// first, since every later position depends on how `\x1b[H` is interpreted.
///
/// Scoped deliberately to state that affects how the replay is DRAWN — margins,
/// origin, reverse video, autowrap, cursor blink/style, synchronized output,
/// LNM, insert mode, both charsets, the shift state, the pen, DECSCA, and an
/// open hyperlink. Excluded:
///
/// * **Input encoding** (cursor-keys, keypad, mouse, bracketed paste, focus,
///   the kitty stack) — it cannot corrupt a repaint, and the session client
///   already resets it on detach.
/// * **The DECCOLM family** (`?3`/`?40`/`?95`) — resetting it would resize the
///   user's terminal, which posh does not own.
/// * **Tab stops** — there is no portable "restore the default every-8 stops"
///   sequence; `\x1b[3g` alone clears them all, which is worse than inheriting.
pub const DRAWABLE_STATE_RESET: &str = "\x1b[r\x1b[?6l\x1b[?5l\x1b[?7h\x1b[?12l\x1b[?2026l\x1b[20l\x1b[4l\x1b(B\x1b)B\x0f\x1b[0m\x1b[0\"q\x1b]8;;\x1b\\";

/// How [`Terminal::dump_cursor`] positions the cursor: `Absolute` (a CUP from
/// home, for paths that redraw the grid from `\x1b[H`) or `Relative` (anchored to
/// the bottom of a continuous content flow, for the scrollback-replay path — so a
/// replay into a taller target lands the cursor consistently, not offset by the
/// height difference).
#[derive(Clone, Copy, PartialEq, Eq)]
enum CursorAnchor {
    Absolute,
    Relative,
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

    /// Serializes one primary-screen scrollback row (by ring index) as the
    /// same per-row escape stream [`Terminal::dump_vt`] emits for a row: the
    /// row's style/hyperlink runs from a fresh default pen, self-contained
    /// from the start of a clean line. A hard-terminated (non-soft-wrapped)
    /// row ends in a pen reset and `\r\n`; a soft-wrapped row omits the
    /// trailing newline, so a consumer that concatenates rows and replays
    /// them onto an autowrapping terminal of the same width regenerates the
    /// wrap seam itself. This is the row unit the scrollback-sync protocol
    /// ships in a `BODY_SCROLLBACK` body (RFC 0002 §2). Returns `None` when
    /// `i` is past the end of the ring.
    pub fn dump_scrollback_row(&self, i: usize) -> Option<Vec<u8>> {
        let row = self.primary.scrollback_row(i)?;
        let mut out = String::new();
        let mut st = EmitState {
            style: Style::default(),
            hyperlink: 0,
        };
        self.emit_terminated_row(&mut out, row, &mut st);
        Some(out.into_bytes())
    }

    /// Emits one row as a self-contained per-row escape stream: the row's
    /// style/hyperlink runs, then — for a hard-terminated (non-soft-wrapped)
    /// row — a pen reset and `\r\n`. A soft-wrapped row omits the newline so a
    /// consumer regenerates the wrap seam by autowrapping the same-width
    /// target. Shared by `dump_scrollback_row` and the primary-screen
    /// scrollback replay in `dump_vt_impl`.
    fn emit_terminated_row(&self, out: &mut String, row: &Row, st: &mut EmitState) {
        self.emit_row(out, row, st);
        if !row.wrapped() {
            self.reset_pen(out, st);
            out.push_str("\r\n");
        }
    }

    /// Serializes the active grid's visible rows as self-contained per-row
    /// escape streams, one `Vec<u8>` per row, in the same format as
    /// [`Terminal::dump_scrollback_row`] (each row starts from a default pen and,
    /// when hard-terminated, ends in a pen reset and `\r\n`). The scrollback
    /// scroll-view (FDR 0005) concatenates the accumulated ring rows with these
    /// to render a window of the session's logical history through `posh_term`'s
    /// own autowrap.
    pub fn dump_visible_rows(&self) -> Vec<Vec<u8>> {
        let grid = self.scr();
        (0..grid.rows())
            .filter_map(|r| grid.row(r))
            .map(|row| {
                let mut out = String::new();
                let mut st = EmitState {
                    style: Style::default(),
                    hyperlink: 0,
                };
                self.emit_terminated_row(&mut out, row, &mut st);
                out.into_bytes()
            })
            .collect()
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
    /// shell.
    ///
    /// Unlike [`Terminal::dump_vt`] — whose consumers replay into a freshly
    /// constructed [`Terminal`] — every consumer of this writes to a REAL tty
    /// that may carry mode leftovers from whatever ran before. So it opens with
    /// [`DRAWABLE_STATE_RESET`] rather than delegating that to callers: the
    /// attach takeover, the daemon's initial replay, and the escape-to-shell
    /// overlay source swap all get it, and none of them can forget it.
    pub fn dump_vt_flat(&self) -> Vec<u8> {
        let mut out = Vec::from(DRAWABLE_STATE_RESET);
        out.extend_from_slice(&self.dump_vt_impl(true));
        out
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
            // Single-screen target: just the active grid from home. `draw_grid`
            // homes, so absolute positioning is height-independent here.
            self.draw_grid(&mut out, self.scr(), &mut st);
            self.dump_graphics(&mut out);
            self.dump_modes(&mut out);
            self.dump_cursor(&mut out, &mut st, CursorAnchor::Absolute);
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
        // The scrollback-flow branch places the grid by a continuous newline
        // flow that lands at the target's BOTTOM (height-dependent), so its
        // cursor must be anchored relative to that flow, not by an absolute CUP
        // (the multi-client taller-target offset bug). The `\x1b[H`-homed
        // else-branch, and the alt path below (which re-homes + redraws the alt
        // grid), stay absolute.
        let cursor_anchor = if sb_len > 0 && !self.alt_active {
            CursorAnchor::Relative
        } else {
            CursorAnchor::Absolute
        };
        if sb_len > 0 {
            for i in 0..sb_len {
                let row = self.primary.scrollback_row(i).unwrap();
                self.emit_terminated_row(&mut out, row, &mut st);
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
            // 1049 only SAVES the cursor, it does not home it — so the park
            // above is already snapshotted by this point, and `draw_grid`'s own
            // home is free to move it.
            self.draw_grid(&mut out, &self.alt, &mut st);
        }

        self.dump_graphics(&mut out);
        self.dump_modes(&mut out);
        self.dump_cursor(&mut out, &mut st, cursor_anchor);
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
        out.push_str("\x1b[?25l");
        out.push_str(DRAWABLE_STATE_RESET);
        out.push_str("\x1b[2J");
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
        // Cleared, then drawn from `draw_grid`'s home, so absolute positioning
        // is correct.
        self.dump_cursor(&mut out, &mut st, CursorAnchor::Absolute);
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
    ///
    /// Homes the cursor itself rather than trusting callers to: every caller
    /// wants a full-screen repaint from row 0, and the one that forgot the
    /// `\x1b[H` painted the alt grid from wherever the preceding primary flow
    /// had parked the cursor — a target-height-dependent origin, which put the
    /// cursor above its content on a taller client (the multi-client offset
    /// bug). Self-enforcing the precondition makes that bug class unreachable.
    fn draw_grid(&self, out: &mut String, grid: &Screen, st: &mut EmitState) {
        out.push_str("\x1b[H");
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

    /// `CursorAnchor::Absolute` positions the cursor with an absolute CUP —
    /// correct for every path that homes (`\x1b[H`) and redraws the grid from the
    /// top, so the target row is height-independent by construction. `Relative`
    /// positions it relative to the BOTTOM of the just-flowed content — for the
    /// scrollback-flow path, where the grid was replayed as a continuous
    /// newline flow that lands at the target terminal's bottom regardless of its
    /// height. An absolute CUP there assumes the SOURCE height and lands the
    /// cursor too high when the target is taller (the multi-client cursor-offset
    /// bug); the relative move anchors to where the flow actually left the
    /// content, so it is correct at any replay height.
    fn dump_cursor(&self, out: &mut String, st: &mut EmitState, anchor: CursorAnchor) {
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
        match anchor {
            CursorAnchor::Absolute => {
                let _ = write!(
                    out,
                    "\x1b[{};{}H",
                    self.cursor.row.saturating_sub(top) + 1,
                    print_col + 1
                );
            }
            CursorAnchor::Relative => {
                // The flow left the cursor at the end of the last grid row
                // (row `rows-1` in the target, wherever the flow landed it).
                // Move up to the cursor's grid row and to its column — both
                // height-independent, so the replay is consistent whether the
                // target is the source height or taller. Origin mode does not
                // affect a CUU/CHA the way it affects an absolute CUP, so `top`
                // is intentionally not applied here.
                let up = (self.rows().saturating_sub(1)).saturating_sub(self.cursor.row);
                out.push('\r');
                if up > 0 {
                    let _ = write!(out, "\x1b[{up}A");
                }
                if print_col > 0 {
                    let _ = write!(out, "\x1b[{}G", print_col + 1);
                }
            }
        }
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

#[cfg(test)]
mod cursor_mismatch_tests {
    use crate::terminal::Terminal;

    /// Dump `session` and replay it into a fresh `target_rows x target_cols`
    /// terminal — the DumpDiff-into-a-client-sized-terminal path (posh cursor
    /// offset). Returns the replay terminal so a test can inspect where the
    /// cursor and content landed.
    fn replay_into(session: &Terminal, target_rows: u16, target_cols: u16) -> Terminal {
        let dump = session.dump_vt();
        let mut t = Terminal::with_scrollback(target_rows, target_cols, 1000);
        t.process(&dump);
        t
    }

    /// The text on the row the cursor is on, trailing blanks trimmed.
    fn cursor_row_text(t: &Terminal) -> String {
        let c = t.cursor();
        t.screen()
            .row(c.row)
            .map(|r| r.text(false).trim_end().to_string())
            .unwrap_or_default()
    }

    /// The core invariant of the fix: whatever content the SOURCE cursor sat on,
    /// the REPLAYED cursor must sit on the same content at any replay height —
    /// the cursor tracks the content, never an absolute source row. `marker` is a
    /// unique substring on the source cursor's row.
    fn assert_cursor_on_marker(session: &Terminal, rows: u16, cols: u16, marker: &str) {
        let src = cursor_row_text(session);
        assert!(
            src.contains(marker),
            "test setup: source cursor row {src:?} must contain marker {marker:?}"
        );
        let replay = replay_into(session, rows, cols);
        let got = cursor_row_text(&replay);
        assert!(
            got.contains(marker),
            "cursor landed on {got:?} at {rows}x{cols}, expected the row with \
             {marker:?} (source cursor row was {src:?})",
        );
    }

    /// A 24-row session scrolled far enough to fill scrollback, cursor left at
    /// the start of the row below the last line. The base for tests that then
    /// position the cursor themselves; `scrolled_session` adds a prompt.
    fn primed_session() -> Terminal {
        let mut s = Terminal::with_scrollback(24, 80, 1000);
        for i in 0..40u16 {
            s.process(format!("line {i:02}\r\n").as_bytes());
        }
        assert!(s.primary_scrollback_len() > 0, "must have scrollback");
        s
    }

    /// A scrolled 24-row session with the cursor at the bottom prompt.
    fn scrolled_session() -> Terminal {
        let mut s = primed_session();
        s.process(b"prompt$ ");
        s
    }

    /// The original repro: scrollback + bottom cursor, replayed into a TALLER
    /// terminal, the cursor must stay on the prompt (was: offset up by the
    /// height difference).
    #[test]
    fn taller_replay_keeps_cursor_on_the_bottom_prompt() {
        let s = scrolled_session();
        assert_eq!(s.cursor().row, 23, "source cursor on the bottom row");
        assert_cursor_on_marker(&s, 24, 80, "prompt$"); // same size (baseline)
        assert_cursor_on_marker(&s, 32, 80, "prompt$"); // taller (the bug)
        assert_cursor_on_marker(&s, 50, 80, "prompt$"); // much taller
    }

    /// A scrolled session whose cursor is MID-SCREEN (not the bottom row): the
    /// cursor must still land on its own content in a taller replay, not an
    /// absolute row. Exercises the up>0-but-not-max relative move.
    #[test]
    fn taller_replay_keeps_cursor_on_a_mid_screen_row() {
        let mut s = primed_session();
        // Land the cursor on a mid-screen row with a unique marker, no trailing
        // newline so it stays put.
        s.process(b"MIDROW-MARK");
        s.process(b"\x1b[10;1H"); // move up into the middle of the grid
        s.process(b"X"); // mark the mid row too so it's addressable
        let c = s.cursor();
        assert!(c.row > 0 && c.row < 23, "cursor is mid-screen at row {}", c.row);
        let marker = cursor_row_text(&s);
        let marker = marker.trim_end();
        assert_cursor_on_marker(&s, 24, 80, marker);
        assert_cursor_on_marker(&s, 32, 80, marker);
    }

    /// Cursor on the TOP row (row 0) of a scrolled session: the relative move is
    /// the maximum (up = rows-1). Must land on row 0's content in any replay.
    #[test]
    fn taller_replay_keeps_cursor_on_the_top_row() {
        let mut s = primed_session();
        s.process(b"\x1b[1;1HTOPMARK"); // home, write a marker on row 0
        s.process(b"\x1b[1;1H"); // cursor back to row 0
        assert_eq!(s.cursor().row, 0, "cursor on the top row");
        assert_cursor_on_marker(&s, 24, 80, "TOPMARK");
        assert_cursor_on_marker(&s, 32, 80, "TOPMARK");
    }

    /// SHORTER target than the source (24 dump -> 20 rows): the reverse of the
    /// bug. The relative move must not underflow, and the cursor must stay on the
    /// prompt content (which the shorter terminal keeps at its bottom).
    #[test]
    fn shorter_replay_keeps_cursor_on_the_prompt() {
        let s = scrolled_session();
        assert_cursor_on_marker(&s, 20, 80, "prompt$");
        assert_cursor_on_marker(&s, 10, 80, "prompt$");
    }

    /// WIDER target (cols mismatch) at a taller height: the horizontal dimension
    /// must not disturb the vertical anchoring. The prompt row's column position
    /// is preserved (col unchanged by the row-relative move).
    #[test]
    fn wider_and_taller_replay_keeps_cursor_on_the_prompt() {
        let s = scrolled_session();
        let src_col = s.cursor().col;
        let replay = replay_into(&s, 32, 120);
        assert!(
            cursor_row_text(&replay).contains("prompt$"),
            "wider+taller: cursor must stay on the prompt row"
        );
        assert_eq!(
            replay.cursor().col,
            src_col,
            "the row-relative move must not shift the column"
        );
    }

    /// NO scrollback (the session never scrolled): dump takes the absolute
    /// `\x1b[H`-home branch, NOT the relative one. A taller replay puts the
    /// content at the TOP (home) and the cursor stays on its absolute row — this
    /// documents that the absolute branch is intentionally height-preserving
    /// (top-anchored), the complement of the scrollback branch's bottom anchor.
    #[test]
    fn no_scrollback_replay_is_top_anchored_and_consistent() {
        let mut s = Terminal::with_scrollback(24, 80, 1000);
        s.process(b"\x1b[H"); // home
        s.process(b"first line top"); // row 0
        s.process(b"\x1b[3;1Hthird row PROMPT"); // row 2, leave cursor here
        assert_eq!(s.primary_scrollback_len(), 0, "no scrollback for this case");
        assert_eq!(s.cursor().row, 2);
        // Absolute branch: content homed at top, cursor on its own content row,
        // consistent at same size and taller.
        assert_cursor_on_marker(&s, 24, 80, "PROMPT");
        assert_cursor_on_marker(&s, 32, 80, "PROMPT");
    }

    /// ALT SCREEN with scrollback, replayed TALLER. `dump_vt` routes this to
    /// `CursorAnchor::Absolute` (the relative anchor is gated on
    /// `sb_len > 0 && !alt_active`), because the alt branch re-homes and redraws
    /// the alt grid rather than placing it by a bottom-landing flow. This test
    /// pins that the absolute anchor really is height-independent here: a
    /// full-screen app's cursor must land on its own content at any replay
    /// height, both mid-screen and on the bottom row (the height-difference-
    /// sensitive case).
    #[test]
    fn taller_replay_keeps_alt_screen_cursor_on_its_content() {
        // Primary has scrollback, so only `alt_active` keeps this off the
        // relative path — the exact gate under test.
        let mut s = primed_session();
        s.process(b"\x1b[?1049h"); // enter the alt screen
        s.process(b"\x1b[5;1HALTMARK"); // mid-screen content, cursor left on it
        assert_eq!(s.cursor().row, 4, "alt cursor mid-screen");
        assert_cursor_on_marker(&s, 24, 80, "ALTMARK"); // same size (baseline)
        assert_cursor_on_marker(&s, 32, 80, "ALTMARK"); // taller
        assert_cursor_on_marker(&s, 50, 80, "ALTMARK"); // much taller

        // Bottom row of the alt grid: an absolute CUP computed against the
        // SOURCE height would land this short of the content on a taller target.
        s.process(b"\x1b[24;1HALTBOTTOM");
        assert_eq!(s.cursor().row, 23, "alt cursor on the bottom row");
        assert_cursor_on_marker(&s, 24, 80, "ALTBOTTOM");
        assert_cursor_on_marker(&s, 32, 80, "ALTBOTTOM");
        assert_cursor_on_marker(&s, 50, 80, "ALTBOTTOM");
    }

    /// Attempt to construct a consumer that DEPENDS on the cursor landing at the
    /// parked primary position when the alt grid is drawn — i.e. something the
    /// post-`?1049h` home would break. Loads the alt path with everything in the
    /// dump interval that could plausibly inherit a cursor: non-default tab stops
    /// (whose HTS replay itself moves the cursor), a DECSTBM scroll region, and
    /// content on the region's edges. If any of it inherited the parked position,
    /// a same-size round trip would diverge from the source.
    #[test]
    fn alt_dump_interval_does_not_inherit_the_parked_cursor() {
        let mut s = primed_session();
        // Park the primary cursor somewhere distinctive and NOT home, so an
        // inherited position would be unmistakable.
        s.process(b"\x1b[12;40H");
        s.process(b"\x1b[?1049h"); // enter alt; 1049 saves the parked position
        s.process(b"\x1b[3g"); // clear tab stops
        s.process(b"\x1b[1;5H\x1bH"); // HTS at col 5
        s.process(b"\x1b[1;33H\x1bH"); // HTS at col 33
        s.process(b"\x1b[4;20r"); // DECSTBM scroll region
        s.process(b"\x1b[6;1HREGION-MARK");
        assert_eq!(s.cursor().row, 5, "alt cursor inside the scroll region");

        // Same size: the round trip must reproduce the source exactly.
        let same = replay_into(&s, 24, 80);
        assert_eq!(
            same.dump_text(),
            s.dump_text(),
            "same-size alt round trip must be content-identical"
        );
        assert_cursor_on_marker(&s, 24, 80, "REGION-MARK");
        // Taller: content must still land on its own rows, cursor with it.
        assert_cursor_on_marker(&s, 32, 80, "REGION-MARK");
        assert_cursor_on_marker(&s, 50, 80, "REGION-MARK");
    }

    /// `dump_vt_flat` replays onto a REAL tty, which may carry mode leftovers
    /// from whatever ran before it. Strand every mode `DRAWABLE_STATE_RESET`
    /// claims to reset — each one non-default on the TARGET and default in the
    /// SOURCE, the asymmetry `dump_modes` cannot correct — and assert the
    /// replay still reproduces the source exactly. posh#141.
    #[test]
    fn flat_replay_normalizes_a_dirty_target() {
        let mut source = Terminal::with_scrollback(24, 80, 100);
        source.process(b"first row content");
        source.process(b"\x1b[3;1Hthird row MARKER");

        let mut target = Terminal::with_scrollback(24, 80, 100);
        target.process(b"\x1b[5;15r"); // DECSTBM margins
        target.process(b"\x1b[?6h"); // DECOM origin mode
        target.process(b"\x1b[?5h"); // reverse video
        target.process(b"\x1b[?7l"); // autowrap off
        target.process(b"\x1b[?12h"); // cursor blink
        target.process(b"\x1b[?2026h"); // synchronized output
        target.process(b"\x1b[20h"); // LNM
        target.process(b"\x1b[4h"); // insert mode
        target.process(b"\x1b(0"); // G0 = DEC special graphics
        target.process(b"\x1b)0"); // G1 = DEC special graphics
        target.process(b"\x0e"); // SO: shift G1 in
        target.process(b"\x1b[31;1;4m"); // dirty pen
        target.process(b"\x1b[1\"q"); // DECSCA protected
        target.process(b"\x1b]8;;http://example.invalid\x1b\\"); // open hyperlink

        target.process(&source.dump_vt_flat());

        for r in 0..source.rows() {
            assert_eq!(
                target.screen().row(r).unwrap().text(true),
                source.screen().row(r).unwrap().text(true),
                "row {r} diverged replaying onto a dirty target"
            );
        }
        assert_eq!(target.cursor().row, source.cursor().row, "cursor row");
        assert_eq!(target.cursor().col, source.cursor().col, "cursor col");
    }

    /// Pending-wrap at the cursor, in a scrolled session, replayed TALLER: the
    /// relative-anchor path emits its own column positioning (CHA) and then the
    /// pending-wrap reprint. The reprinted cell + the armed pending-wrap state
    /// must survive a taller replay exactly as at same size (the reprint runs
    /// after positioning in both anchors, but the relative path is new).
    #[test]
    fn taller_replay_preserves_pending_wrap_at_cursor() {
        let mut s = primed_session();
        // Fill the last grid row to the final column so the cursor is left in
        // the pending-wrap state (armed, but not yet wrapped). `pending_wrap` is
        // internal, so assert its OBSERVABLE effect: the cursor rests on the
        // final column of the filled row (not wrapped to the next line).
        s.process(&[b'W'; 80]);
        let sc = s.cursor();
        assert_eq!(sc.col, 79, "source cursor armed at the last column");

        let replay = replay_into(&s, 32, 80);
        let rc = replay.cursor();
        // The cursor sits on the row whose content is the run of 'W's, at the
        // same final column — the reprint landed at the same cell across the
        // taller replay, and the cursor did not spuriously wrap to a new row.
        assert!(
            cursor_row_text(&replay).contains("WWWW"),
            "cursor on the filled row, got {:?}",
            cursor_row_text(&replay)
        );
        assert_eq!(rc.col, sc.col, "pending-wrap column preserved across the taller replay");
    }

    /// Same-size round-trip: the fix must not change the common case. The cursor
    /// lands on the exact source row AND content when replayed at the source
    /// size (the guard that the relative move is a no-op at source height).
    #[test]
    fn same_size_roundtrip_is_unchanged() {
        let s = scrolled_session();
        let replay = replay_into(&s, 24, 80);
        assert_eq!(
            replay.cursor().row,
            s.cursor().row,
            "same-size replay preserves the absolute cursor row"
        );
        assert_eq!(replay.cursor().col, s.cursor().col, "and the column");
        assert!(cursor_row_text(&replay).contains("prompt$"));
    }
}
