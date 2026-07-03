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
    # ./conformist.nix, the single config source (the store-pinned git hooks
    # eval it too — there is no committed conformist.toml).
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

build: build-nix build-rust build-go build-palette build-toolset

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

# Hermetic posh-palette build (the command-palette renderer, RFC 0005).
[group("build")]
build-palette:
    # The static posh-palette binary. Sources posh-palette/ only; buildGoModule's
    # checkPhase runs `go test ./...` (version + protocol tests). Also realized
    # by build-toolset via poshToolset; this is the focused dev-loop signal.
    nix build -L --show-trace ".#posh-palette" -o result-posh-palette

# The default output: the full posh toolset (github #73). ./result
# aggregates posh + posh-server + poshterity + posht + posh-palette so a bare
# `nix build` yields the whole set. Also realizes the standalone .#poshterity
# output (`nix run .#poshterity`) so the merge gate exercises it. Cheap once
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
# commit, push the branch (so the bump lands on master, not just the tag),
# sign+push+verify a v<sem> tag, and create the GitHub release with the
# changelog as the body. The bump+commit is idempotent: skipped when
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

    # Land the release-bump commit on the branch, not just the tag. `just tag`
    # pushes only the tag; without this, origin/master's version.env never
    # advances, so the bump lives only on the tag commit (which later worktree
    # merges orphan) and `posh version` keeps reporting the pre-bump value. Runs
    # before tagging so a non-fast-forward (origin/master moved) aborts the
    # release instead of cutting a tag off a stale master.
    git push origin "$branch"

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

# Prove SSH agent forwarding end-to-end (FDR 0004): run the #[ignore]'d agent
# E2E tests in remote/server.rs, in three layers — the synthetic byte
# round-trip; the REAL ssh-agent `ssh-add -l` round-trip (in-thread
# server_loop); and the REAL detached `posh server -A` PROCESS forwarding to an
# in-process pump client. Each loads a real ssh-agent and asserts the key
# round-trips through the forwarded socket. Needs the posh binary plus
# ssh-keygen/ssh-agent/ssh-add, absent from the hermetic sandbox (hence
# #[ignore]). Debug-only; the hermetic gate is build-rust.
[group("debug")]
debug-agent-e2e:
    nix develop --command cargo test -p posh --bin posh -- --ignored agent_forward --nocapture

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

# Verify the escape-to-shell overlay (FDR 0008) over a LOCAL loopback
# server+client pair with freshly-built worktree binaries. Runs your $SHELL in
# the session; cd somewhere, then press Ctrl-^ then the escape key (default 's')
# to drop into a transient shell overlay in that dir — `pwd` should match, and
# `exit` returns you to the live session. CMD overrides the escape command
# (e.g. CMD='sc exec', or CMD=env to dump the overlay's environment); ESCKEY
# remaps the trigger sub-key. The server reads POSH_ESCAPE_CMD and the client
# reads POSH_ESCAPE_KEY; on a loopback pair both inherit this recipe's env.
# Run it in the terminal you want to test; detach with Ctrl-^ then "." .
# Debug-only; the hermetic gate is build-rust.
[group("debug")]
debug-verify-escape cmd="" esckey="":
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    nix develop --command cargo build -p posh
    posh=target/debug/posh
    fifo=$(mktemp -u); mkfifo "$fifo"; trap 'rm -f "$fifo"' EXIT
    export POSH_ESCAPE_CMD='{{ cmd }}'
    export POSH_ESCAPE_KEY='{{ esckey }}'
    "$posh" server new -4 >"$fifo" &
    read -r _ _ port key < <(grep -m1 '^POSH CONNECT ' "$fifo")
    echo ">> connecting client to 127.0.0.1:$port — cd somewhere, then press Ctrl-^ '${POSH_ESCAPE_KEY:-s}'" >&2
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

# --- debug: live-session triage (a wedged roaming session) -----------------
# Read-only diagnostics for a posh session that has stopped updating on a
# remote client. The roaming server (remote/server.rs) owns the PTY directly
# (mosh-server style) and syncs dump_vt frames over encrypted UDP; it has NO
# local session-daemon socket, so triage works from the process table, the
# kernel UDP table, and /proc. None of these recipes mutate anything.

