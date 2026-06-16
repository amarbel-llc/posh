//! Provenance guard (github #71): the `poshterity` binary must report both a
//! version and a git sha in `poshterity version`, formatted
//! `poshterity <version> (<sha>)`. A build product shipping without version+sha
//! provenance trips this test. See eng-versioning(7).

use std::process::Command;

#[test]
fn version_subcommand_reports_version_and_sha() {
    let out = Command::new(env!("CARGO_BIN_EXE_poshterity"))
        .arg("version")
        .output()
        .expect("run poshterity version");
    assert!(out.status.success(), "poshterity version exited non-zero");
    let line = String::from_utf8(out.stdout).expect("utf8");
    let line = line.trim();

    // Shape: `poshterity <version> (<sha>)` — both components non-empty.
    let rest = line
        .strip_prefix("poshterity ")
        .unwrap_or_else(|| panic!("missing `poshterity ` prefix: {line:?}"));
    let (version, sha) = rest
        .split_once(" (")
        .unwrap_or_else(|| panic!("missing ` (<sha>)`: {line:?}"));
    let sha = sha
        .strip_suffix(')')
        .unwrap_or_else(|| panic!("missing closing `)`: {line:?}"));
    assert!(!version.is_empty(), "empty version in {line:?}");
    assert!(!sha.is_empty(), "empty git sha in {line:?}");
}
