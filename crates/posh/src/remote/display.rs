//! Display diff renderer (port of mosh's terminaldisplay.cc): given the
//! previous and next screen snapshots, emit the minimal escape sequences
//! that morph the physical terminal from one to the other — per-row cell
//! diffs with SGR pen tracking, cursor repositioning, title updates, and
//! mode synchronization. Also hosts the connection-status banner (port of
//! mosh's NotificationEngine, simplified).

use std::fmt::Write;

use posh_term::{wcwidth, Cell, Color, MouseMode, MouseProtocol, Style, Terminal, UnderlineStyle};

/// A frozen picture of what a terminal shows: the visible grid plus the
/// handful of modes the renderer keeps in sync on the outer terminal.
/// Overlays (predictions, notifications) draw onto this before diffing.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<Vec<Cell>>,
    pub wrapped: Vec<bool>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub title: String,
    pub reverse_video: bool,
    pub bracketed_paste: bool,
    pub focus_reporting: bool,
    pub app_cursor_keys: bool,
    pub app_keypad: bool,
    /// 0 = off, else the DECSET number (9/1000/1002/1003).
    pub mouse_mode: u16,
    /// 0 = default encoding, else the DECSET number (1005/1006/1016).
    pub mouse_encoding: u16,
}

pub fn blank_cell() -> Cell {
    Cell {
        ch: ' ',
        width: 1,
        ..Cell::default()
    }
}

impl Snapshot {
    pub fn blank(rows: u16, cols: u16) -> Snapshot {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Snapshot {
            rows,
            cols,
            cells: (0..rows)
                .map(|_| vec![blank_cell(); cols as usize])
                .collect(),
            wrapped: vec![false; rows as usize],
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            title: String::new(),
            reverse_video: false,
            bracketed_paste: false,
            focus_reporting: false,
            app_cursor_keys: false,
            app_keypad: false,
            mouse_mode: 0,
            mouse_encoding: 0,
        }
    }

    pub fn from_term(term: &Terminal) -> Snapshot {
        let screen = term.screen();
        let (rows, cols) = (screen.rows(), screen.cols());
        let mut cells = Vec::with_capacity(rows as usize);
        let mut wrapped = Vec::with_capacity(rows as usize);
        for r in 0..rows {
            let row = screen.row(r).expect("row in range");
            cells.push(row.cells().to_vec());
            wrapped.push(row.wrapped());
        }
        let cursor = term.cursor();
        Snapshot {
            rows,
            cols,
            cells,
            wrapped,
            cursor_row: cursor.row.min(rows - 1),
            cursor_col: cursor.col.min(cols - 1),
            cursor_visible: cursor.visible,
            title: term.title().to_string(),
            reverse_video: term.reverse_video(),
            bracketed_paste: term.bracketed_paste(),
            focus_reporting: term.focus_reporting(),
            app_cursor_keys: term.app_cursor_keys(),
            app_keypad: term.app_keypad(),
            mouse_mode: match term.mouse_mode() {
                MouseMode::None => 0,
                MouseMode::X10 => 9,
                MouseMode::Normal => 1000,
                MouseMode::ButtonEvent => 1002,
                MouseMode::AnyEvent => 1003,
            },
            mouse_encoding: match term.mouse_protocol() {
                MouseProtocol::Normal => 0,
                MouseProtocol::Utf8 => 1005,
                MouseProtocol::Sgr => 1006,
                MouseProtocol::SgrPixel => 1016,
            },
        }
    }

    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        self.cells.get(row as usize)?.get(col as usize)
    }

    pub fn cell_mut(&mut self, row: u16, col: u16) -> Option<&mut Cell> {
        self.cells.get_mut(row as usize)?.get_mut(col as usize)
    }
}

/// Restores the outer terminal on exit (mosh Display::close): default pen,
/// visible cursor, mouse/paste/focus modes off, scroll region reset.
pub fn close() -> &'static [u8] {
    b"\x1b[0m\x1b[?25h\x1b[?1l\x1b>\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?9l\
      \x1b[?1016l\x1b[?1006l\x1b[?1005l\x1b[?2004l\x1b[?1004l\x1b[r"
}

