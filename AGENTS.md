# posh — repository guide

POSH is **the portable shell**: terminal sessions that roam across networks
(mosh-style encrypted-UDP transport) and persist across disconnects
(zmx-style session daemon), addressed through one scp-style `host:session`
namespace. This file orients an agent working in the repo; the README is the
human-facing introduction.

## Layout

This is a Cargo workspace plus a vendored C++ reference tree and a Go helper.

```
crates/
  posh-term/   dependency-free, 100%-safe-Rust VT100/VT220+ terminal emulator
               (#![forbid(unsafe_code)]; frozen public API in src/lib.rs)
  posh/        the posh binary — session daemon, remote transport, CLI.
               All libc/PTY FFI lives here, never in posh-term.
  posh-rec/    deterministic step-ratcheted terminal recorder/replayer built
               on posh-term (phase 0 landed; roadmap is the #56 epic)
doc/           scdoc man-page SOURCES — posh(1), posh-server(1),
               posh-client(1), posh(7). Compiled by the flake; see below.
docs/          ADRs (docs/decisions/), RFCs (docs/rfcs/), feature records
               (docs/features/, the FDRs), plans (docs/plans/), manual tests
posht/         standalone interactive terminal-capability test (Go/Bubble Tea)
zz-mosh/       the vendored C++ mosh reference tree (the porting reference);
               has its OWN justfile for host-lane recipes: `just zz-mosh/<r>`
```

`posh-server` is the same binary as `posh` (a `bin/posh-server -> posh`
symlink); invoked under that name, argv[0] routes to the `server`
subcommand. The original zmx (Zig) lives in its own repository.

## Build & test

The hermetic nix lanes are the source of truth; the justfile wraps them.

```
just                        # default: validate lint build test (the CI gate)
nix build .#posh            # hermetic build + cargo test --workspace
just build-rust             # the .#posh lane via the justfile
just debug-cargo test --workspace   # fast in-worktree dev-loop (not hermetic)
just lint-doc               # compile doc/*.scd, fail on scdoc parse errors
nix build .#mosh            # the C++ reference (.#mosh); just build-nix
nix build .#posht           # the Go capability test; just build-go
```

`merge-this-session`'s pre-merge hook runs `just` (= validate+lint+build+
test) — that IS the CI lane. Do not redundantly run `just` before merging;
a cheap `go build`/per-crate `cargo build` to check compilation is fine.

The `.#posh` checkPhase runs `cargo test --workspace`, so every workspace
crate's tests (posh-term, posh, posh-rec) gate merges. The C++ `.#mosh`
check runs only the sandbox-safe subset; the tmux-driven emulation tests
SKIP in the sandbox (tracked: wiring them in is #62; the macOS host failure
is #2).

## Conventions (eng + repo-specific)

This repo follows the eng workspace conventions. The authoritative source is
the `eng-*(7)` manpages — read them with `man eng-versioning`,
`man eng-manpages`, `man eng-design_patterns-justfile`, etc. Repo specifics:

- **Versioning (eng-versioning(7)):** `version.env` (`POSH_VERSION`) at the
  repo root is the single source of truth. The crate manifests carry an inert
  `0.0.0` placeholder (`version.workspace = true`); each crate's `build.rs`
  flows `POSH_VERSION` in at compile time, so there is no `Cargo.toml` version
  to keep in lockstep. `just bump-version <sem>` rewrites only `version.env`;
  `just tag` / `just release` cut signed `vX.Y.Z` releases. NOTE: `mosh`
  (vendored upstream, `1.4.0`) and `posht` (Go) keep their own independent
  version lineages — do not fold them into `POSH_VERSION`. `posh-rec` also
  carries its own lineage (it's meant to be a separable tool). `version.env`
  conflicts on rebase resolve to the **higher semver** via a custom git merge
  driver (`scripts/version-merge`, declared in `.gitattributes`); register it
  per-clone with `just install-merge-driver` (the sweatfile `[hooks].create`
  does this for fresh spinclass worktrees).
- **Man pages (eng-manpages(7)):** hand-written scdoc under `doc/*.scd`,
  compiled into the posh package by the flake's `postInstall` (a generic
  section-deriving loop). `man posh` etc. resolve via a consumer's
  home-manager `programs.man`. Lint locally with `just lint-doc`. scdoc
  pitfall: a line starting with `[` collides with table syntax (escape as
  `\[`), and a literal `*` inside `_italic_` is a parse error.
- **Formatting + linting (conformist(7)):** conformist (the treefmt
  successor) is the formatter+linter gate. `conformist.nix` (eng preset +
  clang-format/nixfmt/shfmt) is the source of truth; the flake's `formatter`
  (`nix fmt` / `just codemod-fmt`, repair mode) and `checks.formatting`
  (`just lint-fmt`, read-only) drive it. `conformist.nix` is the single config
  source — there is **no** committed `conformist.toml`; the git hooks are
  store-pinned wrappers (`conformist-pre-commit` / `conformist-repair`,
  conformist#47/#51) that the flake exposes from
  `conformistEval.config.build.preCommit` + a hand-rolled `--commit --amend`
  sibling (conformist#54 tracks folding the repair wrapper into the module),
  each baking its own `/nix/store` config — so they format with the same pinned
  toolchain as `nix fmt`, never silent-skipping a file type the ambient PATH
  lacks. The impure git-state lane (agents-md, git-remotes, sweatfile, …) runs
  via `just lint-worktree`. The sweatfile wires the spinclass hooks: `pre-commit`
  (`conformist-pre-commit`, format at authoring time) and `repair`
  (`conformist-repair`, fold fixes in before the pre-merge verify gate); both
  live on the devShell PATH, and a fresh `sc start`/`sc resume` installs the
  pre-commit hook.
- **Justfile (eng-design_patterns-justfile(7)):** verb-noun leaf recipes
  under bare aggregates; `[group(...)]` attributes; `default` is the first
  recipe. Add release/maintenance recipes to the `maintenance` group.
- **Docs:** significant designs get a record under `docs/` — ADR for
  architecture decisions, RFC for wire/file-format contracts, FDR for
  user-facing features. The target grammar + capability table is RFC 0001;
  the FDRs (0001-0005) cover the namespace, takeover, mosh-parity, ssh agent
  forwarding, and scrollback sync.

## Key design facts (load-bearing, verified)

- **Session lifecycle:** a session is owned by a double-forked daemon, not
  any client. Detach/disconnect/roam leave it running; the daemon exits
  (killing its process group, propagating the shell's exit code) only when
  the shell itself exits. `crates/posh/src/session/daemon.rs`.
- **posh-term is pure state:** feed PTY bytes via `Terminal::process`, read
  the screen via `screen()`/`dump_vt()`/`dump_text()`, drain query replies
  via `take_responses()`. `generation()` bumps on every visible change;
  `mid_escape()` marks escape-sequence boundaries. The public API (lib.rs)
  is frozen: callers may ADD items, never remove/change signatures.
- **Stream parsing (ADR-0003):** multi-byte structures (escape sequences,
  framed records) MUST be reassembled across read boundaries via a byte-fed
  state machine — never assume a `read()` delivers a whole sequence.

## When working here

- You are almost always in a spinclass worktree (`.worktrees/<name>`).
  Operate only within it; never touch the root git directory. After
  `merge-this-session`, start the next work from the same worktree.
- `nix build` on a dirty tree sees only git-TRACKED files — `git add` new
  files (new crates, new `doc/*.scd`, `version.env`) before building, or the
  sandbox won't see them.
- `direnv reload` does not work mid-session; if the devShell needs new
  packages, ask the user to restart the session.
