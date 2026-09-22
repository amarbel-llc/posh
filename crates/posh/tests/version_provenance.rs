//! Provenance guard (github #71): the `posh` binary must report both a version
//! and a git sha in `posh version`, formatted `posh <version>+<sha>` — the
//! shape eng-versioning(7)'s "version subcommand output" mandates. A build
//! product shipping without version+sha provenance trips this test.

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

    // Shape: `posh <version>+<sha>` — both components non-empty. The `+` makes
    // the whole token one comparable build identity (SemVer build metadata),
    // rather than a version a sibling build can duplicate.
    let rest = line
        .strip_prefix("posh ")
        .unwrap_or_else(|| panic!("missing `posh ` prefix: {line:?}"));
    let (version, sha) = rest
        .split_once('+')
        .unwrap_or_else(|| panic!("missing `+<sha>`: {line:?}"));
    assert!(!version.is_empty(), "empty version in {line:?}");
    assert!(!sha.is_empty(), "empty git sha in {line:?}");
}
