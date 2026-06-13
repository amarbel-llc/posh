//! Golden-frame rendering: a deterministic string snapshot of a screen — the
//! deterministic analog of `tmux capture-pane`. `bless` writes it; `assert`
//! re-renders and compares (so there is no golden *parser* to keep in sync).
//!
//! The default `grid` golden is diff-friendly: a plain-text block (so `git
//! diff` shows human-readable content change) plus a per-cell style sidecar
//! that lists only non-default style runs (so color/attr regressions surface
//! as readable line diffs). `vt`/`flat` goldens are the exact-but-opaque
//! `dump_vt`/`dump_vt_flat` escape stream.

use posh_term::{Cell, Color, Style, Terminal, UnderlineStyle};

/// Which golden representation to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoldenKind {
    /// Human-diffable text rows + a non-default style sidecar (default).
    Grid,
    /// Full `dump_vt()` reconstruction stream.
    Vt,
    /// Single-screen `dump_vt_flat()` stream.
    Flat,
}

impl GoldenKind {
    pub fn parse(s: &str) -> Result<GoldenKind, String> {
        match s {
            "grid" => Ok(GoldenKind::Grid),
            "vt" => Ok(GoldenKind::Vt),
            "flat" => Ok(GoldenKind::Flat),
            other => Err(format!("--kind expects grid|vt|flat, got {other:?}")),
        }
    }
}

/// Render a golden of the terminal's current screen. `emu_rev` is stamped into
/// the `grid` header so a later `assert --check-emu-rev` can warn on drift.
pub fn render(term: &Terminal, kind: GoldenKind, emu_rev: &str) -> String {
    match kind {
        GoldenKind::Grid => render_grid(term, emu_rev),
        GoldenKind::Vt => String::from_utf8_lossy(&term.dump_vt()).into_owned(),
        GoldenKind::Flat => String::from_utf8_lossy(&term.dump_vt_flat()).into_owned(),
    }
}

/// Read `emu_rev` from a previously-written `grid` golden, if present.
pub fn golden_emu_rev(golden: &str) -> Option<&str> {
    golden
        .lines()
        .find_map(|l| l.strip_prefix("# emu_rev: "))
        .map(str::trim)
}

fn render_grid(term: &Terminal, emu_rev: &str) -> String {
    let scr = term.screen();
    let mut s = String::new();
    s.push_str("# posh-rec grid golden v1\n");
    s.push_str(&format!("# emu_rev: {emu_rev}\n"));
    s.push_str(&format!("# size: {}x{}\n", scr.cols(), scr.rows()));

    s.push_str("--- text ---\n");
    for r in 0..scr.rows() {
        if let Some(row) = scr.row(r) {
            s.push_str(&row.text(false));
        }
        s.push('\n');
    }

    s.push_str("--- style ---\n");
    for r in 0..scr.rows() {
        if let Some(row) = scr.row(r) {
            for (start, len, desc) in style_runs(row.cells()) {
                s.push_str(&format!("{r} {start}:{len} {desc}\n"));
            }
        }
    }
    s
}

/// Maximal runs of cells sharing a non-default style: `(start_col, len, desc)`.
fn style_runs(cells: &[Cell]) -> Vec<(usize, usize, String)> {
    let mut runs = Vec::new();
    let mut i = 0;
    while i < cells.len() {
        let style = cells[i].style;
        if style.is_default() {
            i += 1;
            continue;
        }
        let start = i;
        while i < cells.len() && cells[i].style == style {
            i += 1;
        }
        runs.push((start, i - start, style_desc(&style)));
    }
    runs
}

fn style_desc(s: &Style) -> String {
    let mut parts = vec![
        format!("fg={}", color_str(s.fg)),
        format!("bg={}", color_str(s.bg)),
    ];
    let mut flags = Vec::new();
    if s.bold {
        flags.push("bold");
    }
    if s.dim {
        flags.push("dim");
    }
    if s.italic {
        flags.push("italic");
    }
    if s.underline != UnderlineStyle::None {
        flags.push("underline");
    }
    if s.blink {
        flags.push("blink");
    }
    if s.inverse {
        flags.push("inverse");
    }
    if s.strikethrough {
        flags.push("strike");
    }
    if !flags.is_empty() {
        parts.push(flags.join(","));
    }
    parts.join(" ")
}

fn color_str(c: Color) -> String {
    match c {
        Color::Default => "def".to_string(),
        Color::Indexed(n) => format!("i{n}"),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

/// A positional line diff of two goldens (same screen size ⇒ aligned), plus,
/// for `grid`, a colored render of the actual screen so a color regression is
/// visible as color.
pub fn diff(golden: &str, fresh: &str, term: &Terminal, kind: GoldenKind) -> String {
    let mut out = String::new();
    let g: Vec<&str> = golden.lines().collect();
    let f: Vec<&str> = fresh.lines().collect();
    for i in 0..g.len().max(f.len()) {
        let (gl, fl) = (g.get(i).copied().unwrap_or(""), f.get(i).copied().unwrap_or(""));
        if gl != fl {
            out.push_str(&format!("- {gl}\n+ {fl}\n"));
        }
    }
    if kind == GoldenKind::Grid {
        out.push_str("\nactual screen:\n");
        let scr = term.screen();
        for r in 0..scr.rows() {
            if let Some(row) = scr.row(r) {
                out.push_str(&crate::assert::render_cells(row.cells()));
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Replay;

    fn term_with(bytes: &[u8]) -> Replay {
        let mut r = Replay::new(2, 12);
        r.feed(bytes);
        r
    }

    #[test]
    fn grid_render_is_deterministic() {
        let r = term_with(b"\x1b[31mred\x1b[0m hi");
        let a = render(r.terminal(), GoldenKind::Grid, "0.1.0");
        let b = render(r.terminal(), GoldenKind::Grid, "0.1.0");
        assert_eq!(a, b);
    }

    #[test]
    fn grid_golden_carries_text_and_style_run() {
        let r = term_with(b"\x1b[31mred\x1b[0m hi");
        let g = render(r.terminal(), GoldenKind::Grid, "0.1.0");
        assert!(g.contains("red hi"), "{g}"); // text block
        assert!(g.contains("0:3 fg=i1"), "{g}"); // "red" is fg indexed 1
        assert_eq!(golden_emu_rev(&g), Some("0.1.0"));
    }

    #[test]
    fn grid_golden_changes_when_color_changes() {
        let red = render(term_with(b"\x1b[31mX").terminal(), GoldenKind::Grid, "x");
        let green = render(term_with(b"\x1b[32mX").terminal(), GoldenKind::Grid, "x");
        assert_ne!(red, green);
        assert!(!diff(&red, &green, term_with(b"\x1b[32mX").terminal(), GoldenKind::Grid).is_empty());
    }
}
