//! Screen grid with scrollback ring buffer.

use std::collections::VecDeque;

use crate::cell::{Cell, Style};

/// OSC 133 shell-integration semantic mark attached to a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticMark {
    /// `OSC 133;A`: start of the prompt.
    PromptStart,
    /// `OSC 133;B`: end of the prompt, start of user input.
    InputStart,
    /// `OSC 133;C`: start of command output.
    OutputStart,
    /// `OSC 133;D`: end of the command.
    CommandEnd,
}

/// One screen or scrollback row.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub(crate) cells: Vec<Cell>,
    /// True if this row soft-wraps onto the next (no hard newline).
    pub(crate) wrapped: bool,
    pub(crate) mark: Option<SemanticMark>,
}

impl Row {
    pub(crate) fn blank(cols: usize, style: Style) -> Row {
        Row {
            cells: vec![Cell::blank(style); cols],
            wrapped: false,
            mark: None,
        }
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn wrapped(&self) -> bool {
        self.wrapped
    }

    pub fn mark(&self) -> Option<SemanticMark> {
        self.mark
    }

    fn resize_width(&mut self, cols: usize) {
        if cols < self.cells.len() {
            self.cells.truncate(cols);
            // A wide head whose spacer was cut off cannot render: blank it.
            if let Some(last) = self.cells.last_mut() {
                if last.width == 2 {
                    *last = Cell::blank(Style::default());
                }
            }
        } else {
            self.cells.resize(cols, Cell::blank(Style::default()));
        }
    }

    /// Plain text of the row; `trim` removes trailing whitespace.
    pub fn text(&self, trim: bool) -> String {
        let mut s = String::new();
        for cell in &self.cells {
            if cell.width == 0 {
                continue;
            }
            s.push(if cell.ch == '\0' { ' ' } else { cell.ch });
            s.extend(cell.extra.iter());
        }
        if trim {
            s.truncate(s.trim_end().len());
        }
        s
    }
}

/// A terminal screen: a visible grid plus (for the primary screen) a
/// scrollback ring buffer.
#[derive(Debug, Default)]
pub struct Screen {
    rows: u16,
    cols: u16,
    grid: Vec<Row>,
    scrollback: VecDeque<Row>,
    max_scrollback: usize,
}

impl Screen {
    pub(crate) fn new(rows: u16, cols: u16, max_scrollback: usize) -> Screen {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Screen {
            rows,
            cols,
            grid: (0..rows)
                .map(|_| Row::blank(cols as usize, Style::default()))
                .collect(),
            scrollback: VecDeque::new(),
            max_scrollback,
        }
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        self.grid.get(row as usize)?.cells.get(col as usize)
    }

