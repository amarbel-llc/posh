// Drift guard: fail the build if Cargo.toml's package.version disagrees
// with the repo-root version.env (the single source of truth, per
// eng-versioning(7) § "Rust crates (build.rs drift guard)"). Cargo's
// package.version field is mandatory, so it cannot be elided in favor of
// version.env; instead `just bump-version` rewrites both together and
// this guard keeps them honest for cargo-native consumers (cargo
// publish, lockfiles, dependents).
//
// Runtime code keeps using env!("CARGO_PKG_VERSION") — trustworthy
// precisely because this guard enforces it matches version.env.
//
// The authoritative version resolves in order:
//   1. $POSH_VERSION in the build environment (set by the nix
//      derivation).
//   2. ../../version.env relative to the crate (dev builds from the
//      workspace checkout; crate is at crates/posh/).
// When neither source exists (e.g. a published crate tarball), the
// guard is a no-op and CARGO_PKG_VERSION stands on its own.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=POSH_VERSION");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let version_env = manifest_dir.join("../../version.env");
    println!("cargo:rerun-if-changed={}", version_env.display());

    let authoritative = env::var("POSH_VERSION").ok().or_else(|| {
        fs::read_to_string(&version_env)
            .ok()
            .as_deref()
            .and_then(parse_posh_version)
    });

    let Some(want) = authoritative else {
        return; // no source of truth available; nothing to guard against.
    };
    let have = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    if want != have {
        panic!(
            "Cargo.toml version ({have}) disagrees with version.env ({want}); \
             run `just bump-version {want}` to resync"
        );
    }
}

// Hand-rolled parse (no regex crate dependency in the build script):
// the first non-comment line whose key — with an optional `export `
// prefix — is POSH_VERSION, with surrounding whitespace and optional
// quotes stripped.
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