# Snapshot every posh/posh-server process for the current user with its state
# flags (STAT: R/S/D/T, + for foreground), elapsed seconds, CPU%, and kernel
# wait channel (WCHAN). A wedged server reads as one of: D (stuck in an
# uninterruptible syscall), high pcpu (spinning), or S with a poll/recv wchan
# (idle — waiting on a client whose acks never arrive). Pair with
# debug-posh-sockets to map pid<->UDP port, then debug-posh-proc-state <pid>.
[group("debug")]
debug-posh-procs:
    #!/usr/bin/env bash
    set -euo pipefail
    out="$(nix shell nixpkgs#procps --command \
      ps -o pid,ppid,stat,etimes,pcpu,wchan:24,args -u "$(id -u)")"
    echo "$out" | head -n1
    # Filter ps output captured BEFORE this grep ran, so the grep/nix helpers
    # aren't in the snapshot. Drop our own recipe line defensively.
    echo "$out" | grep -i posh | grep -v -e 'debug-posh' -e 'grep -i' \
      || echo "(no posh processes for uid $(id -u))"

# Map posh sockets to pids: the mosh-style UDP transport listeners (one live
# server per remote client) and any local session-daemon unix sockets. Then
# list the socket-dir candidates (POSH_DIR | XDG_RUNTIME_DIR/posh/<group> |
# {TMPDIR,/tmp}/posh-<uid>) with the daemon .log files. The ss view is
# env-independent (it reads the kernel); the dir listing reflects THIS shell's
# env, which may differ from the daemons'. Read-only; pair with debug-posh-procs.
[group("debug")]
debug-posh-sockets:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "== ss -nap (udp + unix, posh only) =="
    nix shell nixpkgs#iproute2 --command ss -nap 2>/dev/null \
      | grep -i posh || echo "(no posh sockets)"
    echo
    echo "== socket-dir candidates =="
    uid="$(id -u)"
    for base in \
      "${POSH_DIR:-}" \
      "${XDG_RUNTIME_DIR:-/run/user/$uid}/posh" \
      "${TMPDIR:-}/posh-$uid" \
      "/tmp/posh-$uid"; do
      [ -n "$base" ] && [ -d "$base" ] || continue
      echo "-- $base"
      find "$base" -maxdepth 2 \
        -printf '%M %u %TY-%Tm-%Td %TH:%TM %s %p\n' 2>/dev/null | sort -k6
    done

# Deep read-only kernel state for ONE posh pid (from debug-posh-procs): its
# wait channel + stack (blocked in which syscall?), state, voluntary vs
# nonvoluntary context-switch counters (parked vs spinning), and fd table
# (which UDP socket + which PTY master it holds). This is how you localize
# *where* a wedged roaming server is stuck. Pure /proc reads — the stack may be
# empty if kptr/yama restricts it, but wchan/status/fd are always readable for
# your own process. Usage: just debug-posh-proc-state 12345
[group("debug")]
debug-posh-proc-state pid:
    #!/usr/bin/env bash
    set -euo pipefail
    p='{{ pid }}'
    [ -d "/proc/$p" ] || { echo "no such pid: $p" >&2; exit 1; }
    echo "== cmdline =="; tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null; echo
    echo "== wchan =="; cat "/proc/$p/wchan" 2>/dev/null; echo
    echo "== stack =="; cat "/proc/$p/stack" 2>/dev/null || echo "(stack unreadable)"
    echo "== status =="
    grep -E '^(State|Threads|VmRSS|voluntary_ctxt_switches|nonvoluntary_ctxt_switches|SigQ|SigPnd|SigBlk):' \
      "/proc/$p/status" 2>/dev/null || true
    echo "== fds (sockets, ptys, pipes) =="
    ls -l "/proc/$p/fd" 2>/dev/null || echo "(fd dir unreadable)"

