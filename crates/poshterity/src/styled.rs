//! Style-aware screen conditions for `poshterity seek --styled`: "this text is
//! on the screen AND drawn with these attributes" (a reverse-video bar, a red
//! error, a dimmed backdrop), which a plain text dump cannot say.

use posh_term::{Cell, Color, Terminal, UnderlineStyle};

/// One cell attribute a [`StyledText`] requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attr {
    Bold,
    Dim,
    Italic,
    Underline,
    Inverse,
    Blink,
    Strike,
    Fg(Color),
    Bg(Color),
}

impl Attr {
    /// `bold`, `dim`, `italic`, `underline`, `inverse` (alias `reverse`),
    /// `blink`, `strike`, `fg:N`, `bg:N` (a 0-255 palette index) or
    /// `fg:default` / `bg:default`.
    pub fn parse(s: &str) -> Result<Attr, String> {
        let color = |v: &str| -> Result<Color, String> {
            if v == "default" {
                return Ok(Color::Default);
            }
            v.parse::<u8>()
                .map(Color::Indexed)
                .map_err(|_| format!("a colour is 0-255 or `default`, got {v:?}"))
        };
        match s {
            "bold" => Ok(Attr::Bold),
            "dim" => Ok(Attr::Dim),
            "italic" => Ok(Attr::Italic),
            "underline" => Ok(Attr::Underline),
            "inverse" | "reverse" => Ok(Attr::Inverse),
            "blink" => Ok(Attr::Blink),
            "strike" => Ok(Attr::Strike),
            other => match other.split_once(':') {
                Some(("fg", v)) => Ok(Attr::Fg(color(v)?)),
                Some(("bg", v)) => Ok(Attr::Bg(color(v)?)),
                _ => Err(format!(
                    "unknown attribute {other:?} (bold|dim|italic|underline|inverse|blink|strike|fg:N|bg:N)"
                )),
            },
        }
    }

    fn holds(self, cell: &Cell) -> bool {
        let s = &cell.style;
        match self {
            Attr::Bold => s.bold,
            Attr::Dim => s.dim,
            Attr::Italic => s.italic,
            Attr::Underline => s.underline != UnderlineStyle::None,
            Attr::Inverse => s.inverse,
            Attr::Blink => s.blink,
            Attr::Strike => s.strikethrough,
            Attr::Fg(c) => s.fg == c,
            Attr::Bg(c) => s.bg == c,
        }
    }
}

/// `TEXT` must appear on some row with every cell it covers carrying every
/// attribute in `attrs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledText {
    pub text: String,
    pub attrs: Vec<Attr>,
}

impl StyledText {
    /// Parse `TEXT=attr[,attr]...` (split at the LAST `=`, so TEXT may hold one).
    pub fn parse(spec: &str) -> Result<StyledText, String> {
        let (text, attrs) = spec
            .rsplit_once('=')
            .ok_or_else(|| format!("--styled expects TEXT=attr[,attr]..., got {spec:?}"))?;
        if text.is_empty() {
            return Err(format!("--styled needs some TEXT before `=`, got {spec:?}"));
        }
        let attrs = attrs.split(',').map(Attr::parse).collect::<Result<Vec<_>, _>>()?;
        Ok(StyledText { text: text.to_string(), attrs })
    }

    /// Whether the terminal's visible screen satisfies this condition.
    pub fn holds(&self, term: &Terminal) -> bool {
        let want: Vec<char> = self.text.chars().collect();
        let screen = term.screen();
        (0..screen.rows()).any(|r| {
            let Some(row) = screen.row(r) else { return false };
            // The row as (char, cell) pairs, skipping wide-char spacer cells,
            // so a text match maps back to the cells that drew it.
            let cells: Vec<(char, &Cell)> = row
                .cells()
                .iter()
                .filter(|c| c.width != 0)
                .map(|c| (if c.ch == '\0' { ' ' } else { c.ch }, c))
                .collect();
            cells.windows(want.len().max(1)).any(|w| {
                w.iter().map(|(ch, _)| *ch).eq(want.iter().copied())
                    && w.iter().all(|(_, cell)| self.attrs.iter().all(|a| a.holds(cell)))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Replay;

    fn term(bytes: &[u8]) -> Replay {
        let mut r = Replay::new(4, 30);
        r.feed(bytes);
        r
    }

    #[test]
    fn parses_attributes_and_splits_at_the_last_equals() {
        let s = StyledText::parse("a=b text=bold,fg:1,reverse").unwrap();
        assert_eq!(s.text, "a=b text");
        assert_eq!(s.attrs, vec![Attr::Bold, Attr::Fg(Color::Indexed(1)), Attr::Inverse]);
        assert_eq!(Attr::parse("bg:default"), Ok(Attr::Bg(Color::Default)));
    }

    #[test]
    fn rejects_bad_specs() {
        for bad in ["nospec", "=bold", "x=sparkly", "x=fg:999", "x=fg:red", "x="] {
            assert!(StyledText::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_reverse_video_bar_is_inverse_and_plain_text_is_not() {
        let r = term(b"\x1b[7m banner \x1b[0m\r\nplain");
        let yes = StyledText::parse("banner=inverse").unwrap();
        let no = StyledText::parse("plain=inverse").unwrap();
        assert!(yes.holds(r.terminal()));
        assert!(!no.holds(r.terminal()));
    }

    #[test]
    fn every_covered_cell_must_carry_every_attribute() {
        // "re" is bold, "d" is not: "red" is not wholly bold, "re" is.
        let r = term(b"\x1b[1mre\x1b[0md");
        assert!(StyledText::parse("re=bold").unwrap().holds(r.terminal()));
        assert!(!StyledText::parse("red=bold").unwrap().holds(r.terminal()));
        // Colours: red text is fg:1, and not fg:2.
        let r = term(b"\x1b[31mred\x1b[0m");
        assert!(StyledText::parse("red=fg:1").unwrap().holds(r.terminal()));
        assert!(!StyledText::parse("red=fg:2").unwrap().holds(r.terminal()));
        assert!(StyledText::parse("red=fg:1,bg:default").unwrap().holds(r.terminal()));
    }

    #[test]
    fn text_that_is_not_on_screen_never_holds() {
        let r = term(b"hello");
        assert!(!StyledText::parse("nope=bold").unwrap().holds(r.terminal()));
        assert!(StyledText::parse("hello=fg:default").unwrap().holds(r.terminal()));
    }
}
