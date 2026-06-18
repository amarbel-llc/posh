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

lint: lint-fmt lint-doc lint-impure

# Read-only formatting + eng-convention gate (fails if conformist would change
# anything, or an eng linter finds a violation).
[group("pre-build")]
lint-fmt:
    #!/usr/bin/env bash
    set -euo pipefail
    # Builds the checks.formatting derivation, which runs `conformist check`
    # against a /nix/store snapshot. Does NOT modify the worktree — the
    # modifying counterpart is `codemod-fmt-conformist`. They share
    # ./conformist.nix (the committed ./conformist.toml is generated from the
    # same module via `gen-conformist`).
    system=$(nix eval --raw --impure --expr 'builtins.currentSystem')
    nix build ".#checks.${system}.formatting" --no-link --print-build-logs

# Compile every doc/*.scd man page, failing on any scdoc parse error
# (the nested-inline-formatting / leading-bracket pitfalls). Cheap
# dev-loop check so a broken page is caught before the .#posh build's
# postInstall does. Runs scdoc through `nix develop` so it works
# whether or not the devShell is already active (the pre-merge hook
# runs `just` outside it). See eng-manpages(7).
[group("pre-build")]
lint-doc:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    nix develop --command bash -c '
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
    '

lint-impure: lint-worktree

# Impure eng checks against the WORKING TREE (not the sandbox): the eng-impure
# preset's git-state linters — agents-md (CLAUDE.md -> AGENTS.md), git-remotes
# (SSH-only), git-default-branch (master, no main), and sweatfile (`spinclass
# validate`). These need a live .git + profile tools, so they can't run in the
# sandboxed checks.formatting / `lint-fmt`. Builds the impure config in
# /nix/store, then runs `conformist check` rooted at the worktree. See
# conformist-nix(7) and the eng-impure preset.
[group("pre-build")]
lint-worktree:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    system=$(nix eval --raw --impure --expr 'builtins.currentSystem')
    cfg=$(nix build --no-link --print-out-paths ".#packages.${system}.conformist-impure-config")
    conformist check --config-file "$cfg" --tree-root .

# --- build -----------------------------------------------------------------

build: build-nix build-rust build-go build-toolset

# Hermetic C++ reference build: autogen.sh + configure + make (+ check).
[group("build")]
build-nix:
    # The C++ lane; doCheck runs the sandbox-safe test subset. Sources
    # zz-mosh/ only, so Rust-only changes don't rebuild it.
    nix build -L --show-trace ".#mosh" -o result-mosh

# Hermetic Rust workspace build (cargo test --workspace in checkPhase).
[group("build")]
build-rust:
    # The Rust CI gate (github #33); result-posh holds bin/posh, the
    # bin/posh-server alias, and bin/poshterity.
    nix build -L --show-trace ".#posh" -o result-posh

# Hermetic posht build (Go/Bubble Tea terminal-capability test).
[group("build")]
build-go:
    # The static posht binary (docs/posht.md). Sources posht/ only, so
    # non-posht changes don't rebuild it.
    nix build -L --show-trace ".#posht" -o result-posht

# The default output: the full posh toolset (github #73). ./result
# aggregates posh + posh-server + poshterity + posht so a bare `nix build`
# yields the whole set. Also realizes the standalone .#poshterity output
# (`nix run .#poshterity`) so the merge gate exercises it. Cheap once
# build-rust/build-go realized the inputs.
[group("build")]
build-toolset:
    nix build -L --show-trace -o result
    nix build -L --show-trace --no-link ".#poshterity"


# --- post-build ------------------------------------------------------------

test: test-nix test-rust test-go test-mosh-ffi

# Hermetic, CI-safe C++ test signal (the mosh package's doCheck).
[group("post-build")]
test-nix:
    # The sandbox runs the crypto/protocol/--local subset and SKIPs the
    # tmux emulation tests, so this lane is deterministic. Cheap once
    # build-nix has realized the derivation.
    nix build -L --show-trace --no-link ".#mosh"

