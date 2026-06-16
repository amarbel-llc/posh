//! Provenance guard (github #71): the `posh` binary must report both a version
//! and a git sha in `posh version`, formatted `posh <version> (<sha>)`. A build
//! product shipping without version+sha provenance trips this test. See
//! eng-versioning(7).

use std::process::Command;

#[test]
fn version_subcommand_reports_version_and_sha() {
    let out = Command::new(env!("CARGO_BIN_EXE_posh"))
        .arg("version")
        .output()
        .expect("run posh version");
    assert!(out.status.success(), "posh version exited non-zero");
    let line = String::from_utf8(out.stdout).expect("utf8");
    let line = line.trim();

    // Shape: `posh <version> (<sha>)` — both components non-empty.
    let rest = line
        .strip_prefix("posh ")
        .unwrap_or_else(|| panic!("missing `posh ` prefix: {line:?}"));
    let (version, sha) = rest
        .split_once(" (")
        .unwrap_or_else(|| panic!("missing ` (<sha>)`: {line:?}"));
    let sha = sha
        .strip_suffix(')')
        .unwrap_or_else(|| panic!("missing closing `)`: {line:?}"));
    assert!(!version.is_empty(), "empty version in {line:?}");
    assert!(!sha.is_empty(), "empty git sha in {line:?}");
}
