//! Characterization of mosh's predictive-echo overlay, driven through the FFI
//! shim with an injected clock (task #8).
//!
//! Key behavior captured (confirmed via MOSH_PREDICTION_LOG + terminaloverlay.cc):
//! mosh has a **warm-up**. A prediction is held *tentative* and not painted
//! (ConditionalOverlayCell::apply skips `tentative_until_epoch > confirmed_epoch`)
//! until the server echoes one prediction, which advances `confirmed_epoch`
//! (cull's `Correct` case needs `late_ack >= expiration_frame` and the
//! framebuffer to match). Only then are subsequent same-epoch keystrokes shown
//! speculatively. So a faithful "visible prediction" must model the
//! type -> server-echo -> ack handshake.
//!
//! Unlike the terminal slice's `.in` scripts, each case is an inline closure
//! (multi-step: server frames, ack epochs, keystrokes). Goldens are
//! `tests/fixtures/predict_<name>.grid`. Single `#[test]` because the injected
//! clock is process-global.

use std::path::PathBuf;

use mosh_ffi::{DisplayPreference, MoshPredictor};

fn normalize(grid: &str) -> String {
    grid.lines().map(str::trim_end).collect::<Vec<_>>().join("\n")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Full speculative-echo handshake at a `$ ` prompt:
///   1. type `l` (frame 2)            -> prediction, tentative (warm-up), not shown
///   2. server echoes `l`, late_ack=3 -> cull confirms it, confirmed_epoch advances
///   3. type `s` (frame 4)            -> same epoch, now <= confirmed -> shown speculatively
/// Expected: row 0 reads `$ ls` (the `l` is real/echoed, the `s` is predicted).
fn speculative_echo(pref: DisplayPreference) -> String {
    MoshPredictor::set_clock(1000);
    let mut p = MoshPredictor::new(20, 3, pref, false);
    p.set_send_interval(50);

    // Baseline prompt.
    p.feed_server(b"$ ");
    p.set_frame_acked(1);
    p.set_frame_late_acked(1);

    // Type 'l' (tentative during warm-up).
    p.set_frame_sent(2);
    p.key(b'l');

    // Server echoes 'l' and acks past its expiration frame (2 + 1 = 3).
    p.feed_server(b"l");
    p.set_frame_acked(3);
    p.set_frame_late_acked(3);

    // Type 's' — now shown speculatively (confirmed_epoch has caught up).
    p.set_frame_sent(4);
    p.key(b's');

    p.render()
}

/// Never mode: predictions are never created/shown; only the confirmed frame
/// (the echoed `l`) appears.
fn never_overlay() -> String {
    MoshPredictor::set_clock(1000);
    let mut p = MoshPredictor::new(20, 3, DisplayPreference::Never, false);
    p.set_send_interval(50);
    p.feed_server(b"$ ");
    p.set_frame_acked(1);
    p.set_frame_late_acked(1);
    p.set_frame_sent(2);
    p.key(b'l');
    p.feed_server(b"l");
    p.set_frame_acked(3);
    p.set_frame_late_acked(3);
    p.set_frame_sent(4);
    p.key(b's');
    p.render()
}

#[test]
fn predictor_characterization_matches_goldens() {
    let cases: Vec<(&str, String)> = vec![
        ("speculative_always", speculative_echo(DisplayPreference::Always)),
        ("speculative_adaptive", speculative_echo(DisplayPreference::Adaptive)),
        ("never", never_overlay()),
    ];

    let bless = std::env::var_os("MOSH_FFI_BLESS").is_some();
    let dir = fixtures_dir();
    let mut failures = Vec::new();

    for (name, rendered) in &cases {
        let actual = normalize(rendered);
        let path = dir.join(format!("predict_{name}.grid"));
        if bless {
            std::fs::write(&path, format!("{actual}\n"))
                .unwrap_or_else(|e| panic!("bless predict_{name}.grid: {e}"));
            eprintln!("=== predict_{name} ===\n{actual}\n=== end ===");
            continue;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("read predict_{name}.grid: {e} (run `just debug-mosh-bless`)")
        });
        let expected = expected.strip_suffix('\n').unwrap_or(&expected);
        if actual != *expected {
            failures.push(format!("--- {name} ---\nexpected:\n{expected}\nactual:\n{actual}"));
        }
    }

    if bless {
        eprintln!("blessed {} predictor goldens", cases.len());
        return;
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}