# Hermetic Rust test signal (posh checkPhase; mosh-ffi is gated separately by
# test-mosh-ffi via workspace default-members).
[group("post-build")]
test-rust:
    # Cheap once build-rust has realized the derivation. github #33.
    nix build -L --show-trace --no-link ".#posh"

# Hermetic posht build signal (cheap once build-go has realized it).
[group("post-build")]
test-go:
    nix build -L --show-trace --no-link ".#posht"

# Hermetic mosh-ffi gate: the C++ FFI oracle's characterization tests (ADR
# 0004). mosh-ffi is excluded from .#posh (workspace default-members), so this
# builds the dedicated .#checks.<system>.mosh-ffi derivation, which runs
# cargo test -p mosh-ffi (compiling the zz-mosh C++ slice via cc).
[group("post-build")]
test-mosh-ffi:
    #!/usr/bin/env bash
    set -euo pipefail
    system=$(nix eval --raw --impure --expr 'builtins.currentSystem')
    nix build -L --show-trace --no-link ".#checks.${system}.mosh-ffi"


# --- operational -----------------------------------------------------------

# Run the default posh toolset via the flake (nix run . -- <args>).
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

codemod-fmt: codemod-fmt-conformist

# Rewrite the worktree in place via conformist (clang-format + nixfmt + shfmt).
[group("codemod")]
codemod-fmt-conformist:
    # Read-only counterpart is `lint-fmt`. They share ./conformist.nix.
    nix fmt

# --- maintenance -----------------------------------------------------------

clean: clean-build

# Remove nix build symlinks (result, result-*) from the worktree.
[group("maintenance")]
clean-build:
    # The C++ tree's distclean is `just zz-mosh/clean-build`.
    rm -rf result result-*

# Update every flake input to its latest revision (rewrites flake.lock).
[group("maintenance")]
update-nix:
    nix flake update

# Regenerate the committed conformist.toml from ./conformist.nix (the nix
# module is the source of truth). The bare `conformist --staged` pre-commit
# hook and `conformist --commit` repair hook discover this committed file by
# walking up the tree — they take no --config-file — so it must be kept in
# sync with the module. Run after editing conformist.nix.
[group("maintenance")]
update-conformist-config:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    system=$(nix eval --raw --impure --expr 'builtins.currentSystem')
    out=$(nix build --no-link --print-out-paths ".#packages.${system}.conformist-config")
    install -m 644 "$out" conformist.toml
    echo "regenerated conformist.toml from ./conformist.nix"

# Register the version.env merge driver in this clone's git config so
# .gitattributes' `merge=keep-higher-semver` resolves. Required once per clone
# (the driver name lives in .git/config, which is not version-controlled and
# is shared across worktrees via $GIT_COMMON_DIR). posh's sweatfile [hooks]
# create also runs this at `sc start`, so a fresh spinclass worktree is wired
# automatically; this recipe is the manual / non-spinclass path. Idempotent.
[group("maintenance")]
install-merge-driver:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    git config merge.keep-higher-semver.name 'keep the higher POSH_VERSION (version.env)'
    git config merge.keep-higher-semver.driver 'scripts/version-merge %O %A %B'
    echo "registered merge.keep-higher-semver driver"

# Version bump + tag + release, per eng-versioning(7). version.env
# (POSH_VERSION) is posh's single source of truth, read by flake.nix at
# eval time and flowed into every crate at build time via each crate's
# build.rs (cargo:rustc-env=POSH_VERSION). `bump-version` is a pure
# mutation; `tag` reads the current value and pushes a signed tag;
# `release` orchestrates changelog -> bump -> commit -> tag -> gh release.
# (mosh and posht keep their own version lineages and are untouched by
# these recipes.)

# Rewrite POSH_VERSION in version.env — the only place the version lives.
# The crates' Cargo.toml package.version is an inert "0.0.0" placeholder
# (version.workspace = true) that build.rs overrides at compile time, so
# there is no Cargo.toml or Cargo.lock version to resync here. Touches no
# other file — committing is `release`'s job. Usage: just bump-version 0.1.1
[group("maintenance")]
bump-version new_version:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    sed -E -i 's/^(export POSH_VERSION)=.*/\1={{ new_version }}/' version.env

