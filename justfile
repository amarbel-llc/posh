# mosh justfile — eng conventions: verb-noun leaves under bare aggregates,
# aggregates-only `default` (first recipe), eng-nix(7) flags on every nix
# invocation (--show-trace always; -L on builds; --no-link on verify-only).
# See eng-design_patterns-justfile(7) and eng-nix(7).

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

build: build-nix

# Hermetic flake build: autogen.sh + configure + make (+ make check).
[group("build")]
build-nix:
    # The CI-equivalent build; doCheck runs the sandbox-safe test subset.
    nix build -L --show-trace

# Fast C++ dev-loop: autotools build in the devShell, no nix rebuild.
[group("build")]
build-autotools:
    # Leaves build products in the worktree for iteration.
    nix develop --command bash -c './autogen.sh && ./configure --with-crypto-library=openssl && make'

# --- post-build ------------------------------------------------------------

test: test-nix

# Hermetic, CI-safe test signal (the package's doCheck `make check`).
[group("post-build")]
test-nix:
    #!/usr/bin/env bash
    set -euo pipefail
    # The sandbox runs the crypto/protocol subset and SKIPs the tmux/pty
    # emulation tests, so this lane is deterministic. Cheap once build-nix
    # has realized the derivation.
    system=$(nix eval --raw --impure --expr 'builtins.currentSystem')
    nix build -L --show-trace ".#packages.${system}.default"

# Full host `make check` (includes tmux emulation tests; not in `default`).
[group("post-build")]
test-autotools: build-autotools
    # The sandbox SKIPs the tmux/pty emulation tests; this runs them on the
    # host. Kept OUT of `default` because the emulation suite has a host
    # failure (emulation-80th-column) tracked in amarbel-llc/mosh#2. Promote
    # back into the `test` aggregate once that's resolved.
    nix develop --command make check

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
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf result result-*
    nix develop --command bash -c 'make distclean || true'

[group("maintenance")]
update-nix:
    nix flake update

# --- debug -----------------------------------------------------------------

# Run a single autotools test by name (e.g. `just debug-test-one
# mouse-alternate-scroll.test`) in the devShell, for fast iteration on one
# test without the whole `make check` suite.
[group("debug")]
debug-test-one name: build-autotools
    nix develop --command make -C src/tests check TESTS='{{ name }}'