    pub fn row(&self, row: u16) -> Option<&Row> {
        self.grid.get(row as usize)
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    pub fn scrollback_row(&self, i: usize) -> Option<&Row> {
        self.scrollback.get(i)
    }

    pub(crate) fn row_mut(&mut self, row: u16) -> &mut Row {
        let r = (row as usize).min(self.grid.len() - 1);
        &mut self.grid[r]
    }

    pub(crate) fn cell_mut(&mut self, row: u16, col: u16) -> &mut Cell {
        let cols = self.cols;
        let r = self.row_mut(row);
        let c = (col.min(cols - 1)) as usize;
        &mut r.cells[c]
    }

    /// Scrolls rows `top..=bot` up by `n`, inserting blank rows at the
    /// bottom. When `save` is set, rows scrolled off the top are pushed to
    /// the scrollback buffer.
    pub(crate) fn scroll_up(&mut self, top: u16, bot: u16, n: u16, save: bool, style: Style) {
        let (top, bot) = (top as usize, (bot as usize).min(self.grid.len() - 1));
        if top > bot {
            return;
        }
        let n = (n as usize).min(bot - top + 1).max(1);
        for _ in 0..n {
            let row = self.grid.remove(top);
            if save && self.max_scrollback > 0 {
                if self.scrollback.len() >= self.max_scrollback {
                    self.scrollback.pop_front();
                }
                self.scrollback.push_back(row);
            }
            self.grid.insert(bot, Row::blank(self.cols as usize, style));
        }
    }

    /// Scrolls rows `top..=bot` down by `n`, inserting blank rows at the top.
    pub(crate) fn scroll_down(&mut self, top: u16, bot: u16, n: u16, style: Style) {
        let (top, bot) = (top as usize, (bot as usize).min(self.grid.len() - 1));
        if top > bot {
            return;
        }
        let n = (n as usize).min(bot - top + 1).max(1);
        for _ in 0..n {
            self.grid.remove(bot);
            self.grid.insert(top, Row::blank(self.cols as usize, style));
        }
    }

    pub(crate) fn clear_scrollback(&mut self) {
        self.scrollback.clear();
    }

    pub(crate) fn clear_grid(&mut self, style: Style) {
        for row in &mut self.grid {
            *row = Row::blank(self.cols as usize, style);
        }
    }

    /// Resizes the grid. With `reflow` set (primary screen), width changes
    /// rewrap logical lines kitty-style and the cursor follows its logical
    /// cell; otherwise each row truncates or pads (alt screen). Height
    /// changes exchange rows with the scrollback buffer when `reflow` is
    /// set, keeping the cursor row in view.
    pub(crate) fn resize(&mut self, rows: u16, cols: u16, cursor: &mut (u16, u16), reflow: bool) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if cols != self.cols {
            if reflow {
                self.reflow_width(cols, cursor);
            } else {
                for row in &mut self.grid {
                    row.resize_width(cols as usize);
                }
                for row in &mut self.scrollback {
                    row.resize_width(cols as usize);
                }
                self.cols = cols;
            }
            cursor.1 = cursor.1.min(cols - 1);
        }
        let target = rows as usize;
        while self.grid.len() > target {
            let cursor_at_end = (cursor.0 as usize) >= self.grid.len() - 1;
            let bottom_blank = self
                .grid
                .last()
                .map(|r| r.cells.iter().all(|c| c.is_dump_skippable()))
                .unwrap_or(true);
            if bottom_blank && !cursor_at_end {
                self.grid.pop();
            } else if reflow {
                let row = self.grid.remove(0);
                if self.max_scrollback > 0 {
                    if self.scrollback.len() >= self.max_scrollback {
                        self.scrollback.pop_front();
                    }
                    self.scrollback.push_back(row);
                }
                cursor.0 = cursor.0.saturating_sub(1);
            } else {
                self.grid.pop();
            }
        }
        while self.grid.len() < target {
            if reflow {
                if let Some(row) = self.scrollback.pop_back() {
                    self.grid.insert(0, row);
                    cursor.0 = cursor.0.saturating_add(1);
                    continue;
                }
            }
            self.grid.push(Row::blank(cols as usize, Style::default()));
        }
        self.rows = rows;
        cursor.0 = cursor.0.min(rows - 1);
    }

