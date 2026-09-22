//! Provenance guard (github #71): the `poshterity` binary must report a build
//! IDENTITY in `poshterity version`, formatted `poshterity <version>+<sha>` —
//! the self-identification line eng-versioning(7)'s "version subcommand
//! output" mandates, and the same shape `posh version` prints. A build product
//! shipping without version+sha provenance trips this test.
//!
//! The version alone would not do: a version does not identify a build, and
//! one host routinely runs several binaries reporting the same one.

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

    // Shape: `poshterity <version>+<sha>` — both components non-empty. The
    // sha half may carry a `-dirty` suffix; the version half never contains
    // `+`, so the first one is the join.
    let rest = line
        .strip_prefix("poshterity ")
        .unwrap_or_else(|| panic!("missing `poshterity ` prefix: {line:?}"));
    let (version, sha) = rest
        .split_once('+')
        .unwrap_or_else(|| panic!("missing `+<sha>`: {line:?}"));
    assert!(!version.is_empty(), "empty version in {line:?}");
    assert!(!sha.is_empty(), "empty git sha in {line:?}");
}