# Sign + push the tag named after version.env (the "v" prefix is added for
# you), then verify the signature. `message` is declared with a leading `$`
# so just exports it into the environment instead of {{ }}-splicing it into
# the script — a changelog body with backticks or $(...) would otherwise be
# re-parsed by bash into the annotation (eng-versioning(7) § tag recipe).
# Usage: just tag "posh v0.1.1"
[group("maintenance")]
tag $message:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    . version.env
    tag="v${POSH_VERSION:?missing POSH_VERSION in version.env}"
    git tag -s -m "$message" "$tag"
    gum log --level info "created tag $tag"
    git push origin "$tag"
    gum log --level info "pushed $tag"
    git tag -v "$tag"

# Cut a release from the default branch (eng-versioning(7) § release):
# refuse off master; generate the changelog (commits since the previous v*
# tag) BEFORE bumping so the bump commit isn't in its own changelog; bump
# version.env (the only versioned file — Pattern B, no Cargo.toml resync),
# commit, sign+push+verify a v<sem> tag, and create the GitHub release with
# the changelog as the body. The bump+commit is idempotent: skipped when
# version.env already holds <new>. Usage: just release 0.1.1
[group("maintenance")]
release new_version:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'

    # Release only from the default branch.
    branch="$(git rev-parse --abbrev-ref HEAD)"
    if [ "$branch" != "master" ]; then
        gum log --level error "release only allowed from master (on '$branch')"
        exit 1
    fi

    header="posh v{{ new_version }}"
    # Commits since the last v* tag (all history when none exists yet),
    # computed BEFORE the bump so the bump commit isn't in its own changelog.
    last_tag="$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || true)"
    if [ -n "$last_tag" ]; then
        changelog="$(git log --no-merges --pretty='- %s' "${last_tag}..HEAD")"
    else
        changelog="$(git log --no-merges --pretty='- %s')"
    fi
    notes="$header"$'\n\n'"${changelog:-- (no changes recorded)}"

    # Idempotent bump: skip when version.env already holds the target, else
    # `git commit` would abort with "nothing to commit".
    . version.env
    if [ "${POSH_VERSION:-}" != "{{ new_version }}" ]; then
        just bump-version "{{ new_version }}"
        git add version.env
        git commit -m "$header"
    else
        gum log --level info "version.env already at {{ new_version }}; skipping bump/commit"
    fi

    # The full changelog rides as the tag annotation (safe via tag's $message).
    just tag "$notes"
    gh release create "v{{ new_version }}" --title "$header" --notes "$notes"

# --- debug -----------------------------------------------------------------

# Exercise scripts/version-merge (the version.env semver merge driver) on
# synthetic %O/%A/%B blobs and assert the higher POSH_VERSION wins, in both
# orderings, plus the fail-safe (non-zero exit when a side lacks a parseable
# version). Debug-only; the driver's real exercise is a rebase conflict.
[group("debug")]
debug-version-merge:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
    fail=0
    assert_eq() { # <label> <expected> <actual>
      if [ "$2" = "$3" ]; then echo "  PASS $1"; else echo "  FAIL $1: expected '$2' got '$3'"; fail=1; fi
    }
    mk() { printf 'export POSH_VERSION=%s\n' "$1"; }

    # theirs higher: ours=0.1.8, theirs=0.2.0 -> 0.2.0 wins, exit 0
    o="$tmp/o" a="$tmp/a" b="$tmp/b"
    mk 0.1.0 >"$o"; mk 0.1.8 >"$a"; mk 0.2.0 >"$b"
    scripts/version-merge "$o" "$a" "$b"; rc=$?
    assert_eq "theirs-higher exit" 0 "$rc"
    assert_eq "theirs-higher value" "export POSH_VERSION=0.2.0" "$(cat "$a")"

    # ours higher: ours=0.3.0, theirs=0.2.5 -> ours kept, exit 0
    mk 0.1.0 >"$o"; mk 0.3.0 >"$a"; mk 0.2.5 >"$b"
    scripts/version-merge "$o" "$a" "$b"; rc=$?
    assert_eq "ours-higher exit" 0 "$rc"
    assert_eq "ours-higher value" "export POSH_VERSION=0.3.0" "$(cat "$a")"

    # double-digit ordering (lexical trap): 0.9.0 vs 0.10.0 -> 0.10.0 wins
    mk 0.1.0 >"$o"; mk 0.9.0 >"$a"; mk 0.10.0 >"$b"
    scripts/version-merge "$o" "$a" "$b"
    assert_eq "semver-not-lexical" "export POSH_VERSION=0.10.0" "$(cat "$a")"

    # fail-safe: theirs unparseable -> non-zero exit, ours untouched
    mk 0.1.0 >"$o"; mk 0.1.8 >"$a"; printf 'garbage\n' >"$b"
    if scripts/version-merge "$o" "$a" "$b" 2>/dev/null; then
      echo "  FAIL fail-safe: expected non-zero exit"; fail=1
    else
      echo "  PASS fail-safe exit"
    fi
    assert_eq "fail-safe untouched" "export POSH_VERSION=0.1.8" "$(cat "$a")"

    [ "$fail" -eq 0 ] && echo "version-merge: all checks passed" || { echo "version-merge: FAILURES"; exit 1; }

