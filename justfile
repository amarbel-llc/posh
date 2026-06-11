# posh justfile — eng conventions: verb-noun leaves under bare aggregates,
# aggregates-only `default` (first recipe), eng-nix(7) flags on every nix
# invocation (--show-trace always; -L on builds; --no-link on verify-only).
# See eng-design_patterns-justfile(7) and eng-nix(7). The C++ reference
# tree lives in zz-mosh/ with its own justfile for the host-lane recipes
# (`just zz-mosh/<recipe>`); the hermetic .#mosh lane stays here.

default: validate lint build test

# --- pre-build -------------------------------------------------------------

validate: validate-devshell

# Build-check the devShell (catches devShell-only breakage prod build masks).
[group("pre-build")]
validate-devshell:
    #!/usr/bin/env bash
    set -euo pipefail
    # --no-link: build-check only, no kept artifact.
    system=$(nix eval --raw --impure --expr 'builtins.currentSystem')
    nix build --no-link --show-trace ".#devShells.${system}.default"

lint: lint-fmt

# Read-only formatting gate (fails if treefmt would change anything).
[group("pre-build")]
lint-fmt:
    #!/usr/bin/env bash
    set -euo pipefail
    # Builds the checks.formatting derivation, which runs treefmt against a
    # /nix/store snapshot. Does NOT modify the worktree — the modifying
    # counterpart is `codemod-fmt-treefmt`. They share ./treefmt.nix.
    system=$(nix eval --raw --impure --expr 'builtins.currentSystem')
    nix build ".#checks.${system}.formatting" --no-link --print-build-logs

# --- build -----------------------------------------------------------------

build: build-nix build-rust

# Hermetic C++ reference build: autogen.sh + configure + make (+ check).
[group("build")]
build-nix:
    # The C++ lane; doCheck runs the sandbox-safe test subset. Sources
    # zz-mosh/ only, so Rust-only changes don't rebuild it.
    nix build -L --show-trace ".#mosh" -o result-mosh

# Hermetic Rust workspace build (cargo test --workspace in checkPhase).
[group("build")]
build-rust:
    # The Rust CI gate (github #33) and the default package; ./result is
    # the posh binary (bin/posh + the bin/posh-server alias).
    nix build -L --show-trace


# --- post-build ------------------------------------------------------------

test: test-nix test-rust

# Hermetic, CI-safe C++ test signal (the mosh package's doCheck).
[group("post-build")]
test-nix:
    # The sandbox runs the crypto/protocol/--local subset and SKIPs the
    # tmux emulation tests, so this lane is deterministic. Cheap once
    # build-nix has realized the derivation.
    nix build -L --show-trace --no-link ".#mosh"

# Hermetic Rust test signal (cargo test --workspace in the posh checkPhase).
[group("post-build")]
test-rust:
    # Cheap once build-rust has realized the derivation. github #33.
    nix build -L --show-trace --no-link ".#posh"


# --- operational -----------------------------------------------------------

run-nix *ARGS:
    nix run . -- {{ ARGS }}

# --- codemod ---------------------------------------------------------------

codemod-fmt: codemod-fmt-treefmt

# Rewrite the worktree in place via treefmt (clang-format + nixfmt + shfmt).
[group("codemod")]
codemod-fmt-treefmt:
    # Read-only counterpart is `lint-fmt`. They share ./treefmt.nix.
    nix fmt

# --- maintenance -----------------------------------------------------------

clean: clean-build

[group("maintenance")]
clean-build:
    # The C++ tree's distclean is `just zz-mosh/clean-build`.
    rm -rf result result-*

[group("maintenance")]
update-nix:
    nix flake update

# --- debug -----------------------------------------------------------------

# Run cargo against the Rust workspace in the devShell — the fast dev-loop
# (incremental, in-worktree). The hermetic gate is build-rust/test-rust.
[group("debug")]
debug-cargo *ARGS:
    nix develop --command cargo {{ ARGS }}

# Run go against the posht tool via nixpkgs (no Go in the devShell yet —
# posht is a standalone static TUI, see docs/posht.md / PR #38).
[group("debug")]
debug-go *ARGS:
    nix shell nixpkgs#go --command bash -c 'cd posht && go {{ ARGS }}'