# Liveness probe for ONE posh pid: is its poll loop still cycling, or frozen?
# Samples the context-switch counters and WCHAN/State twice across SECS (default
# 3s) and prints the delta. A live-but-idle roaming server still wakes on its
# heartbeat/poll timeout, so dvol > 0 means the event loop is alive (the wedge
# is then on the network/peer side — acks not arriving, peer forgotten, sends
# stopped); dvol == 0 with State S means genuinely parked (nothing to do — no
# PTY output, no datagrams). Also lists child processes (the session shell): a
# live shell child confirms the session itself is intact. Read-only.
# Usage: just debug-posh-proc-sample 12345 [secs]
[group("debug")]
debug-posh-proc-sample pid secs="3":
    #!/usr/bin/env bash
    set -euo pipefail
    p='{{ pid }}'
    [ -d "/proc/$p" ] || { echo "no such pid: $p" >&2; exit 1; }
    read_vol() { awk -F'\t' '/^voluntary_ctxt_switches:/{print $2}' "/proc/$p/status"; }
    read_nonvol() { awk -F'\t' '/^nonvoluntary_ctxt_switches:/{print $2}' "/proc/$p/status"; }
    v0="$(read_vol)"; n0="$(read_nonvol)"; w0="$(cat /proc/$p/wchan)"
    sleep '{{ secs }}'
    v1="$(read_vol)"; n1="$(read_nonvol)"; w1="$(cat /proc/$p/wchan)"
    st="$(awk -F'\t' '/^State:/{print $2}' /proc/$p/status)"
    echo "pid $p  State=$st"
    echo "  voluntary_ctxt_switches:    $v0 -> $v1   (delta $((v1 - v0)) over {{ secs }}s)"
    echo "  nonvoluntary_ctxt_switches: $n0 -> $n1   (delta $((n1 - n0)))"
    echo "  wchan: $w0 -> $w1"
    if [ "$((v1 - v0))" -gt 0 ]; then
      echo "  => event loop ALIVE (waking on heartbeat/poll); wedge is peer/network-side"
    else
      echo "  => parked: no wakeups in {{ secs }}s (idle, or genuinely stuck)"
    fi
    echo "== child processes (session shell) =="
    nix shell nixpkgs#procps --command ps --ppid "$p" -o pid,stat,etimes,wchan:20,args \
      || echo "(no children — shell may have exited)"

# Trigger a one-shot SIGUSR2 transport-state dump from a running roaming posh
# server OR client (pid from debug-posh-procs) and print the new line. The
# process appends a snapshot of its live transport state — peer address,
# last-heard/last-send ages, acked-vs-current frame — to $POSH_DEBUG_LOG if it
# was set, else <runtime>/posh/posh-<role>-<pid>.log. Own process, no sudo. This
# is the on-demand introspection a wedged session needs (remote/diag.rs; see the
# SIGNALS section of posh-server(1)/posh-client(1)). Usage: just debug-posh-dump 12345
[group("debug")]
debug-posh-dump pid:
    #!/usr/bin/env bash
    set -euo pipefail
    pid='{{ pid }}'
    [ -d "/proc/$pid" ] || { echo "no such pid: $pid" >&2; exit 1; }
    kill -USR2 "$pid"
    sleep 0.3
    uid="$(id -u)"
    # Default-on logging (#83) writes the per-pid sink under the LOG dir (the
    # SIGUSR2 dump reuses it), no longer only the socket-dir. Search the same
    # precedence as session::resolve_log_base, then the socket-dir (the dump's
    # own fallback), then an explicit POSH_DEBUG_LOG. A pid's sink is exactly one.
    dirs=(
      ${POSH_LOG_DIR:+"$POSH_LOG_DIR"}
      ${XDG_LOG_HOME:+"$XDG_LOG_HOME/posh"}
      ${XDG_STATE_HOME:+"$XDG_STATE_HOME/posh/log"}
      "${POSH_DIR:-${XDG_RUNTIME_DIR:-/run/user/$uid}/posh}"
    )
    f=""
    for d in "${dirs[@]}"; do
      [ -d "$d" ] || continue
      cand="$(ls -t "$d"/posh-*-"$pid".log 2>/dev/null | head -n1 || true)"
      [ -n "$cand" ] && { f="$cand"; break; }
    done
    if [ -z "${f:-}" ] && [ -n "${POSH_DEBUG_LOG:-}" ] && [ -f "${POSH_DEBUG_LOG}" ]; then
      f="${POSH_DEBUG_LOG}"
    fi
    if [ -z "${f:-}" ] || [ ! -f "$f" ]; then
      echo "no dump file for pid $pid (searched: ${dirs[*]}; POSH_DEBUG_LOG=${POSH_DEBUG_LOG:-unset})" >&2
      exit 1
    fi
    echo ">> $f"
    tail -n1 "$f"