# End-to-end proof that git actually invokes the keep-higher-semver driver via
# .gitattributes: build a throwaway repo under .tmp/, wire the driver + a
# `version.env merge=keep-higher-semver` attribute, create a real divergent
# version.env conflict, merge, and assert the higher semver landed with NO
# conflict markers. Exercises the git plumbing the unit test (debug-version-
# merge) cannot. Debug-only.
[group("debug")]
debug-merge-driver-e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    driver="$PWD/scripts/version-merge"
    repo=$(mktemp -d "$PWD/.tmp/merge-e2e.XXXXXX"); trap 'rm -rf "$repo"' EXIT
    cd "$repo"
    git init -q
    git config user.email e2e@posh.test
    git config user.name "posh e2e"
    git config commit.gpgsign false
    git config merge.keep-higher-semver.name "keep higher POSH_VERSION"
    git config merge.keep-higher-semver.driver "$driver %O %A %B"
    printf 'version.env merge=keep-higher-semver\n' >.gitattributes
    printf 'export POSH_VERSION=0.1.0\n' >version.env
    git add -A; git commit -qm base
    # branch bumps to 0.1.8
    git switch -qc feature
    printf 'export POSH_VERSION=0.1.8\n' >version.env
    git commit -qam "feature: bump 0.1.8"
    # master races ahead to 0.2.0 (a release)
    git switch -q master
    printf 'export POSH_VERSION=0.2.0\n' >version.env
    git commit -qam "release: 0.2.0"
    # merge feature into master: without the driver this conflicts on version.env
    git merge -q --no-edit feature
    got=$(cat version.env)
    if grep -q '^<<<<<<<' version.env; then
      echo "FAIL: conflict markers present — driver did not fire"; exit 1
    fi
    if [ "$got" = "export POSH_VERSION=0.2.0" ]; then
      echo "PASS: merge kept higher semver (0.2.0), no conflict markers"
    else
      echo "FAIL: expected 0.2.0, got '$got'"; exit 1
    fi

# Run cargo against the Rust workspace in the devShell — the fast dev-loop
# (incremental, in-worktree). The hermetic gate is build-rust/test-rust.
[group("debug")]
debug-cargo *ARGS:
    nix develop --command cargo {{ ARGS }}

# (Re)bless the mosh terminal characterization goldens (task #4). The driver is
# the mosh-ffi C++ FFI shim, so a fixed VT script always renders the same grid
# (no clock, no network). Assert with the normal loop: `just debug-cargo test
# -p mosh-ffi`. Debug-only; the hermetic gate is build-rust.
[group("debug")]
debug-mosh-bless:
    nix develop --command env MOSH_FFI_BLESS=1 cargo test -p mosh-ffi -- --nocapture

# Perf probe (prediction perf followups #13/#15): time the per-frame client
# apply costs at representative sizes, in release. Runs both probes in
# perf_probe.rs: the DumpDiff reparse + compose (Snapshot::from_term) baseline,
# and the #15 MorphDelta incremental apply (process(escapes)) vs DumpDiff
# reparse comparison. Drives the measure-first perf work so optimization isn't
# speculative. Debug-only; the hermetic gate is build-rust.
[group("debug")]
debug-perf-compose:
    nix develop --command cargo test -p posh --release remote::perf_probe -- --ignored --nocapture

