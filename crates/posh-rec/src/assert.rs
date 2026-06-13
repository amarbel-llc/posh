//! Screen assertion helpers, typed on `posh_term`'s `Screen`/`Row`/`Cell`,
//! with a colored expected-vs-actual mismatch render.
//!
//! Use them in `#[test]`s over a [`crate::player::Player`]'s terminal:
//! `cells_have_fg(player.terminal().screen(), row, cols, Color::Indexed(2))?`.
//! On failure the [`Mismatch`] prints the affected row twice — actual and
//! expected — with 24-bit SGR, so a wrong color shows up *as* the wrong color.

use posh_term::{Cell, Color, Screen, Style, UnderlineStyle};

/// The first visible-grid row whose trimmed text contains `substr`.
pub fn find_line(screen: &Screen, substr: &str) -> Option<u16> {
    (0..screen.rows()).find(|&r| {
        screen
            .row(r)
            .is_some_and(|row| row.text(true).contains(substr))
    })
}

/// Assert every cell in `cols` of `row` has foreground `want`.
pub fn cells_have_fg(
    screen: &Screen,
    row: u16,
    cols: impl IntoIterator<Item = u16>,
    want: Color,
) -> Result<(), Mismatch> {
    check(screen, row, cols, Want::Fg(want))
}

/// Assert every cell in `cols` of `row` has background `want`.
pub fn cells_have_bg(
    screen: &Screen,
    row: u16,
    cols: impl IntoIterator<Item = u16>,
    want: Color,
) -> Result<(), Mismatch> {
    check(screen, row, cols, Want::Bg(want))
}

/// Assert the bold attribute over `cols` of `row`.
pub fn cells_are_bold(
    screen: &Screen,
    row: u16,
    cols: impl IntoIterator<Item = u16>,
    want: bool,
) -> Result<(), Mismatch> {
    check(screen, row, cols, Want::Bold(want))
}

/// Assert the dim attribute over `cols` of `row`.
pub fn cells_are_dim(
    screen: &Screen,
    row: u16,
    cols: impl IntoIterator<Item = u16>,
    want: bool,
) -> Result<(), Mismatch> {
    check(screen, row, cols, Want::Dim(want))
}

/// Assert the inverse attribute over `cols` of `row`.
pub fn cells_are_inverse(
    screen: &Screen,
    row: u16,
    cols: impl IntoIterator<Item = u16>,
    want: bool,
) -> Result<(), Mismatch> {
    check(screen, row, cols, Want::Inverse(want))
}

/// Assert the underline attribute (any style) over `cols` of `row`.
pub fn cells_are_underline(
    screen: &Screen,
    row: u16,
    cols: impl IntoIterator<Item = u16>,
    want: bool,
) -> Result<(), Mismatch> {
    check(screen, row, cols, Want::Underline(want))
}

/// The asserted property and its wanted value.
#[derive(Debug, Clone, Copy)]
enum Want {
    Fg(Color),
    Bg(Color),
    Bold(bool),
    Dim(bool),
    Inverse(bool),
    Underline(bool),
}

impl Want {
    fn matches(&self, cell: &Cell) -> bool {
        let s = &cell.style;
        match self {
            Want::Fg(c) => s.fg == *c,
            Want::Bg(c) => s.bg == *c,
            Want::Bold(b) => s.bold == *b,
            Want::Dim(b) => s.dim == *b,
            Want::Inverse(b) => s.inverse == *b,
            Want::Underline(b) => (s.underline != UnderlineStyle::None) == *b,
        }
    }

    fn apply(&self, style: &mut Style) {
        match self {
            Want::Fg(c) => style.fg = *c,
            Want::Bg(c) => style.bg = *c,
            Want::Bold(b) => style.bold = *b,
            Want::Dim(b) => style.dim = *b,
            Want::Inverse(b) => style.inverse = *b,
            Want::Underline(b) => {
                style.underline = if *b {
                    UnderlineStyle::Single
                } else {
                    UnderlineStyle::None
                }
            }
        }
    }
}

fn check(
    screen: &Screen,
    row: u16,
    cols: impl IntoIterator<Item = u16>,
    want: Want,
) -> Result<(), Mismatch> {
    let cols: Vec<u16> = cols.into_iter().collect();
    let Some(r) = screen.row(row) else {
        return Err(Mismatch {
            message: format!("row {row} is out of range (screen has {} rows)", screen.rows()),
        });
    };
    let cells = r.cells();
    let bad: Vec<u16> = cols
        .iter()
        .copied()
        .filter(|&c| !cells.get(c as usize).is_some_and(|cell| want.matches(cell)))
        .collect();
    if bad.is_empty() {
        return Ok(());
    }
    Err(Mismatch {
        message: build_message(row, cells, &cols, &want, &bad),
    })
}