# Print the most recent wedge-forensics bundle for a roaming posh CLIENT pid.
# A bundle is written on an apply-stall (ReackAndWait), and on demand via the
# SIGUSR2 dump or the "Dump wedge forensics" palette command. The .txt carries
# the verdict (SHORT_BASE prefix+suffix>applied = the #90 wedge; LEN_OK = #94
# content divergence); the sibling .applied/.diff hold the raw bytes for an
# offline apply_diff re-run. See remote/diag.rs::capture_forensics.
# Usage: just debug-posh-forensics 12345
[group("debug")]
debug-posh-forensics pid:
    #!/usr/bin/env bash
    set -euo pipefail
    pid='{{ pid }}'
    uid="$(id -u)"
    base="${POSH_DIR:-${XDG_RUNTIME_DIR:-/run/user/$uid}/posh}"
    txt="$(ls -t "$base"/posh-forensic-client-"$pid"-*.txt 2>/dev/null | head -n1 || true)"
    if [ -z "${txt:-}" ] || [ ! -f "$txt" ]; then
      echo "no forensic bundle for pid $pid under $base (none captured -- not wedged?)" >&2
      exit 1
    fi
    echo ">> $txt"
    cat "$txt"
    echo "== raw byte siblings (for an offline apply_diff re-run) =="
    ls -l "${txt%.txt}.applied" "${txt%.txt}.diff" 2>/dev/null || true

# Start a detached loopback roaming server (worktree debug binary) running a
# long `sleep`, for HEADLESS transport debugging — e.g. exercising
# debug-posh-dump without a tty. Prints the server's CONNECT line, then the
# server double-forks and detaches; find its pid with debug-posh-procs and tear
# it down with `kill`. Debug-only; the hermetic gate is build-rust.
[group("debug")]
debug-posh-server-smoke secs="600":
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    nix develop --command cargo build -q -p posh
    # Foreground returns once the daemon has double-forked away (prints CONNECT +
    # "[posh-server detached]"); the detached server runs `sleep {{ secs }}`.
    # POSH_DEBUG_LOG is cleared so the SIGUSR2 dump exercises its own default
    # per-pid sink (<runtime>/posh/posh-server-<pid>.log), not a pre-armed file.
    env -u POSH_DEBUG_LOG target/debug/posh server new -4 -- sleep '{{ secs }}'

# Spin up ONE fresh roaming server for hand-testing the command palette, with NO
# ambient paths. Kills any lingering posh servers, builds the hermetic toolset
# (absolute /nix/store paths; posh + posh-palette co-installed so Ctrl-^ finds the
# renderer next to itself), starts the server running $SHELL (fish) detached, and
# prints the exact client command to paste — eliminating the stale-binary and
# port-race confusion of leaning on target/debug or ./result. Tear down later
# with: pkill -f '[p]osh server new'. Debug-only; the hermetic gate is build-rust.
[group("debug")]
debug-posh-palette-demo:
    #!/usr/bin/env bash
    set -euo pipefail
    cd '{{ justfile_directory() }}'
    # The [p] character class stops pkill from matching its own command line
    # (the classic self-kill footgun).
    pkill -f '[p]osh server new' || true
    sleep 0.5
    posh="$(nix build .#default --no-link --print-out-paths)/bin/posh"
    fish="$(command -v fish || echo /bin/sh)"
    out=$(SHELL="$fish" env -u POSH_DEBUG_LOG POSH_SERVER_NETWORK_TMOUT=3600 \
        setsid "$posh" server new -4)
    echo "$out"
    read -r port key < <(awk '/POSH CONNECT/ {print $3, $4}' <<<"$out")
    echo
    echo "=== paste this to connect (absolute store path, no env juggling): ==="
    echo "POSH_KEY=$key $posh client -4 127.0.0.1 $port"

