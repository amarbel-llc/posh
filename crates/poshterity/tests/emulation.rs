//! Phase-5 adoption (#61): a real mosh emulation byte stream — the VT100
//! attributes test (`zz-mosh/src/tests/emulation-attributes-vt100.test`) —
//! replayed deterministically. This is the `tmux capture-pane` + `sleep` race
//! removed at the root: the screen is a pure function of the recorded bytes,
//! so there is no live terminal and nothing to time.
//!
//! The stream clears the screen then prints `E` under SGR attrs 0/1/4/5/7
//! (normal, bold, underline, blink, inverse) and "end".

use std::process::Command;

use poshterity::assert::{cells_are_bold, cells_are_inverse, cells_are_underline, find_line};
use poshterity::player::Player;

const CASTX: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/emulation-attributes-vt100.castx"
);
const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/emulation-attributes-vt100.grid"
);

/// The committed recording replays to the committed golden — the deterministic
/// analog of the old tmux capture-pane compare, via the `poshterity` binary.
#[test]
fn vt100_attributes_replay_matches_golden() {
    let out = Command::new(env!("CARGO_BIN_EXE_poshterity"))
        .args(["assert", CASTX, "--golden", GOLDEN])
        .output()
        .expect("run poshterity assert");
    assert!(
        out.status.success(),
        "golden assert failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The same stream, checked in-process with the typed assertion helpers: each
/// `E` carries the attribute its SGR set.
#[test]
fn vt100_attributes_carry_the_right_styles() {
    let src = std::fs::read_to_string(CASTX).unwrap();
    let mut p = Player::from_source(&src).unwrap();
    p.step_to_end();
    let scr = p.terminal().screen();

    assert_eq!(find_line(scr, "E E E E E end"), Some(0));
    cells_are_bold(scr, 0, [0], false).unwrap(); // attr 0 — normal
    cells_are_bold(scr, 0, [2], true).unwrap(); // attr 1 — bold
    cells_are_underline(scr, 0, [4], true).unwrap(); // attr 4 — underline
    cells_are_inverse(scr, 0, [8], true).unwrap(); // attr 7 — inverse
}

/// Determinism: replaying the same bytes always yields the same screen. The
/// 50× zero-flake proof lives in `just debug-replay-loop`; this guards the
/// invariant in the normal lane.
#[test]
fn replay_is_repeatable() {
    let src = std::fs::read_to_string(CASTX).unwrap();
    let dump = |s: &str| {
        let mut p = Player::from_source(s).unwrap();
        p.step_to_end();
        p.terminal().dump_vt()
    };
    assert_eq!(dump(&src), dump(&src));
}
