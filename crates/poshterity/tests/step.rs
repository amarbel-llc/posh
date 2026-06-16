//! CLI wiring for the step-ratchet Player: exec the real `poshterity` binary
//! against a hand-written `.castx` (no PTY needed) and check `step` / `replay
//! --to-marker` dump the expected intermediate screens.

use std::process::Command;

const FIXTURE: &str = "{\"version\":2,\"width\":20,\"height\":3}\n\
                       [0.0,\"o\",\"alpha\"]\n\
                       [0.1,\"m\",\"done\"]\n\
                       [0.2,\"o\",\"beta\"]\n";

fn write_fixture(tag: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("poshterity-step-{}-{tag}.castx", std::process::id()));
    std::fs::write(&path, FIXTURE).unwrap();
    path
}

fn run(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_poshterity"))
        .args(args)
        .output()
        .expect("run poshterity");
    assert!(
        out.status.success(),
        "poshterity {args:?} failed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn step_by_write_dumps_intermediate_screen() {
    let f = write_fixture("write");
    let fs = f.to_str().unwrap();
    // One write = "alpha"; the second write ("beta") hasn't been fed.
    let text = run(&["step", fs, "--by", "write", "--n", "1", "--dump", "text"]);
    assert!(text.contains("alpha"), "{text:?}");
    assert!(!text.contains("beta"), "{text:?}");
    let _ = std::fs::remove_file(&f);
}

#[test]
fn step_to_marker_lands_before_later_output() {
    let f = write_fixture("marker");
    let fs = f.to_str().unwrap();
    let text = run(&["step", fs, "--to-marker", "done", "--dump", "text"]);
    assert!(text.contains("alpha"), "{text:?}");
    assert!(!text.contains("beta"), "{text:?}");
    let _ = std::fs::remove_file(&f);
}

#[test]
fn replay_to_marker_stops_at_the_marker() {
    let f = write_fixture("replay");
    let fs = f.to_str().unwrap();
    // Whole replay shows both; --to-marker stops at the marker.
    assert!(run(&["replay", fs, "--dump", "text"]).contains("beta"));
    let stopped = run(&["replay", fs, "--to-marker", "done", "--dump", "text"]);
    assert!(stopped.contains("alpha") && !stopped.contains("beta"), "{stopped:?}");
    let _ = std::fs::remove_file(&f);
}