# Scan a posh server/client debug log for STALLS: consecutive records whose
# leading [epoch-ms] timestamps jump by more than THRESH ms (default 5000). The
# periodic [stats] flush fires every 1-3s while the loop is alive, so one large
# gap == the event loop was parked (a roaming wedge / no-paint freeze) for that
# long. Pass a pid (resolves the default per-pid sink under <runtime>/posh) or an
# explicit log path. Prints each gap with its two boundary records (last srtt /
# outstanding / retransmit before the freeze are right there). Read-only.
# Usage: just debug-posh-log-gaps 12345 [thresh_ms]   or   ... /path/to.log [thresh_ms]
[group("debug")]
debug-posh-log-gaps target thresh="5000":
    #!/usr/bin/env bash
    set -euo pipefail
    t='{{ target }}'
    if [ -f "$t" ]; then
      f="$t"
    else
      uid="$(id -u)"
      base="${POSH_DIR:-${XDG_RUNTIME_DIR:-/run/user/$uid}/posh}"
      f="$base/posh-server-$t.log"
      [ -f "$f" ] || f="$base/posh-client-$t.log"
    fi
    [ -f "$f" ] || { echo "no log for '$t' (tried it as a path, then posh-{server,client}-$t.log under $base)" >&2; exit 1; }
    echo ">> $f"
    awk -v thresh='{{ thresh }}' '
      substr($0,1,1) == "[" {
        rb = index($0, "]")
        if (rb < 3) next
        ts = substr($0, 2, rb-2) + 0
        if (have && ts - prev > thresh) {
          dt = ts - prev
          ngaps++
          printf "GAP %d ms (%.1fs, ~%.1f min) ending at line %d:\n", dt, dt/1000, dt/60000, NR
          printf "  before: %s\n", prevline
          printf "  after:  %s\n", $0
        }
        prev = ts; prevline = $0; have = 1
      }
      END {
        if (have) printf "(scanned %d records, last ts %d; %d gap(s) over %d ms)\n", NR, prev, ngaps+0, thresh
      }
    ' "$f"

# Pinpoint a posh transport BLACKOUT in a server log when the event loop never
# stalled (debug-posh-log-gaps finds nothing) but the user still saw a no-paint
# freeze: the client became unreachable, so the server kept tx-ing frames that
# were never acked. That shows up as a burst in the cumulative `retransmit`
# counter + `outstanding` (unacked frames) climbing while srtt/rto inflate, then
# a collapse on recovery. Reports every record whose retransmit grew by >RT
# (default 40) since the prior record, or whose outstanding is >=OUT (default 8),
# and the single worst retransmit jump. Read-only.
# Usage: just debug-posh-log-loss 12345 [retransmit_delta] [outstanding_min]
[group("debug")]
debug-posh-log-loss target rt="40" out="8":
    #!/usr/bin/env bash
    set -euo pipefail
    t='{{ target }}'
    if [ -f "$t" ]; then
      f="$t"
    else
      uid="$(id -u)"
      base="${POSH_DIR:-${XDG_RUNTIME_DIR:-/run/user/$uid}/posh}"
      f="$base/posh-server-$t.log"
      [ -f "$f" ] || f="$base/posh-client-$t.log"
    fi
    [ -f "$f" ] || { echo "no log for '$t' (tried it as a path, then posh-{server,client}-$t.log under $base)" >&2; exit 1; }
    echo ">> $f"
    awk -v rt='{{ rt }}' -v outmin='{{ out }}' '
      substr($0,1,1) == "[" {
        rb = index($0, "]"); ts = substr($0, 2, rb-2) + 0
        r=""; o=""; s="";
        for (i=1;i<=NF;i++) {
          if ($i ~ /^retransmit=/) r = substr($i,12)+0
          else if ($i ~ /^outstanding=/) o = substr($i,13)+0
          else if ($i ~ /^srtt=/) s = $i
        }
        if (r == "") next
        dr = (haveR ? r - prevR : 0)
        flag = ""
        if (haveR && dr > rt) flag = flag sprintf(" retransmit+%d", dr)
        if (o != "" && o+0 >= outmin) flag = flag sprintf(" outstanding=%d", o)
        if (flag != "") printf "L%d ts=%d %s%s  (retransmit=%d)\n", NR, ts, s, flag, r
        if (haveR && dr > maxdr) { maxdr=dr; maxln=NR; maxts=ts }
        prevR=r; haveR=1
      }
      END { printf "(max retransmit jump = +%d at line %d, ts %d)\n", maxdr+0, maxln+0, maxts+0 }
    ' "$f"

