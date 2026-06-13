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

lint: lint-fmt lint-doc

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

# Compile every doc/*.scd man page, failing on any scdoc parse error
# (the nested-inline-formatting / leading-bracket pitfalls). Cheap
# dev-loop check so a broken page is caught before the .#posh build's
# postInstall does. See eng-manpages(7).
[group("pre-build")]
lint-doc:
    #!/usr/bin/env bash
    set -euo pipefail
    fail=0
    for f in doc/*.scd; do
      if scdoc < "$f" > /dev/null; then
        echo "ok   $f"
      else
        echo "FAIL $f"
        fail=1
      fi
    done
    exit "$fail"

# --- build -----------------------------------------------------------------

build: build-nix build-rust build-go

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

# Hermetic posht build (Go/Bubble Tea terminal-capability test).
[group("build")]
build-go:
    # The static posht binary (docs/posht.md). Sources posht/ only, so
    # non-posht changes don't rebuild it.
    nix build -L --show-trace ".#posht" -o result-posht


# --- post-build ------------------------------------------------------------

test: test-nix test-rust test-go

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

# Hermetic posht build signal (cheap once build-go has realized it).
[group("post-build")]
test-go:
    nix build -L --show-trace --no-link ".#posht"


# --- operational -----------------------------------------------------------

run-nix *ARGS:
    nix run . -- {{ ARGS }}

# Build and run posht: locally with no arguments, or on <host> (cross-
# compile + scp + run via `posh ssh` — the posh#3 plain-SSH path) when a
# host is given. Extra args go to posht; for local args pass an empty
# host: `just run-posht '' --list`.
[group("operational")]
run-posht host="" *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    if [ -n '{{ host }}' ]; then
      exec nix shell nixpkgs#go --command posht/run-remote.sh '{{ host }}' {{ ARGS }}
    fi
    nix shell nixpkgs#go --command bash -c 'cd posht && go build -o posht .'
    exec posht/posht {{ ARGS }}

# Cross-compile + scp + run posht inside a PERSISTENT roaming session on
# <host> (`posh host:SESSION`, RFC 0001 §2). This is the path that carries
# the per-frame DECSET 1007 sync, so it reproduces the wheel→arrows bug
# (posh#3/#28) that the plain-ssh `run-posht` path does not. SESSION
# defaults to "posht". To run only the relevant test:
#   just run-posht-session box -- --only altscroll,mouse
[group("operational")]
run-posht-session host session="posht" *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    exec nix shell nixpkgs#go --command \
      posht/run-remote.sh --via 'session={{ session }}' '{{ host }}' {{ ARGS }}

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

# Verify POSH_GRAB_MOUSE (#50) end-to-end over a LOCAL loopback server+client
# pair, using freshly-built worktree binaries (the profile posh may predate the
# change). Runs posht inside the session; the client takes over your terminal,
# so run it in the terminal you want to test (e.g. kitty). GRAB is on|off —
# run both and compare the altscroll receipt in ~/.local/log/posht/. ARGS go
# to posht (default: --only altscroll). Quit posht normally; detach the client
# with Ctrl-^ then "." . Debug-only; the hermetic gate is build-rust.
[group("debug")]
debug-verify-grab grab="on" *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    args='{{ ARGS }}'; [ -n "$args" ] || args='--only altscroll'
    nix develop --command cargo build -p posh
    nix shell nixpkgs#go --command bash -c 'cd posht && go build -o posht .'
    posh=target/debug/posh
    # Start the loopback server running posht; it prints "POSH CONNECT <port>
    # <key>" then detaches. Capture that line from a fifo.
    fifo=$(mktemp -u); mkfifo "$fifo"; trap 'rm -f "$fifo"' EXIT
    "$posh" server new -4 -- "$PWD/posht/posht" $args >"$fifo" &
    read -r _ _ port key < <(grep -m1 '^POSH CONNECT ' "$fifo")
    echo ">> connecting client (POSH_GRAB_MOUSE={{ grab }}) to 127.0.0.1:$port" >&2
    POSH_KEY="$key" POSH_GRAB_MOUSE='{{ grab }}' exec "$posh" client -4 127.0.0.1 "$port"

# Verify TERM/COLORTERM forwarding (#51) over a LOCAL loopback server+client
# pair with freshly-built worktree binaries. Runs your $SHELL in the session;
# inside it check `echo $TERM` is non-empty and `git -c color.ui=auto status`
# (or a Charmbracelet TUI) shows color. Detach the client with Ctrl-^ then "."
# Loopback note: the server inherits THIS process's env, so it sees the same
# TERM the recipe runs under — the resolution still proves the spawn_shell
# extra_env path; for the true ssh-strips-TERM case test over a real host.
# Debug-only; the hermetic gate is build-rust.
[group("debug")]
debug-verify-term:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    nix develop --command cargo build -p posh
    posh=target/debug/posh
    fifo=$(mktemp -u); mkfifo "$fifo"; trap 'rm -f "$fifo"' EXIT
    "$posh" server new -4 >"$fifo" &
    read -r _ _ port key < <(grep -m1 '^POSH CONNECT ' "$fifo")
    echo ">> connecting client to 127.0.0.1:$port — in the session, run: echo \$TERM" >&2
    POSH_KEY="$key" exec "$posh" client -4 127.0.0.1 "$port"
