# mosh justfile — eng conventions: verb-noun leaves under bare aggregates,
# aggregates-only `default` (first recipe), eng-nix(7) flags on every nix
# invocation (--show-trace always; -L on builds; --no-link on verify-only).
# See eng-design_patterns-justfile(7) and eng-nix(7).

default: validate lint build test

# --- pre-build -------------------------------------------------------------

validate: validate-devshell

# Verify the devShell evaluates and builds without errors — catches
# devShell-only breakage the prod-binary build can mask. Build-check only
# (--no-link), no kept artifact.
[group("pre-build")]
validate-devshell:
    #!/usr/bin/env bash
    set -euo pipefail
    system=$(nix eval --raw --impure --expr 'builtins.currentSystem')
    nix build --no-link --show-trace ".#devShells.${system}.default"

lint: lint-fmt

# Read-only formatting gate: builds the `checks.formatting` derivation,
# which runs treefmt against a /nix/store snapshot and fails if anything
# would change. Does NOT modify the worktree — the modifying counterpart
# is `codemod-fmt-treefmt`.
[group("pre-build")]
lint-fmt:
    #!/usr/bin/env bash
    set -euo pipefail
    system=$(nix eval --raw --impure --expr 'builtins.currentSystem')
    nix build ".#checks.${system}.formatting" --no-link --print-build-logs

# --- build -----------------------------------------------------------------

build: build-nix

# Hermetic package build through the flake: autogen.sh + configure + make
# (+ make check via doCheck). This is the CI-equivalent build.
[group("build")]
build-nix:
    nix build -L --show-trace

# Fast C++ dev-loop: run the autotools build directly inside the devShell,
# no nix rebuild. Leaves build products in the worktree for iteration.
[group("build")]
build-autotools:
    nix develop --command bash -c './autogen.sh && ./configure --with-crypto-library=openssl && make'

# --- post-build ------------------------------------------------------------

test: test-autotools

# Run mosh's own `make check` suite inside the devShell against the
# autotools build. (`build-nix` already exercises the suite via doCheck;
# this gives a fast, isolated test signal without a full nix rebuild.)
[group("post-build")]
test-autotools: build-autotools
    nix develop --command make check

# --- operational -----------------------------------------------------------

run-nix *ARGS:
    nix run . -- {{ ARGS }}

# --- codemod ---------------------------------------------------------------

codemod-fmt: codemod-fmt-treefmt

# Rewrite the worktree in place via treefmt (clang-format + nixfmt + shfmt).
# Read-only counterpart is `lint-fmt`. They share ./treefmt.nix.
[group("codemod")]
codemod-fmt-treefmt:
    nix fmt

# --- maintenance -----------------------------------------------------------

clean: clean-build

[group("maintenance")]
clean-build:
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf result result-*
    nix develop --command bash -c 'make distclean || true'

[group("maintenance")]
update-nix:
    nix flake update