/// A failed screen assertion. Its `Debug`/`Display` (the latter shown when a
/// test `.unwrap()`s the `Result`) renders the row in real terminal color.
pub struct Mismatch {
    message: String,
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::fmt::Debug for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Debug is what `Result::unwrap` prints; show the colored render, not a
        // struct dump.
        f.write_str("\n")?;
        f.write_str(&self.message)
    }
}

fn build_message(row: u16, cells: &[Cell], cols: &[u16], want: &Want, bad: &[u16]) -> String {
    let mut m = format!("row {row}: expected {want:?} at cols {cols:?}; mismatched at {bad:?}\n");
    m.push_str("  actual:   ");
    m.push_str(&render_row(cells, None));
    m.push_str("\n  expected: ");
    m.push_str(&render_row(cells, Some((cols, want))));
    m.push('\n');
    m
}

/// Render a row of cells with 24-bit SGR (shared with the golden colored view).
pub(crate) fn render_cells(cells: &[Cell]) -> String {
    render_row(cells, None)
}

/// Render a row with 24-bit SGR, re-emitting the pen only on style changes. If
/// `over` is set, the listed cols are restyled to the wanted value (the
/// "expected" view).
fn render_row(cells: &[Cell], over: Option<(&[u16], &Want)>) -> String {
    let mut out = String::new();
    let mut pen: Option<Style> = None;
    for (i, cell) in cells.iter().enumerate() {
        if cell.width == 0 {
            continue;
        }
        let mut style = cell.style;
        if let Some((cols, want)) = over {
            if cols.contains(&(i as u16)) {
                want.apply(&mut style);
            }
        }
        if pen != Some(style) {
            out.push_str(&sgr(&style));
            pen = Some(style);
        }
        out.push(if cell.ch == '\0' { ' ' } else { cell.ch });
    }
    out.push_str("\x1b[0m");
    out
}

/// The SGR run that establishes `style` from a reset baseline.
fn sgr(style: &Style) -> String {
    let mut s = String::from("\x1b[0m");
    if let Some((r, g, b)) = style.fg.to_rgb() {
        s.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
    }
    if let Some((r, g, b)) = style.bg.to_rgb() {
        s.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
    }
    if style.bold {
        s.push_str("\x1b[1m");
    }
    if style.dim {
        s.push_str("\x1b[2m");
    }
    if style.italic {
        s.push_str("\x1b[3m");
    }
    if style.underline != UnderlineStyle::None {
        s.push_str("\x1b[4m");
    }
    if style.blink {
        s.push_str("\x1b[5m");
    }
    if style.inverse {
        s.push_str("\x1b[7m");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Replay;

    fn screen_with(bytes: &[u8]) -> Replay {
        let mut r = Replay::new(3, 20);
        r.feed(bytes);
        r
    }

    #[test]
    fn find_line_locates_substring() {
        let r = screen_with(b"alpha\r\nbeta");
        assert_eq!(find_line(r.screen(), "beta"), Some(1));
        assert_eq!(find_line(r.screen(), "nope"), None);
    }

    #[test]
    fn cells_have_fg_passes_on_matching_run() {
        // "ok" in SGR red (31 -> indexed 1).
        let r = screen_with(b"\x1b[31mok\x1b[0m");
        assert!(cells_have_fg(r.screen(), 0, 0..2, Color::Indexed(1)).is_ok());
    }

    #[test]
    fn cells_have_fg_fails_with_colored_render() {
        let r = screen_with(b"\x1b[31mok\x1b[0m");
        let err = cells_have_fg(r.screen(), 0, 0..2, Color::Indexed(2))
            .expect_err("red is not green");
        let msg = err.to_string();
        // The mismatch carries SGR escapes (the colored render) and both views.
        assert!(msg.contains('\x1b'), "{msg:?}");
        assert!(msg.contains("actual:") && msg.contains("expected:"), "{msg:?}");
    }

    #[test]
    fn cells_are_bold_checks_attribute() {
        let r = screen_with(b"\x1b[1mB\x1b[0mn");
        assert!(cells_are_bold(r.screen(), 0, [0], true).is_ok());
        assert!(cells_are_bold(r.screen(), 0, [1], true).is_err());
        assert!(cells_are_bold(r.screen(), 0, [1], false).is_ok());
    }
}
