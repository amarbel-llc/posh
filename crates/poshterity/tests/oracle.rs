//! The test-oracle scenario the phase exists for: step to a named marker, then
//! assert content and color at that frame via the helpers — no live terminal.

use poshterity::assert::{cells_have_fg, cells_have_bg, find_line};
use poshterity::player::Player;
use poshterity::Color;

// "ready " (cols 0-5) then green "GO" (cols 6-7) in SGR 32 = indexed 2, a
// marker, then " done" which must NOT be on screen at the marker.
const DOC: &str = "{\"version\":2,\"width\":20,\"height\":3,\"poshterity\":{\"v\":1,\"emu_rev\":\"0.1.0\"}}\n\
                   [0.0,\"o\",\"ready \"]\n\
                   [0.1,\"o\",\"\\u001b[32mGO\\u001b[0m\"]\n\
                   [0.2,\"m\",\"go-shown\"]\n\
                   [0.3,\"o\",\" done\"]\n";

#[test]
fn assert_text_and_color_at_a_marker() {
    let mut p = Player::from_source(DOC).unwrap();
    assert!(p.step_to_marker("go-shown"));
    let scr = p.terminal().screen();

    // Content: row 0 says "ready GO"; " done" hasn't been fed yet.
    assert_eq!(find_line(scr, "ready GO"), Some(0));
    assert_eq!(find_line(scr, "done"), None);

    // Color: the "GO" cells are green (SGR 32 -> palette index 2).
    cells_have_fg(scr, 0, 6..8, Color::Indexed(2)).unwrap();

    // A wrong assertion produces a colored mismatch (not green-on-red bg).
    let err = cells_have_bg(scr, 0, 6..8, Color::Indexed(1)).unwrap_err();
    assert!(err.to_string().contains('\u{1b}'));
}
