//! Placeholder implementation of the frozen API. Replaced by the real
//! emulator; exists so dependents can compile against the contract.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: UnderlineStyle,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
    pub width: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
    pub shape: CursorShape,
}

#[derive(Debug, Default)]
pub struct Screen {
    rows: u16,
    cols: u16,
    cells: Vec<Cell>,
}

impl Screen {
    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        self.cells.get(row as usize * self.cols as usize + col as usize)
    }
}

#[derive(Debug)]
pub struct Terminal {
    screen: Screen,
    title: String,
    cursor: Cursor,
    generation: u64,
    responses: Vec<u8>,
}

impl Terminal {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self::with_scrollback(rows, cols, 10_000)
    }

    pub fn with_scrollback(rows: u16, cols: u16, _scrollback: usize) -> Self {
        Terminal {
            screen: Screen {
                rows,
                cols,
                cells: vec![Cell::default(); rows as usize * cols as usize],
            },
            title: String::new(),
            cursor: Cursor { visible: true, ..Cursor::default() },
            generation: 0,
            responses: Vec::new(),
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.generation += 1;
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.screen.rows = rows;
        self.screen.cols = cols;
        self.screen.cells = vec![Cell::default(); rows as usize * cols as usize];
        self.generation += 1;
    }

    pub fn rows(&self) -> u16 {
        self.screen.rows
    }

    pub fn cols(&self) -> u16 {
        self.screen.cols
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn dump_vt(&self) -> Vec<u8> {
        Vec::new()
    }

    pub fn dump_text(&self) -> String {
        String::new()
    }

    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }
}