/// SGR parameter string reproducing `style` from a reset pen.
fn sgr_params(style: &Style) -> String {
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
    let mut color = |base: u16, extended: u16, c: &Color| match *c {
        Color::Default => {}
        Color::Indexed(i) if i < 8 => {
            let _ = write!(s, ";{}", base + u16::from(i));
        }
        Color::Indexed(i) if i < 16 => {
            let _ = write!(s, ";{}", base + 60 + u16::from(i) - 8);
        }
        Color::Indexed(i) => {
            let _ = write!(s, ";{extended}:5:{i}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(s, ";{extended}:2:{r}:{g}:{b}");
        }
    };
    color(30, 38, &style.fg);
    color(40, 48, &style.bg);
    if style.underline_color != Color::Default {
        match style.underline_color {
            Color::Indexed(i) => {
                let _ = write!(s, ";58:5:{i}");
            }
            Color::Rgb(r, g, b) => {
                let _ = write!(s, ";58:2:{r}:{g}:{b}");
            }
            Color::Default => {}
        }
    }
    s
}

/// Escape-stream builder with cursor/pen bookkeeping (mosh FrameState).
struct FrameState {
    out: String,
    x: i32,
    y: i32,
    style: Style,
    cursor_visible: bool,
}

impl FrameState {
    fn append(&mut self, s: &str) {
        self.out.push_str(s);
    }

    fn append_n(&mut self, n: usize, ch: char) {
        for _ in 0..n {
            self.out.push(ch);
        }
    }

    fn append_cell(&mut self, cell: &Cell) {
        self.out.push(if cell.ch == '\0' { ' ' } else { cell.ch });
        self.out.extend(cell.extra.iter());
    }

    fn update_style(&mut self, style: &Style, force: bool) {
        if force || self.style != *style {
            let _ = write!(self.out, "\x1b[{}m", sgr_params(style));
            self.style = *style;
        }
    }

    /// Whether EL/ECH may be used: only when erasing with the default pen
    /// (we make no BCE assumption about the outer terminal).
    fn can_use_erase(&self) -> bool {
        self.style == Style::default()
    }

    fn append_move(&mut self, y: i32, x: i32) {
        let (last_x, last_y) = (self.x, self.y);
        self.x = x;
        self.y = y;
        if last_x != -1 && last_y != -1 {
            // CR and/or short LF runs are cheap.
            if x == 0 && y - last_y >= 0 && y - last_y < 5 {
                if last_x != 0 {
                    self.out.push('\r');
                }
                self.append_n((y - last_y) as usize, '\n');
                return;
            }
            // Short backspace runs too.
            if y == last_y && x - last_x < 0 && x - last_x > -5 {
                self.append_n((last_x - x) as usize, '\u{8}');
                return;
            }
        }
        let _ = write!(self.out, "\x1b[{};{}H", y + 1, x + 1);
    }

    fn append_silent_move(&mut self, y: i32, x: i32) {
        if self.x == x && self.y == y {
            return;
        }
        // Hide the cursor before jumping it around.
        if self.cursor_visible {
            self.append("\x1b[?25l");
            self.cursor_visible = false;
        }
        self.append_move(y, x);
    }
}

fn cell_width(cell: &Cell) -> u16 {
    u16::from(cell.width.max(1))
}

/// Emits the escape stream that morphs a terminal showing `last` into one
/// showing `f`. With `initialized == false` the outer terminal state is
/// unknown: the screen is cleared and fully repainted (first frame, resize,
/// Ctrl-L). The stream always leaves the pen at SGR default.
pub fn new_frame(initialized: bool, last: &Snapshot, f: &Snapshot) -> Vec<u8> {
    let mut init = initialized;
    let mut frame = FrameState {
        out: String::new(),
        x: 0,
        y: 0,
        style: Style::default(),
        cursor_visible: last.cursor_visible,
    };

    // Title.
    if !init || f.title != last.title {
        let _ = write!(frame.out, "\x1b]0;{}\x07", f.title);
    }

    // Reverse video.
    if !init || f.reverse_video != last.reverse_video {
        frame.append(if f.reverse_video {
            "\x1b[?5h"
        } else {
            "\x1b[?5l"
        });
    }

    // Size change forces a full repaint.
    if !init || f.rows != last.rows || f.cols != last.cols {
        frame.append("\x1b[r\x1b[0m\x1b[H\x1b[2J");
        init = false;
        frame.x = 0;
        frame.y = 0;
        frame.style = Style::default();
    } else {
        frame.x = i32::from(last.cursor_col);
        frame.y = i32::from(last.cursor_row);
    }

    if !init {
        frame.append("\x1b[?25l");
        frame.cursor_visible = false;
    }

    // Row-by-row cell diff.
    let blank_row: Vec<Cell> = vec![blank_cell(); f.cols as usize];
    let mut wrap = false;
    for y in 0..f.rows {
        let old_row: &[Cell] = if init {
            last.cells.get(y as usize).map_or(&blank_row, |r| r)
        } else {
            &blank_row
        };
        wrap = put_row(init, &mut frame, f, y, old_row, wrap);
    }

    // Cursor location.
    if !init || frame.y != i32::from(f.cursor_row) || frame.x != i32::from(f.cursor_col) {
        frame.append_move(i32::from(f.cursor_row), i32::from(f.cursor_col));
    }

    // Cursor visibility.
    if !init || f.cursor_visible != frame.cursor_visible {
        frame.append(if f.cursor_visible {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        });
        frame.cursor_visible = f.cursor_visible;
    }

    // Leave the pen in a known (default) state for the next frame.
    frame.update_style(&Style::default(), !init);

    // Bracketed paste.
    if !init || f.bracketed_paste != last.bracketed_paste {
        frame.append(if f.bracketed_paste {
            "\x1b[?2004h"
        } else {
            "\x1b[?2004l"
        });
    }

    // Application cursor keys / keypad: synced so local keys produce the
    // byte sequences the remote application asked for.
    if !init || f.app_cursor_keys != last.app_cursor_keys {
        frame.append(if f.app_cursor_keys {
            "\x1b[?1h"
        } else {
            "\x1b[?1l"
        });
    }
    if !init || f.app_keypad != last.app_keypad {
        frame.append(if f.app_keypad { "\x1b=" } else { "\x1b>" });
    }

    // Mouse reporting mode.
    if !init || f.mouse_mode != last.mouse_mode {
        if f.mouse_mode == 0 {
            frame.append("\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?9l");
        } else {
            if init && last.mouse_mode != 0 {
                let _ = write!(frame.out, "\x1b[?{}l", last.mouse_mode);
            }
            let _ = write!(frame.out, "\x1b[?{}h", f.mouse_mode);
        }
    }

    // Focus reporting.
    if !init || f.focus_reporting != last.focus_reporting {
        frame.append(if f.focus_reporting {
            "\x1b[?1004h"
        } else {
            "\x1b[?1004l"
        });
    }

    // Mouse encoding.
    if !init || f.mouse_encoding != last.mouse_encoding {
        if f.mouse_encoding == 0 {
            frame.append("\x1b[?1016l\x1b[?1006l\x1b[?1005l");
        } else {
            if init && last.mouse_encoding != 0 {
                let _ = write!(frame.out, "\x1b[?{}l", last.mouse_encoding);
            }
            let _ = write!(frame.out, "\x1b[?{}h", f.mouse_encoding);
        }
    }

    frame.out.into_bytes()
}

/// Diffs one row (mosh Display::put_row). Returns true when the cursor was
/// left wrapped onto the next row.
fn put_row(
    init: bool,
    frame: &mut FrameState,
    f: &Snapshot,
    y: u16,
    old_cells: &[Cell],
    wrap: bool,
) -> bool {
    let row = &f.cells[y as usize];
    let row_wrap = f.wrapped[y as usize];
    let width = f.cols;
    let mut frame_x: u16 = 0;

    // Forced to write the first column because the previous row wrapped.
    if wrap {
        let cell = &row[0];
        frame.update_style(&cell.style, false);
        frame.append_cell(cell);
        let w = cell_width(cell);
        frame_x += w;
        frame.x += i32::from(w);
    }

    let mut clear_count: usize = 0;
    let mut wrote_last_cell = false;
    let mut blank_style = Style::default();

    while frame_x < width {
        let cell = &row[frame_x as usize];

        // Unchanged cell: skip (only when no blank run is pending).
        if init && clear_count == 0 && old_cells.get(frame_x as usize) == Some(cell) {
            frame_x += cell_width(cell);
            continue;
        }

        // Spacer halves of wide chars render nothing of their own.
        if cell.width == 0 {
            frame_x += 1;
            continue;
        }

        // Slurp runs of blank cells with a uniform style.
        if cell.is_blank() {
            if clear_count == 0 {
                blank_style = cell.style;
            }
            if cell.style == blank_style {
                clear_count += 1;
                frame_x += 1;
                continue;
            }
        }

        // Flush a pending blank run within the row.
        if clear_count > 0 {
            frame.append_silent_move(i32::from(y), i32::from(frame_x) - clear_count as i32);
            frame.update_style(&blank_style, false);
            if frame.can_use_erase() && clear_count > 4 {
                let _ = write!(frame.out, "\x1b[{clear_count}X");
            } else {
                frame.append_n(clear_count, ' ');
                frame.x = i32::from(frame_x);
            }
            clear_count = 0;
            // Another blank in a different style restarts the run.
            if cell.is_blank() {
                blank_style = cell.style;
                clear_count = 1;
                frame_x += 1;
                continue;
            }
        }

        // Draw the cell. Writing into the last column leaves the real
        // cursor in an ambiguous (pending-wrap) state: trash our tracked
        // position to force explicit positioning afterwards.
        let w = cell_width(cell);
        if row_wrap && frame_x + w >= width {
            frame.x = -1;
            frame.y = -1;
        }
        frame.append_silent_move(i32::from(y), i32::from(frame_x));
        frame.update_style(&cell.style, false);
        frame.append_cell(cell);
        frame_x += w;
        frame.x += i32::from(w);
        if frame_x >= width {
            wrote_last_cell = true;
        }
    }

    // Blank run reaching the end of the line.
    if clear_count > 0 {
        frame.append_silent_move(i32::from(y), i32::from(frame_x) - clear_count as i32);
        frame.update_style(&blank_style, false);
        if frame.can_use_erase() && !row_wrap {
            frame.append("\x1b[K");
        } else {
            frame.append_n(clear_count, ' ');
            frame.x = i32::from(frame_x);
            wrote_last_cell = true;
        }
    }

    if !(wrote_last_cell && y + 1 < f.rows) {
        return false;
    }
    if row_wrap {
        // Let the real cursor wrap where the content wrapped, so the outer
        // terminal groups the line for selection.
        frame.x = 0;
        frame.y += 1;
        return true;
    }
    frame.append("\r\n");
    frame.x = 0;
    frame.y += 1;
    false
}

// ---------------------------------------------------------------------------
// Connection-status banner (port of mosh's NotificationEngine, simplified
// to the "last contact" countup plus transient messages).

/// Silence threshold before the banner appears (mosh: 6.5s).
pub const SERVER_LATE_AFTER: u64 = 6500; // ms

pub struct NotificationEngine {
    last_word_from_server: u64,
    message: String,
    /// None = permanent message.
    message_expiration: Option<u64>,
    show_quit_keystroke: bool,
}

fn human_readable_duration(num_seconds: u64) -> String {
    if num_seconds < 60 {
        format!("{num_seconds} seconds")
    } else if num_seconds < 3600 {
        format!("{}:{:02}", num_seconds / 60, num_seconds % 60)
    } else {
        format!(
            "{}:{:02}:{:02}",
            num_seconds / 3600,
            (num_seconds / 60) % 60,
            num_seconds % 60
        )
    }
}

impl NotificationEngine {
    pub fn new(now: u64) -> NotificationEngine {
        NotificationEngine {
            last_word_from_server: now,
            message: String::new(),
            message_expiration: Some(0),
            show_quit_keystroke: true,
        }
    }

    pub fn server_heard(&mut self, now: u64) {
        self.last_word_from_server = now;
    }

    pub fn server_late(&self, now: u64) -> bool {
        now.saturating_sub(self.last_word_from_server) > SERVER_LATE_AFTER
    }

    pub fn set_message(&mut self, message: &str, permanent: bool, now: u64) {
        self.message = message.to_string();
        self.message_expiration = if permanent { None } else { Some(now + 1000) };
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// Clears an expired transient message.
    pub fn adjust(&mut self, now: u64) {
        if let Some(expiry) = self.message_expiration {
            if now >= expiry {
                self.message.clear();
            }
        }
    }

    /// How soon the banner needs redrawing, for poll deadlines.
    pub fn wait_time(&self, now: u64) -> u64 {
        let mut wait = u64::MAX;
        if let Some(expiry) = self.message_expiration {
            if !self.message.is_empty() {
                wait = wait.min(expiry.saturating_sub(now));
            }
        }
        if self.server_late(now) {
            wait = wait.min(1000); // countup ticks once a second
        } else {
            let until_late = (self.last_word_from_server + SERVER_LATE_AFTER).saturating_sub(now);
            wait = wait.min(until_late.max(1));
        }
        wait
    }

    /// Draws the reverse-video status line across the top of the screen.
    pub fn apply(&self, fb: &mut Snapshot, now: u64) {
        let time_expired = self.server_late(now);
        if self.message.is_empty() && !time_expired {
            return;
        }

        // Hide the cursor if it sits under the bar.
        if fb.cursor_row == 0 {
            fb.cursor_visible = false;
        }

        let bar_style = Style {
            inverse: true,
            bold: true,
            ..Style::default()
        };
        for cell in fb.cells[0].iter_mut() {
            *cell = Cell {
                style: bar_style,
                ..blank_cell()
            };
        }
        fb.wrapped[0] = false;

        let since_heard = now.saturating_sub(self.last_word_from_server) / 1000;
        let keystroke = if self.show_quit_keystroke {
            " [To quit: Ctrl-^ .]"
        } else {
            ""
        };
        let text = if self.message.is_empty() {
            format!(
                "posh: Last contact {} ago.{}",
                human_readable_duration(since_heard),
                keystroke
            )
        } else if !time_expired {
            format!("posh: {}{}", self.message, keystroke)
        } else {
            format!(
                "posh: {} ({} without contact.){}",
                self.message,
                human_readable_duration(since_heard),
                keystroke
            )
        };

        let mut col: u16 = 0;
        for ch in text.chars() {
            let w = wcwidth(ch);
            if w == 0 {
                continue;
            }
            if u16::from(col) + u16::from(w) > fb.cols {
                break;
            }
            fb.cells[0][col as usize] = Cell {
                ch,
                style: bar_style,
                width: w,
                ..Cell::default()
            };
            if w == 2 {
                fb.cells[0][col as usize + 1] = Cell {
                    ch: '\0',
                    style: bar_style,
                    width: 0,
                    ..Cell::default()
                };
            }
            col += u16::from(w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term_with(rows: u16, cols: u16, bytes: &[u8]) -> Terminal {
        let mut t = Terminal::with_scrollback(rows, cols, 0);
        t.process(bytes);
        t
    }

    /// Renders the diff between two byte streams and verifies that a third
    /// terminal seeded with the previous state ends up matching the next
    /// state cell-for-cell (and cursor) after processing the diff.
    fn assert_morph(rows: u16, cols: u16, prev_bytes: &[u8], extra_bytes: &[u8]) {
        let prev = term_with(rows, cols, prev_bytes);
        let mut next_term = term_with(rows, cols, prev_bytes);
        next_term.process(extra_bytes);

        let prev_snap = Snapshot::from_term(&prev);
        let next_snap = Snapshot::from_term(&next_term);
        let diff = new_frame(true, &prev_snap, &next_snap);

        let mut verify = term_with(rows, cols, prev_bytes);
        verify.process(&diff);
        assert_screens_match(&verify, &next_term, &diff);
    }

    fn assert_screens_match(got: &Terminal, want: &Terminal, diff: &[u8]) {
        let diff_str = String::from_utf8_lossy(diff).into_owned();
        for r in 0..want.rows() {
            for c in 0..want.cols() {
                let g = got.screen().cell(r, c).unwrap();
                let w = want.screen().cell(r, c).unwrap();
                let g_ch = if g.ch == '\0' { ' ' } else { g.ch };
                let w_ch = if w.ch == '\0' { ' ' } else { w.ch };
                assert_eq!(g_ch, w_ch, "char mismatch at ({r},{c}); diff={diff_str:?}");
                // Blank cells only need matching backgrounds to look right.
                if !w.is_blank() {
                    assert_eq!(
                        g.style, w.style,
                        "style mismatch at ({r},{c}); diff={diff_str:?}"
                    );
                } else {
                    assert_eq!(
                        g.style.bg, w.style.bg,
                        "bg mismatch at ({r},{c}); diff={diff_str:?}"
                    );
                }
            }
        }
        assert_eq!(got.cursor().row, want.cursor().row, "cursor row");
        assert_eq!(got.cursor().col, want.cursor().col, "cursor col");
    }

    #[test]
    fn initial_frame_paints_everything() {
        let next = term_with(5, 20, b"hello\r\nworld");
        let blank = Snapshot::blank(5, 20);
        let bytes = new_frame(false, &blank, &Snapshot::from_term(&next));
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\x1b[2J"), "first frame clears: {s:?}");

        let mut verify = Terminal::with_scrollback(5, 20, 0);
        verify.process(&bytes);
        assert_screens_match(&verify, &next, &bytes);
    }

    #[test]
    fn incremental_frame_avoids_clear_screen() {
        let prev = term_with(5, 20, b"hello");
        let mut next = term_with(5, 20, b"hello");
        next.process(b" world");
        let diff = new_frame(
            true,
            &Snapshot::from_term(&prev),
            &Snapshot::from_term(&next),
        );
        let s = String::from_utf8_lossy(&diff);
        assert!(!s.contains("\x1b[2J"), "no clear-screen in a diff: {s:?}");
        // The unchanged "hello" (and even the blank cell after it) is
        // skipped; only the new text is written.
        assert!(s.contains("world"), "appended text present: {s:?}");
        assert!(!s.contains("hello"), "unchanged text not rewritten: {s:?}");
    }

    #[test]
    fn morph_simple_text() {
        assert_morph(5, 20, b"hello\r\nworld", b"\r\nthird line");
    }

    #[test]
    fn morph_colored_text() {
        assert_morph(
            5,
            30,
            b"\x1b[31mred\x1b[0m plain",
            b"\r\n\x1b[1;44mbold on blue\x1b[0m",
        );
    }

    #[test]
    fn morph_overwrite_and_erase() {
        // Overwrite a long line with a shorter one (EL path).
        assert_morph(4, 30, b"a long line of text here", b"\x1b[1;1Hshort\x1b[K");
    }

    #[test]
    fn morph_cursor_move_only() {
        let prev = term_with(5, 20, b"abc");
        let mut next = term_with(5, 20, b"abc");
        next.process(b"\x1b[4;7H");
        let diff = new_frame(
            true,
            &Snapshot::from_term(&prev),
            &Snapshot::from_term(&next),
        );
        let mut verify = term_with(5, 20, b"abc");
        verify.process(&diff);
        assert_eq!(verify.cursor().row, 3);
        assert_eq!(verify.cursor().col, 6);
    }

    #[test]
    fn morph_mid_row_change() {
        assert_morph(3, 30, b"the quick brown fox", b"\x1b[1;5HSLOW!");
    }

    #[test]
    fn morph_wide_characters() {
        assert_morph(4, 20, "ab\u{4F60}\u{597D}cd".as_bytes(), b"\r\nnext");
    }

    #[test]
    fn identical_states_emit_nothing() {
        let term = term_with(5, 20, b"steady state");
        let snap = Snapshot::from_term(&term);
        let diff = new_frame(true, &snap, &snap);
        assert!(
            diff.is_empty(),
            "no-op diff should be empty: {:?}",
            String::from_utf8_lossy(&diff)
        );
    }

    #[test]
    fn title_change_emits_osc() {
        let prev = term_with(3, 20, b"x");
        let mut next = term_with(3, 20, b"x");
        next.process(b"\x1b]0;new title\x07");
        let diff = new_frame(
            true,
            &Snapshot::from_term(&prev),
            &Snapshot::from_term(&next),
        );
        let s = String::from_utf8_lossy(&diff);
        assert!(s.contains("\x1b]0;new title\x07"), "{s:?}");
    }

    #[test]
    fn size_change_forces_repaint() {
        let prev = term_with(5, 20, b"hello");
        let next = term_with(10, 40, b"hello");
        let diff = new_frame(
            true,
            &Snapshot::from_term(&prev),
            &Snapshot::from_term(&next),
        );
        let s = String::from_utf8_lossy(&diff);
        assert!(s.contains("\x1b[2J"), "resize repaints: {s:?}");
        let mut verify = Terminal::with_scrollback(10, 40, 0);
        verify.process(&diff);
        assert_screens_match(&verify, &next, &diff);
    }

    #[test]
    fn mode_changes_synced() {
        let prev = term_with(3, 20, b"");
        let mut next = term_with(3, 20, b"");
        next.process(b"\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1h");
        let diff = new_frame(
            true,
            &Snapshot::from_term(&prev),
            &Snapshot::from_term(&next),
        );
        let s = String::from_utf8_lossy(&diff);
        assert!(s.contains("\x1b[?2004h"), "{s:?}");
        assert!(s.contains("\x1b[?1000h"), "{s:?}");
        assert!(s.contains("\x1b[?1006h"), "{s:?}");
        assert!(s.contains("\x1b[?1h"), "{s:?}");

        // And turning them back off.
        let diff_off = new_frame(
            true,
            &Snapshot::from_term(&next),
            &Snapshot::from_term(&prev),
        );
        let s = String::from_utf8_lossy(&diff_off);
        assert!(s.contains("\x1b[?2004l"), "{s:?}");
        assert!(s.contains("\x1b[?1000l"), "{s:?}");
    }

    #[test]
    fn cursor_visibility_synced() {
        let prev = term_with(3, 20, b"");
        let mut next = term_with(3, 20, b"");
        next.process(b"\x1b[?25l");
        let diff = new_frame(
            true,
            &Snapshot::from_term(&prev),
            &Snapshot::from_term(&next),
        );
        assert!(String::from_utf8_lossy(&diff).contains("\x1b[?25l"));
    }

    #[test]
    fn notification_banner_appears_when_late() {
        let mut notify = NotificationEngine::new(0);
        let mut fb = Snapshot::blank(5, 60);
        // Not late yet: nothing drawn.
        notify.apply(&mut fb, 1000);
        assert_eq!(fb.cells[0][0].ch, ' ');
        assert!(!fb.cells[0][0].style.inverse);
        // 7 seconds of silence: banner appears.
        notify.apply(&mut fb, 7000);
        let text: String = fb.cells[0]
            .iter()
            .filter(|c| c.width > 0)
            .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
            .collect();
        assert!(
            text.starts_with("posh: Last contact 7 seconds ago."),
            "{text:?}"
        );
        assert!(fb.cells[0][0].style.inverse, "banner is reverse-video");

        // Contact resumes: banner goes away.
        notify.server_heard(8000);
        let mut fb2 = Snapshot::blank(5, 60);
        notify.apply(&mut fb2, 8100);
        assert!(!fb2.cells[0][0].style.inverse);
    }

    #[test]
    fn notification_message_expires() {
        let mut notify = NotificationEngine::new(0);
        notify.set_message("Exiting...", false, 100);
        assert_eq!(notify.message(), "Exiting...");
        notify.adjust(500);
        assert_eq!(notify.message(), "Exiting...");
        notify.adjust(1200);
        assert_eq!(notify.message(), "");
        // Permanent messages stick.
        notify.set_message("for good", true, 100);
        notify.adjust(1_000_000);
        assert_eq!(notify.message(), "for good");
    }

    #[test]
    fn notification_hides_cursor_on_top_row() {
        let mut notify = NotificationEngine::new(0);
        notify.set_message("hi", true, 0);
        let mut fb = Snapshot::blank(5, 60);
        fb.cursor_row = 0;
        fb.cursor_visible = true;
        notify.apply(&mut fb, 10);
        assert!(!fb.cursor_visible);
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(human_readable_duration(5), "5 seconds");
        assert_eq!(human_readable_duration(65), "1:05");
        assert_eq!(human_readable_duration(3725), "1:02:05");
    }
}