# Prove poshterity replay determinism (poshterity phase 5, #61): `poshterity assert`
# the committed VT100 emulation fixture against its golden N times (default 50)
# and fail loudly on the first mismatch. Zero flakes is the headline of the
# deterministic replacement for the mosh tests' tmux capture-pane + sleep race.
[group("debug")]
debug-replay-loop n="50":
    nix develop --command bash -c ' \
      set -euo pipefail; \
      cargo build -q -p poshterity; \
      f=crates/poshterity/tests/fixtures/emulation-attributes-vt100; \
      for i in $(seq 1 {{ n }}); do \
        ./target/debug/poshterity assert "$f.castx" --golden "$f.grid" \
          || { echo "FLAKE at iteration $i"; exit 1; }; \
      done; \
      echo "{{ n }}/{{ n }} deterministic, zero flakes"'

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

# Record an interactive posht session over a chosen transport to a .castx, to
# capture and diff posh's client-side rendering against the ground truth when
# chasing drawing bugs. TRANSPORT is `posh` (posht inside a persistent posh
# roaming session) or `ssh` (plain `ssh -t`, no posh in the loop — the
# reference render). HOST is the remote ([user@]host); extra ARGS go to posht.
# posht is cross-built and scp'd to /tmp/posht-rec on HOST each run; poshterity
# records its launch over the transport (`poshterity record --via`).
# Run it in the terminal you want to test (e.g. kitty); the session is teed
# live AND recorded by poshterity. Quit posht normally to finalize the file.
# Usage: just debug-record-posht posh box   /   just debug-record-posht ssh box
# Then diff the two .castx (e.g. `poshterity` replay/dump). Debug-only; the
# hermetic gate is build-rust/build-go.
[group("debug")]
debug-record-posht transport host *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    case '{{ transport }}' in
      # posh: record posht inside a persistent posh session (host:posht) — the
      # per-frame mode-sync path where the drawing bugs live.
      posh) via=posh; target='{{ host }}:posht' ;;
      # ssh: plain `ssh -t`, no posh in the loop — the ground-truth render.
      ssh) via=ssh; target='{{ host }}' ;;
      *)
        echo "debug-record-posht: transport must be 'posh' or 'ssh', got '{{ transport }}'" >&2
        exit 64
        ;;
    esac
    # Build the client tools: posh (the roaming transport, used by --via posh)
    # and poshterity (the recorder). target/debug supplies both on PATH.
    nix develop --command cargo build -p posh -p poshterity
    export PATH="$PWD/target/debug:$PATH"
    # Deploy posht to a stable remote path (static Go binary, cross-built for the
    # host's arch). poshterity then records its launch over the transport, so the
    # recorder stays pure — it never touches go/scp itself.
    remote=/tmp/posht-rec
    nix shell nixpkgs#go --command \
      ./posht/run-remote.sh --deploy "$remote" '{{ host }}' >&2
    out="$PWD/posht-{{ transport }}-$(date +%Y%m%dT%H%M%S).castx"
    echo ">> recording posht over '{{ transport }}' on '{{ host }}' -> $out" >&2
    # posht/ssh exiting non-zero (posht's own exit code, or ^C teardown) must
    # NOT fail the recipe — the recording is the artifact. Only a missing/empty
    # output file is a real failure.
    set +e
    poshterity record --out "$out" --via "$via" --host "$target" -- "$remote" {{ ARGS }}
    rc=$?
    set -e
    if [ ! -s "$out" ]; then
      echo ">> recording failed (rc=$rc, no output written)" >&2
      exit "$rc"
    fi
    if [ "$rc" -ne 0 ]; then
      echo ">> note: posht/ssh exited $rc (normal on quit/^C); recording still written" >&2
    fi
    echo ">> wrote recording: $out" >&2
    echo ">> diff a posh vs ssh capture to localize the drawing bug" >&2
