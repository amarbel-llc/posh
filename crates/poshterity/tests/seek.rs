//! CLI wiring for `poshterity seek`: exec the real binary against a hand-written
//! `.castx` and check it finds the first write whose screen matches, in one
//! process, and fails loudly when none does.

use std::process::Command;

// write 1 "alpha"; write 2 adds a "banner" line; write 3 clears the screen.
const FIXTURE: &str = "{\"version\":2,\"width\":20,\"height\":4}\n\
                       [0.0,\"o\",\"alpha\"]\n\
                       [0.1,\"o\",\"\\r\\nbanner\"]\n\
                       [0.2,\"o\",\"\\u001b[2J\\u001b[Hgone\"]\n";

fn fixture(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("poshterity-seek-{}-{tag}.castx", std::process::id()));
    std::fs::write(&path, FIXTURE).unwrap();
    path
}

struct Ran {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn seek(f: &std::path::Path, args: &[&str]) -> Ran {
    let out = Command::new(env!("CARGO_BIN_EXE_poshterity"))
        .arg("seek")
        .arg(f)
        .args(args)
        .output()
        .expect("run poshterity");
    Ran {
        ok: out.status.success(),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

#[test]
fn seek_contains_prints_the_matching_screen_and_the_write_index() {
    let f = fixture("contains");
    let r = seek(&f, &["--contains", "banner"]);
    assert!(r.ok, "{}", r.stderr);
    assert!(r.stdout.contains("alpha") && r.stdout.contains("banner"), "{:?}", r.stdout);
    assert!(r.stderr.contains("write 2"), "{:?}", r.stderr);
    let _ = std::fs::remove_file(&f);
}

#[test]
fn seek_lacks_finds_the_first_screen_without_the_text() {
    let f = fixture("lacks");
    // "banner" is on screen after write 2 and gone after the clear in write 3.
    let r = seek(&f, &["--contains", "alpha", "--lacks", "banner"]);
    assert!(r.ok && r.stderr.contains("write 1"), "{} / {}", r.stderr, r.stdout);
    let r = seek(&f, &["--lacks", "banner", "--from-write", "2"]);
    assert!(r.ok && r.stderr.contains("write 3"), "{} / {}", r.stderr, r.stdout);
    assert!(r.stdout.contains("gone") && !r.stdout.contains("alpha"), "{:?}", r.stdout);
    let _ = std::fs::remove_file(&f);
}

#[test]
fn seek_that_never_matches_fails_and_says_what_it_wanted() {
    let f = fixture("never");
    let r = seek(&f, &["--contains", "nope"]);
    assert!(!r.ok, "a miss must be a non-zero exit");
    assert!(r.stderr.contains("nope"), "{:?}", r.stderr);
    let _ = std::fs::remove_file(&f);
}

#[test]
fn seek_needs_a_condition() {
    let f = fixture("bare");
    let r = seek(&f, &[]);
    assert!(!r.ok);
    assert!(r.stderr.contains("--contains") || r.stderr.contains("--lacks"), "{:?}", r.stderr);
    let _ = std::fs::remove_file(&f);
}