    /// Rewraps every logical line (scrollback included) to `new_cols`,
    /// keeping the cursor on the cell it occupied within its logical line.
    /// Wide characters never split: one that no longer fits at a line's end
    /// moves to the next row, leaving blank padding behind (kitty behavior).
    fn reflow_width(&mut self, new_cols: u16, cursor: &mut (u16, u16)) {
        let width = new_cols as usize;
        let cursor_abs = self.scrollback.len() + cursor.0 as usize;

        let mut old: Vec<Row> = self.scrollback.drain(..).collect();
        old.append(&mut self.grid);
        // Drop trailing all-blank rows (below the cursor) so padding at the
        // bottom of the grid does not push content into scrollback.
        while old.len() > cursor_abs + 1 {
            let last = old.last().unwrap();
            if last.cells.iter().all(|c| c.is_dump_skippable()) && !last.wrapped {
                old.pop();
            } else {
                break;
            }
        }

        let mut new_rows: Vec<Row> = Vec::new();
        let mut new_cursor: Option<(usize, u16)> = None;
        let mut i = 0;
        while i < old.len() {
            // Unwrap one logical line.
            let mut line: Vec<Cell> = Vec::new();
            let mut mark = None;
            let mut cursor_off: Option<usize> = None;
            loop {
                let row = &mut old[i];
                if mark.is_none() {
                    mark = row.mark;
                }
                if i == cursor_abs {
                    cursor_off = Some(line.len() + cursor.1 as usize);
                }
                let wrapped = row.wrapped;
                line.append(&mut row.cells);
                i += 1;
                if !wrapped || i == old.len() {
                    break;
                }
            }
            // Trim trailing blanks, but never past the cursor cell.
            let mut keep = line
                .iter()
                .rposition(|c| !c.is_dump_skippable())
                .map(|p| p + 1)
                .unwrap_or(0);
            if let Some(off) = cursor_off {
                keep = keep.max(off + 1);
            }
            line.truncate(keep);

            // Rewrap the logical line to the new width.
            let first_new = new_rows.len();
            let mut cur: Vec<Cell> = Vec::with_capacity(width);
            let mut j = 0;
            while j < line.len() {
                let cell = &line[j];
                if cell.width == 0 {
                    // Spacers are re-synthesized after their head.
                    if cursor_off == Some(j) && new_cursor.is_none() {
                        new_cursor = Some((new_rows.len(), cur.len().min(width - 1) as u16));
                    }
                    j += 1;
                    continue;
                }
                let w = cell.width.max(1) as usize;
                if w > width {
                    // A wide char cannot fit at any position: blank it.
                    if let Some(off) = cursor_off {
                        if off == j || off == j + 1 {
                            new_cursor = Some((new_rows.len(), cur.len() as u16));
                        }
                    }
                    let skip = if line.get(j + 1).map(|c| c.width == 0).unwrap_or(false) {
                        2
                    } else {
                        1
                    };
                    cur.push(Cell::blank(Style::default()));
                    j += skip;
                    if cur.len() == width && j < line.len() {
                        new_rows.push(Row {
                            cells: std::mem::take(&mut cur),
                            wrapped: true,
                            mark: None,
                        });
                    }
                    continue;
                }
                if cur.len() + w > width {
                    // Wide char at the edge: pad and move it to the next row.
                    while cur.len() < width {
                        cur.push(Cell::blank(Style::default()));
                    }
                    new_rows.push(Row {
                        cells: std::mem::take(&mut cur),
                        wrapped: true,
                        mark: None,
                    });
                }
                if let Some(off) = cursor_off {
                    if off == j || (w == 2 && off == j + 1) {
                        new_cursor = Some((new_rows.len(), (cur.len() + (off - j)) as u16));
                    }
                }
                let head = cell.clone();
                let spacer = Cell {
                    ch: '\0',
                    style: head.style,
                    width: 0,
                    extra: Vec::new(),
                    hyperlink: head.hyperlink,
                };
                cur.push(head);
                if w == 2 {
                    cur.push(spacer);
                    // Consume the original spacer when it survived trimming.
                    j += if line.get(j + 1).map(|c| c.width == 0).unwrap_or(false) {
                        2
                    } else {
                        1
                    };
                } else {
                    j += 1;
                }
                if cur.len() == width && j < line.len() {
                    new_rows.push(Row {
                        cells: std::mem::take(&mut cur),
                        wrapped: true,
                        mark: None,
                    });
                }
            }
            cur.resize(width, Cell::blank(Style::default()));
            new_rows.push(Row {
                cells: cur,
                wrapped: false,
                mark: None,
            });
            new_rows[first_new].mark = mark;
            if cursor_off.is_some() && new_cursor.is_none() {
                // Fallback: cursor cell vanished (orphan spacer); park it at
                // the end of the rewrapped line.
                let last = new_rows.len() - 1;
                let col = new_rows[last]
                    .cells
                    .iter()
                    .rposition(|c| !c.is_dump_skippable())
                    .map(|p| p + 1)
                    .unwrap_or(0)
                    .min(width - 1);
                new_cursor = Some((last, col as u16));
            }
        }

        let (cur_abs, cur_col) = new_cursor.unwrap_or((0, 0));
        let total = new_rows.len();
        let mut start = total.saturating_sub(self.rows as usize);
        if cur_abs < start {
            // Keep the cursor on screen even if rows below it overflow.
            start = cur_abs;
            new_rows.truncate(start + self.rows as usize);
        }
        let grid = new_rows.split_off(start);
        let mut scrollback: VecDeque<Row> = new_rows.into();
        while scrollback.len() > self.max_scrollback {
            scrollback.pop_front();
        }
        self.grid = grid;
        self.scrollback = scrollback;
        self.cols = new_cols;
        while self.grid.len() < self.rows as usize {
            self.grid.push(Row::blank(width, Style::default()));
        }
        *cursor = ((cur_abs - start) as u16, cur_col);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(s: &mut Screen, row: u16, text: &str) {
        for (i, ch) in text.chars().enumerate() {
            let cell = s.cell_mut(row, i as u16);
            cell.ch = ch;
            cell.width = 1;
        }
    }

    #[test]
    fn scroll_up_saves_to_scrollback() {
        let mut s = Screen::new(3, 10, 100);
        put(&mut s, 0, "one");
        put(&mut s, 1, "two");
        s.scroll_up(0, 2, 1, true, Style::default());
        assert_eq!(s.scrollback_len(), 1);
        assert_eq!(s.scrollback_row(0).unwrap().text(true), "one");
        assert_eq!(s.row(0).unwrap().text(true), "two");
        assert_eq!(s.row(2).unwrap().text(true), "");
    }

    #[test]
    fn scrollback_ring_evicts_oldest() {
        let mut s = Screen::new(2, 4, 2);
        for i in 0..4 {
            put(&mut s, 0, &i.to_string());
            s.scroll_up(0, 1, 1, true, Style::default());
        }
        assert_eq!(s.scrollback_len(), 2);
        assert_eq!(s.scrollback_row(0).unwrap().text(true), "2");
        assert_eq!(s.scrollback_row(1).unwrap().text(true), "3");
    }

    #[test]
    fn scroll_down_inserts_at_top() {
        let mut s = Screen::new(3, 10, 0);
        put(&mut s, 0, "one");
        put(&mut s, 1, "two");
        s.scroll_down(0, 2, 1, Style::default());
        assert_eq!(s.row(0).unwrap().text(true), "");
        assert_eq!(s.row(1).unwrap().text(true), "one");
        assert_eq!(s.row(2).unwrap().text(true), "two");
    }

    #[test]
    fn resize_narrower_reflows() {
        let mut s = Screen::new(2, 8, 10);
        put(&mut s, 0, "abcdefgh");
        let mut cur = (0u16, 0u16);
        s.resize(2, 4, &mut cur, true);
        assert_eq!(s.cols(), 4);
        assert_eq!(s.row(0).unwrap().text(true), "abcd");
        assert!(s.row(0).unwrap().wrapped());
        assert_eq!(s.row(1).unwrap().text(true), "efgh");
        assert!(!s.row(1).unwrap().wrapped());
    }

    #[test]
    fn resize_narrower_without_reflow_truncates() {
        let mut s = Screen::new(2, 8, 0);
        put(&mut s, 0, "abcdefgh");
        let mut cur = (0u16, 0u16);
        s.resize(2, 4, &mut cur, false);
        assert_eq!(s.row(0).unwrap().text(true), "abcd");
        assert!(!s.row(0).unwrap().wrapped());
    }

    #[test]
    fn resize_shorter_pushes_to_scrollback() {
        let mut s = Screen::new(4, 10, 10);
        put(&mut s, 0, "a");
        put(&mut s, 1, "b");
        put(&mut s, 2, "c");
        put(&mut s, 3, "d");
        let mut cur = (3u16, 0u16);
        s.resize(2, 10, &mut cur, true);
        assert_eq!(s.scrollback_len(), 2);
        assert_eq!(s.row(0).unwrap().text(true), "c");
        assert_eq!(s.row(1).unwrap().text(true), "d");
        assert_eq!(cur.0, 1);
    }

    #[test]
    fn resize_taller_pulls_from_scrollback() {
        let mut s = Screen::new(2, 10, 10);
        put(&mut s, 0, "x");
        s.scroll_up(0, 1, 1, true, Style::default());
        let mut cur = (1u16, 0u16);
        s.resize(3, 10, &mut cur, true);
        assert_eq!(s.scrollback_len(), 0);
        assert_eq!(s.row(0).unwrap().text(true), "x");
        assert_eq!(cur.0, 2);
    }

    #[test]
    fn resize_shrink_trims_blank_bottom_first() {
        let mut s = Screen::new(4, 10, 10);
        put(&mut s, 0, "a");
        let mut cur = (0u16, 0u16);
        s.resize(2, 10, &mut cur, true);
        // Blank bottom rows were trimmed; nothing went to scrollback.
        assert_eq!(s.scrollback_len(), 0);
        assert_eq!(s.row(0).unwrap().text(true), "a");
        assert_eq!(cur.0, 0);
    }

    #[test]
    fn width_truncation_blanks_cut_wide_char() {
        let mut s = Screen::new(1, 4, 0);
        {
            let c = s.cell_mut(0, 2);
            c.ch = '中';
            c.width = 2;
        }
        {
            let c = s.cell_mut(0, 3);
            c.ch = '\0';
            c.width = 0;
        }
        let mut cur = (0u16, 0u16);
        s.resize(1, 3, &mut cur, false);
        assert!(s.cell(0, 2).unwrap().is_blank());
        assert_eq!(s.cell(0, 2).unwrap().width, 1);
    }

    #[test]
    fn reflow_widen_rejoins_wrapped_lines() {
        let mut s = Screen::new(3, 4, 10);
        put(&mut s, 0, "abcd");
        s.row_mut(0).wrapped = true;
        put(&mut s, 1, "ef");
        let mut cur = (1u16, 2u16);
        s.resize(3, 10, &mut cur, true);
        assert_eq!(s.row(0).unwrap().text(true), "abcdef");
        assert!(!s.row(0).unwrap().wrapped());
        assert_eq!(cur, (0, 6));
    }

    #[test]
    fn reflow_cursor_follows_logical_cell() {
        let mut s = Screen::new(3, 8, 10);
        put(&mut s, 0, "abcdefgh");
        s.row_mut(0).wrapped = true;
        put(&mut s, 1, "ij");
        // Cursor on 'g' (logical offset 6).
        let mut cur = (0u16, 6u16);
        s.resize(3, 4, &mut cur, true);
        assert_eq!(s.row(1).unwrap().text(true), "efgh");
        assert_eq!(cur, (1, 2));
    }

    #[test]
    fn reflow_moves_unfit_wide_char_to_next_row() {
        let mut s = Screen::new(2, 6, 10);
        put(&mut s, 0, "abcd");
        {
            let c = s.cell_mut(0, 4);
            c.ch = '中';
            c.width = 2;
        }
        {
            let c = s.cell_mut(0, 5);
            c.ch = '\0';
            c.width = 0;
        }
        let mut cur = (0u16, 0u16);
        s.resize(2, 5, &mut cur, true);
        // The wide char cannot straddle column 5: it moves down whole.
        assert_eq!(s.row(0).unwrap().text(true), "abcd");
        assert!(s.row(0).unwrap().wrapped());
        assert_eq!(s.cell(1, 0).unwrap().ch, '中');
        assert_eq!(s.cell(1, 0).unwrap().width, 2);
    }

    #[test]
    fn reflow_scrollback_rewraps_too() {
        let mut s = Screen::new(2, 8, 10);
        put(&mut s, 0, "12345678");
        s.scroll_up(0, 1, 1, true, Style::default());
        let mut cur = (0u16, 0u16);
        s.resize(2, 4, &mut cur, true);
        let joined: Vec<String> = (0..s.scrollback_len())
            .map(|i| s.scrollback_row(i).unwrap().text(true))
            .collect();
        assert!(joined.concat().contains("1234"), "{joined:?}");
    }
}
