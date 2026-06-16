//! Shared build-script helper for the posh workspace.
//!
//! Every posh crate's `build.rs` is one line — `posh_build::flow();` — so the
//! version + git-sha resolution lives in exactly one place and cannot drift
//! across crates (github #71). It flows the repo's single source of truth,
//! `version.env` (`POSH_VERSION`), plus the git SHA (`POSH_GIT_SHA`) into the
//! consuming crate as compile-time env vars; runtime code reads
//! `env!("POSH_VERSION")` / `env!("POSH_GIT_SHA")`. See eng-versioning(7).
//!
//! Cargo's `package.version` stays an inert "0.0.0" placeholder (see the root
//! `Cargo.toml`) that nothing reads for the actual version — so there is
//! nothing to keep in sync and no drift to guard against.

use std::env;
use std::fs;
use std::path::PathBuf;

/// Flow `POSH_VERSION` and `POSH_GIT_SHA` into the consuming crate. Call from
/// each crate's `build.rs` `main`.
///
/// Runs in the *consuming* crate's build-script process, so
/// `CARGO_MANIFEST_DIR` is that crate's directory — every posh crate lives at
/// `crates/<name>/`, so `../../version.env` resolves to the repo root for all
/// of them.
pub fn flow() {
    flow_version();
    flow_git_sha();
}

/// Resolve and flow the authoritative version, in order:
///   1. `$POSH_VERSION` in the build environment (set by the nix derivation).
///   2. `../../version.env` relative to the crate (dev builds from the
///      workspace checkout).
///   3. `CARGO_PKG_VERSION` as a never-hit fallback (only when neither source
///      exists, e.g. a published crate tarball), so `env!("POSH_VERSION")`
///      always resolves.
fn flow_version() {
    println!("cargo:rerun-if-env-changed=POSH_VERSION");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let version_env = manifest_dir.join("../../version.env");
    println!("cargo:rerun-if-changed={}", version_env.display());

    let version = env::var("POSH_VERSION")
        .ok()
        .or_else(|| {
            fs::read_to_string(&version_env)
                .ok()
                .as_deref()
                .and_then(parse_posh_version)
        })
        .or_else(|| env::var("CARGO_PKG_VERSION").ok())
        .expect("no version source: POSH_VERSION, version.env, or CARGO_PKG_VERSION");

    // Flow the authoritative version into the crate. Runtime: env!("POSH_VERSION").
    println!("cargo:rustc-env=POSH_VERSION={version}");
}

/// Resolve and flow the git revision (github #63). Resolves like the version:
///   1. `$POSH_GIT_SHA` in the build env (set by the nix derivation from the
///      flake's git rev — already carries a "-dirty" suffix when unclean).
///   2. `git` in a dev checkout — short sha plus "-dirty" for a modified tree.
///   3. "unknown" (no env, no git — e.g. a source tarball).
fn flow_git_sha() {
    println!("cargo:rerun-if-env-changed=POSH_GIT_SHA");
    let git_sha = env::var("POSH_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(git_describe)
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=POSH_GIT_SHA={git_sha}");
}

/// Dev-checkout git revision: `<short-sha>` plus `-dirty` when the working tree
/// has uncommitted changes. `None` outside a git checkout (the nix build sets
/// `$POSH_GIT_SHA` instead, so this never runs there).
fn git_describe() -> Option<String> {
    use std::process::Command;
    let rev = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !rev.status.success() {
        return None;
    }
    let mut sha = String::from_utf8(rev.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    if let Ok(status) = Command::new("git").args(["status", "--porcelain"]).output() {
        if status.status.success() && !status.stdout.is_empty() {
            sha.push_str("-dirty");
        }
    }
    Some(sha)
}

/// Hand-rolled parse (no regex crate dependency in the build script): the first
/// non-comment line whose key — with an optional `export ` prefix — is
/// `POSH_VERSION`, with surrounding whitespace and optional quotes stripped.
fn parse_posh_version(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        let body = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let (key, value) = body.split_once('=')?;
        if key.trim() != "POSH_VERSION" {
            continue;
        }
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        if value.is_empty() {
            return None;
        }
        return Some(value);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_posh_version;

    #[test]
    fn parses_export_prefixed_line() {
        assert_eq!(
            parse_posh_version("export POSH_VERSION=0.1.3\n").as_deref(),
            Some("0.1.3")
        );
    }

    #[test]
    fn parses_bare_key_and_strips_quotes() {
        assert_eq!(
            parse_posh_version("POSH_VERSION=\"1.2.3\"\n").as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            parse_posh_version("POSH_VERSION='4.5.6'").as_deref(),
            Some("4.5.6")
        );
    }

    #[test]
    fn skips_comments_blank_lines_and_other_keys() {
        let content = "# a comment\n\nOTHER=nope\nexport POSH_VERSION=9.9.9\n";
        assert_eq!(parse_posh_version(content).as_deref(), Some("9.9.9"));
    }

    #[test]
    fn none_when_absent_or_empty() {
        assert_eq!(parse_posh_version("OTHER=1\n"), None);
        assert_eq!(parse_posh_version("POSH_VERSION=\n"), None);
    }
}
