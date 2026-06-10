//! Cell, style, and color types for the screen model.

/// A color as stored in a [`Style`]: terminal default, one of the 256
/// indexed palette slots, or a direct RGB value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Underline rendering style (SGR 4 and its colon subparameters).
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

/// The full set of SGR attributes a cell can carry, plus the DECSCA
/// protection guard (kept with the pen attributes although SGR sequences,
/// including SGR 0, never change it).
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
    /// DECSCA (CSI " q): protected from DECSED/DECSEL selective erase.
    pub protected: bool,
}

impl Style {
    pub fn is_default(&self) -> bool {
        *self == Style::default()
    }
}

/// One grid cell. `width` is 1 for normal cells, 2 for the head of a wide
/// (double-column) character, and 0 for the spacer cell that follows a wide
/// head. `extra` holds combining marks attached to `ch`. `hyperlink` is an
/// OSC 8 hyperlink id (0 = none), resolvable via `Terminal::hyperlink`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
    pub width: u8,
    pub extra: Vec<char>,
    pub hyperlink: u32,
}

impl Cell {
    /// A space cell carrying `style` (the erased/cleared cell shape).
    pub fn blank(style: Style) -> Cell {
        Cell {
            ch: ' ',
            style,
            width: 1,
            extra: Vec::new(),
            hyperlink: 0,
        }
    }

    /// True if the cell shows nothing (space or empty) with no combining marks.
    pub fn is_blank(&self) -> bool {
        (self.ch == ' ' || self.ch == '\0') && self.extra.is_empty()
    }

    /// Blank with default style and no hyperlink: can be skipped when
    /// serializing over a freshly-cleared background.
    pub(crate) fn is_dump_skippable(&self) -> bool {
        self.is_blank() && self.style.is_default() && self.hyperlink == 0
    }
}

/// Default xterm 256-color palette entry.
pub(crate) fn default_palette_entry(i: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match i {
        0..=15 => BASE[i as usize],
        16..=231 => {
            let v = i - 16;
            let scale = |x: u8| if x == 0 { 0 } else { 55 + 40 * x };
            (scale(v / 36), scale((v / 6) % 6), scale(v % 6))
        }
        _ => {
            let g = 8 + 10 * (i - 232);
            (g, g, g)
        }
    }
}

pub(crate) fn default_palette() -> [(u8, u8, u8); 256] {
    let mut p = [(0u8, 0u8, 0u8); 256];
    for (i, slot) in p.iter_mut().enumerate() {
        *slot = default_palette_entry(i as u8);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_defaults() {
        assert_eq!(default_palette_entry(1), (205, 0, 0));
        assert_eq!(default_palette_entry(16), (0, 0, 0));
        assert_eq!(default_palette_entry(196), (255, 0, 0));
        assert_eq!(default_palette_entry(232), (8, 8, 8));
        assert_eq!(default_palette_entry(255), (238, 238, 238));
    }

    #[test]
    fn blank_cell() {
        let c = Cell::blank(Style::default());
        assert!(c.is_blank());
        assert!(c.is_dump_skippable());
        let mut styled = c.clone();
        styled.style.bold = true;
        assert!(!styled.is_dump_skippable());
    }
}
