//! poshterity: a deterministic, step-ratcheted terminal recorder/replayer
//! built on the [`posh_term`] emulator.
//!
//! The problem it solves: terminal-emulation tests are flaky when they
//! drive a *live* terminal and race a screen capture against
//! non-deterministic redraw (the classic `tmux capture-pane` + `sleep`
//! pattern). poshterity replays a recorded output byte stream through an
//! *in-process* [`posh_term::Terminal`] and lets callers inspect exact
//! screen state — the screen is a pure function of the bytes fed, so there
//! is no timing to race.
//!
//! This is Phase 0 (tracer bullet): the minimal replay seam plus a
//! deterministic test proving the core in CI. The recording file format
//! (`.castx`, an asciinema `.cast` v2 superset), the step-ratchet `Player`
//! with its multiple granularities, the assertion/golden surface, and the
//! `poshterity` CLI all build on this seam in later phases. See
//! `docs/features/` for the feature record once it lands.
#![forbid(unsafe_code)]

pub mod assert;
pub mod castx;
pub mod cli;
pub mod golden;
pub mod json;
pub mod player;

// Re-exported so callers of the assertion helpers (which take/return these
// posh-term types) don't need a separate posh-term dependency.
pub use posh_term::{Color, Screen};

use posh_term::Terminal;

/// A deterministic replay of a terminal output byte stream through an
/// in-process emulator.
///
/// Phase 0 exposes only "feed the whole stream, then read the screen". The
/// step-ratchet (advance by byte / escape sequence / visible change /
/// recorded frame / named marker) layers onto this same owned [`Terminal`]
/// in a later phase, driven by the emulator signals posh-term already
/// exposes (`generation()`, `mid_escape()`).
pub struct Replay {
    term: Terminal,
}

impl Replay {
    /// Create a replay with a terminal of the given size.
    pub fn new(rows: u16, cols: u16) -> Replay {
        Replay {
            term: Terminal::new(rows, cols),
        }
    }

    /// Create a replay whose terminal retains `scrollback` lines.
    pub fn with_scrollback(rows: u16, cols: u16, scrollback: usize) -> Replay {
        Replay {
            term: Terminal::with_scrollback(rows, cols, scrollback),
        }
    }

    /// Feed a chunk of recorded output bytes into the emulator. Query
    /// replies the emulator wants to send back (DA/DSR/kitty) are drained
    /// and discarded: a replay is one-directional output, but draining
    /// keeps the emulator's internal state consistent with a live session.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.term.process(bytes);
        let _ = self.term.take_responses();
    }

    /// Resize the emulated terminal. Honors `.castx` `r` (resize) events on
    /// replay. Argument order matches [`Terminal::resize`] (rows, then cols);
    /// the caller is responsible for the asciinema `"WxH"` cols-first→rows
    /// mapping (see [`cli`]).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.term.resize(rows, cols);
    }

    /// Read access to the emulated screen (cells, rows, scrollback).
    pub fn screen(&self) -> &Screen {
        self.term.screen()
    }

    /// Read access to the full terminal (cursor, title, dump_vt, ...).
    pub fn terminal(&self) -> &Terminal {
        &self.term
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use posh_term::Color;

    /// Plain text of row `r`, trailing whitespace trimmed.
    fn row_text(replay: &Replay, r: u16) -> String {
        replay.screen().row(r).unwrap().text(true)
    }

    /// The tracer bullet: a fixed output stream replayed through the
    /// in-process emulator yields an exact, deterministic screen — no live
    /// terminal, no `capture-pane`, no timing. This is the property the
    /// whole tool is built to provide; if it ever flakes, the premise is
    /// broken.
    #[test]
    fn fixed_stream_replays_to_deterministic_screen() {
        let mut replay = Replay::new(5, 20);
        // "hello " in default color, "red" in SGR red (31), reset, then a
        // newline and a second line. Cursor moves are implicit in the
        // newline + carriage return.
        replay.feed(b"hello \x1b[31mred\x1b[0m\r\nsecond line");

        // Exact text content of the first two rows.
        assert_eq!(row_text(&replay, 0), "hello red");
        assert_eq!(row_text(&replay, 1), "second line");

        // The "red" run carries SGR red as an indexed color (palette slot
        // 1). "hello " before it is default. Inspect the cells directly.
        let row0 = replay.screen().row(0).unwrap();
        let cells = row0.cells();
        // "hello " == 6 cells of default fg.
        for cell in &cells[0..6] {
            assert_eq!(cell.style.fg, Color::Default, "{:?}", cell.ch);
        }
        // "red" == 3 cells of indexed red (SGR 31 -> palette index 1).
        for cell in &cells[6..9] {
            assert_eq!(cell.style.fg, Color::Indexed(1), "{:?}", cell.ch);
        }
    }

    /// Replaying the same bytes twice yields byte-identical screen dumps —
    /// the determinism guarantee stated as an invariant.
    #[test]
    fn replay_is_repeatable() {
        let bytes = b"\x1b[1mbold\x1b[0m\r\n\x1b[2Jcleared";
        let mut a = Replay::new(4, 12);
        let mut b = Replay::new(4, 12);
        a.feed(bytes);
        b.feed(bytes);
        assert_eq!(a.terminal().dump_vt(), b.terminal().dump_vt());
    }
}