# Explain a high posh retransmit rate by probing the underlying network path: is
# the Tailscale link to a roaming peer DIRECT or DERP-relayed, and what's its real
# latency/loss? `tailscale ping` reports `via DERP(region)` vs `via <ip>:<port>`
# (direct) — a relayed path adds a WAN hop and is the usual cause of 100ms+ srtt
# and steady UDP loss. Also prints the peer's `tailscale status` line (endpoint /
# relay) and the host's UDP error counters + the posh socket's own drop count, so
# socket-level receive drops (RcvbufErrors) are distinguished from path loss.
# Read-only. PEER is the roaming client's 100.x address (from debug-posh-dump's
# `remote=`); PORT defaults to the server's 60001. Usage: just debug-posh-net 100.x.y.z
[group("debug")]
debug-posh-net peer port="60001":
    #!/usr/bin/env bash
    set -euo pipefail
    peer='{{ peer }}'
    echo "== tailscale status (peer line) =="
    tailscale status | grep -F "$peer" || echo "(peer $peer not listed in tailscale status)"
    echo
    echo "== tailscale ping $peer (stops at first DIRECT, else shows DERP) =="
    timeout 20 tailscale ping "$peer" || true
    echo
    echo "== posh UDP socket (drops/backlog, port {{ port }}) =="
    ss -uapmi "sport = :{{ port }}" || true
    echo
    echo "== host UDP error counters =="
    nstat -a -s -z 2>/dev/null | grep -i udp || true

# Quantify the real loss+latency on the direct Tailscale path to a roaming peer
# (the wireguard tunnel that carries posh's UDP) with ICMP, and report THIS host's
# NAT/firewall posture via `tailscale netcheck`: UDP reachability, hard-NAT
# (MappingVariesByDestIP), and whether any port-mapping protocol (UPnP/NAT-PMP/PCP)
# is available. Hard NAT + no port-mapping == fragile, loss-prone direct paths —
# the usual root cause of a high posh retransmit rate even when the link reports
# "direct". Read-only. Usage: just debug-posh-pathloss 100.x.y.z [count]
[group("debug")]
debug-posh-pathloss peer count="20":
    #!/usr/bin/env bash
    set -euo pipefail
    peer='{{ peer }}'
    echo "== ping (loss/rtt on the tailscale path, {{ count }} probes) =="
    ping -c '{{ count }}' -i 0.25 -w 25 "$peer" || true
    echo
    echo "== tailscale netcheck (NAT / firewall / port-mapping posture) =="
    timeout 25 tailscale netcheck || true

# posh#100 background-bleed: the BCE-on-scroll test. posh-term fills scrolled-in
# lines with the active pen's background (scroll_up_n -> blank_style, terminal.rs),
# but kitty does NOT background-color-erase that scroll — so the SAME bytes render
# clean in pure kitty but leave a stuck steel-blue line after a round-trip through
# posh (posh-term models the BCE, new_frame paints it into the client). Two cases,
# NO forced resize (natural width, so no wrap confound):
#   E1  scroll a DECSTBM region with the bg pen STILL ACTIVE  -> BCE fills the new
#       bottom line with steel-blue in posh-term's model
#   E2  identical, but reset the pen (ESC[0m) BEFORE the scroll -> default pen, no
#       spurious bg (the control)
# Prediction: pure kitty shows BOTH clean; through posh, E1's scrolled-in line
# (row 9) is steel-blue and E2's (row 19) is not. That difference IS #100.
# Cat .tmp/bleedscroll.raw once in a raw kitty and once inside posh; report each.
[group("debug")]
debug-posh-bleed-scroll:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p .tmp
    out=.tmp/bleedscroll.raw
    E=$'\033'
    BG="${E}[0;48;2;38;79;120m"
    R="${E}[0m"
    {
      printf '%s[2J' "$E"
      printf '%s[2;1H%sE1: DECSTBM region rows 5-9, scroll with the bg pen ACTIVE (BCE).' "$E" "$R"
      printf '%s[3;1H%s    After the scroll, row 9 should be BLANK. Steel-blue there = #100.' "$E" "$R"
      printf '%s[5;9r' "$E"                                              # region rows 5-9
      printf '%s[9;1H%s  E1 bottom-margin fill (scrolls up)  \r\n%s%s[r' "$E" "$BG" "$R" "$E"
      printf '%s[12;1H%sE2 (control): same, but ESC[0m BEFORE the scroll (default pen).' "$E" "$R"
      printf '%s[13;1H%s    Row 19 should be BLANK in every terminal.' "$E" "$R"
      printf '%s[15;19r' "$E"                                            # region rows 15-19
      printf '%s[19;1H%s  E2 bottom-margin fill (scrolls up)  %s\r\n%s[r' "$E" "$BG" "$R" "$E"
      printf '%s[22;1H%sE1=row9 (expect bleed via posh only)   E2=row19 (expect always clean)' "$E" "$R"
      printf '%s[24;1H' "$E"
    } > "$out"
    echo "wrote $out ($(wc -c < "$out") bytes) at the terminal's natural size (no resize)"
    echo "test A (pure kitty): cat $out ; sleep 20 ; reset   -> expect BOTH rows 9 and 19 clean"
    echo "test B (inside posh): cat $out ; sleep 20 ; reset   -> expect row 9 steel-blue, row 19 clean"

# posh#100 background-bleed: report a terminal's `bce` (background-color-erase)
# terminfo capability — the crux of the bug. kitty deliberately omits bce; the
# xterm lineage has it. mosh's renderer gates its erase optimization on the
# CLIENT's bce (Display::can_use_erase = has_bce || default-pen); posh-term's
# model BCEs unconditionally. Read-only. Usage: just debug-term-bce xterm-kitty
[group("debug")]
debug-term-bce term:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v infocmp >/dev/null 2>&1; then
      echo "infocmp not on PATH" >&2; exit 1
    fi
    if ! infocmp -1 '{{ term }}' >/dev/null 2>&1; then
      echo "{{ term }}: not in the terminfo DB"; exit 0
    fi
    if infocmp -1 '{{ term }}' | grep -qE '^\s*bce,'; then
      echo "{{ term }}: bce = YES (BCE — scroll/erase keep the pen background)"
    else
      echo "{{ term }}: bce = no (non-BCE — scroll/erase fall back to default background)"
    fi

# posh#100: render the BCE-on-scroll synthetic through the server->client round
# trip (poshterity render) into the exact CLIENT tty bytes a roaming posh client
# would paint — so #100 reproduces in a PURE kitty with NO live posh session, and
# (post-fix) this same file must render clean. Writes .tmp/bleedscroll-client.raw.
# SIZE is the client width (default 80x24). Rebuild-free: uses debug cargo.
[group("debug")]
debug-posh-bleed-render size="80x24":
    #!/usr/bin/env bash
    set -euo pipefail
    raw="$PWD/.tmp/bleedscroll.raw"
    [ -f "$raw" ] || { echo "missing $raw — run: just debug-posh-bleed-scroll" >&2; exit 1; }
    out="$PWD/.tmp/bleedscroll-client.raw"
    nix develop --command cargo run -q -p poshterity -- \
      render --raw "$raw" --size '{{ size }}' > "$out"
    echo "wrote $out ($(wc -c < "$out") bytes) — the CLIENT tty bytes for bleedscroll.raw"
    echo
    echo "cat it into a PURE kitty (NO posh):  cat $out ; sleep 20 ; reset"
    echo "row 9 (E1): steel-blue = the #100 bleed (pre-fix); CLEAN = fixed (ADR 0005)."
    echo "row 8 stays steel-blue either way — the fill text that scrolled up (legit)."
