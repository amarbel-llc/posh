# posh — repository guide

POSH is **the portable shell**: terminal sessions that roam across networks
(mosh-style encrypted-UDP transport) and persist across disconnects
(zmx-style session daemon), addressed through one scp-style `host:session`
namespace. This file orients an agent working in the repo; the README is the
human-facing introduction.

**This file is deliberately a router, not a manual.** posh documents itself in
four places — man pages, design records, justfile recipe comments, and the
session prompt — each of which is fuller and closer to the code than a summary
here could stay. A summary of any of them is a standing drift generator: it
goes stale the commit after it is written, and it is stale silently, because
nothing checks a prose paragraph against the thing it describes. So the rule
for editing this file is: **before adding a paragraph, find which of the four
homes it belongs in, and put it there instead.** Add here only what has no
home — and then say where it is implemented, so the next reader can leave.

conformist's `agents-md` linter caps this file (40000 characters by default,
a merge-gate check). The cap is the symptom, not the constraint: an
orientation doc that grows unbounded stops being one.

## Where the answers live

| question | go to |
|---|---|
| what does `$POSH_*` do? | `man posh` / `man posh-client` / `man posh-server` — **ENVIRONMENT** documents every variable, **SIGNALS** every dump |
| what does a subcommand do? | the same pages — `posh(1)` has the target grammar, the `ph` front door, and every session/remote command |
| the shape of the whole system | `posh(7)` — namespace, roaming, persistence, takeover, prediction |
| why does a user-facing feature behave like this? | `docs/features/` (FDRs) — the levers, the rollback, the decisions and their dates |
| what is on the wire or in a file? | `docs/rfcs/` — RFC 0001 holds the target grammar and the capability registry, maintained **in place** (a new or retired id edits its table, citing the RFC that changed it) |
| why was it built this way at all? | `docs/decisions/` (ADRs) |
| can I rely on a record today? | `docs/README.md` — the `status` vocabulary, and the rule that status moves in the commit that moves the code |
| what does a `just` recipe do, and why? | `just --list` for the catalogue; the **comment block above the recipe** is its documentation, and it is fuller than any summary of it |
| eng-wide conventions | `man eng-versioning`, `man eng-manpages`, `man eng-design_patterns-justfile`, `man conformist` |

Design records are deliberately not enumerated here — an inline index of a
growing directory drifts on every new record, and agent sessions are handed
the current set with statuses automatically.

## Layout

A Cargo workspace plus a vendored C++ reference tree and two Go helpers.

```
crates/
  posh-term/   dependency-free, 100%-safe-Rust VT100/VT220+ terminal emulator
               (#![forbid(unsafe_code)]; frozen public API in src/lib.rs)
  posh/        the posh binary — session daemon, remote transport, CLI.
               All libc/PTY FFI lives here, never in posh-term.
  posh-proto/  shared frame/display protocol: the Snapshot + new_frame
               renderer, the swappable frame codecs (DumpDiff/MorphDelta),
               the ServerFrame/FrameBody wire types, and the RFC 0001
               capability table. Extracted so poshterity can drive the same
               codecs without a posh→poshterity→posh cycle (#75).
  poshterity/  deterministic step-ratcheted terminal recorder/replayer built
               on posh-term (#56 epic); also hosts the deterministic
               server-frame harness (framereplay, #75)
  posh-build/  the shared build.rs logic every crate's build.rs calls
  mosh-ffi/    C++ FFI characterization tests against zz-mosh (not a default
               workspace member; `just test-mosh-ffi`)
doc/           scdoc man-page SOURCES, compiled by the flake
docs/          ADRs, RFCs, FDRs, plans, manual tests — see docs/README.md
posht/         standalone interactive terminal-capability test (Go/Bubble Tea)
posh-palette/  the command-palette renderer (Go/Bubble Tea v2): a subprocess
               the client drives over a JSON-RPC control channel (RFC 0005)
               and composites onto the session view. Its own Go module.
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

`merge-this-session`'s pre-merge hook runs `just` — that IS the CI lane. Do
not redundantly run `just` before merging; a cheap `go build` / per-crate
`cargo build` to check compilation is fine.

The `.#posh` checkPhase runs `cargo test --workspace`, so every workspace
crate's tests gate merges. The C++ `.#mosh` check runs only the sandbox-safe
subset; the tmux-driven emulation tests SKIP in the sandbox (wiring them in
is #62; the macOS host failure is #2).

## Key design facts

These are **traps** — places where reading the code naively gets it wrong, or
where two things that look interchangeable are not. Each ends with where the
full story lives; this list is not a tour of the system.

- **Two serializers, two contracts** (`posh-term/src/dump.rs`): `dump_vt()`
  targets a freshly built `Terminal` that MAY BE LARGER than the source, so
  it must never derive a position from an assumed height; `dump_vt_flat()`
  targets a REAL tty that may carry mode leftovers, so it emits
  `DRAWABLE_STATE_RESET` first. Swapping one for the other at a call site is
  a bug, not a refactor.
- **Geometry travels UP only:** a client reports its size (`Tag::Init` /
  `Tag::Resize`); nothing ever tells a client the resulting session size.
  `ServerFrame` carries no dimensions, and `Snapshot`'s `rows`/`cols` are
  encode-side only, never serialized — so a client sizes its mirror
  `Terminal` to its own tty, which is the wrong size whenever it is not the
  smallest client. Root of the mismatched-size cursor bugs. ADR 0006 +
  RFC 0012.
- **Multi-client sizing is smallest-wins, and the DAEMON owns it**
  (`min_client_size` / `apply_client_size`, `session/daemon.rs`; tmux
  `window-size smallest`). Every client but the smallest permanently renders
  a session smaller than its own terminal — a steady state, not a transient.
  The roaming server (`remote/server.rs`) is by contrast **single-peer**: its
  `client_size` is one peer's size, not an arbitration.
- **Frames are unconditional daemon-side but NOT universal:** a client gets
  frames iff it advertises `CAP_PROTOCOL_VERSION` in its `Tag::Init`
  capability table — `is_frame_capable` tests for that specific id, not
  merely for a table being present. Without it a client stays on raw
  `Tag::Output` (the old-client skew case, pinned by `daemon.rs`'s
  version-skew tests). `POSH_SESSION_FRAMES` was retired 2026-08-25
  (posh#171) and is ignored; the only rollback to Architecture A is
  `POSH_RELAY=0`.
- **posh-term is pure state, and its API is frozen:** feed PTY bytes via
  `Terminal::process`, read via `screen()` / `dump_vt()` / `dump_text()`,
  drain query replies via `take_responses()`. `generation()` bumps on every
  visible change; `mid_escape()` marks escape-sequence boundaries. Callers
  may ADD to `lib.rs`, never remove or change a signature.
- **Stream parsing (ADR-0003):** multi-byte structures (escape sequences,
  framed records) MUST be reassembled across read boundaries via a byte-fed
  state machine — never assume a `read()` delivers a whole sequence.
- **Session lifecycle:** a session is owned by a double-forked daemon, not
  any client. Detach/disconnect/roam leave it running; the daemon exits
  (killing its process group, propagating the shell's exit code) only when
  the shell itself exits. `session/daemon.rs`; posh(7) PERSISTENCE.
- **The mux endpoint is default-on in both increments, and that is load
  bearing:** M1 (agent forwarding) and M2 (sessions riding the same
  connection as channels) both ship on, with `POSH_MUX=0` /
  `POSH_MUX_SESSIONS=0` as the opt-outs, and every failure falls back
  per-invocation with a warning. So a change that "only affects the mux
  path" affects the default path. FDR 0014 for the feature and its
  decisions, RFC 0011 for the envelope, RFC 0015 for the resume cursor a
  reconnect must carry, `man posh` for every gate's resolved meaning.
- **Durable stream ⇒ a `SessionResume` field** (`remote/resume.rs`,
  RFC 0015): any offset that must stay continuous when the transport is
  rebuilt goes on the cursor. It has no blanket `Default` precisely so the
  compiler forces every reattach site and the wire codec to carry a new
  one — the invariant is structural, not per-stream discipline.
- **A remote attach takes over the terminal FIRST**, before the mux endpoint
  ensure, and hosts its whole establishment — the bootstrap ssh included —
  in a palette-style modal. Off-tty there is no takeover. This is why
  ordering matters around `cmd_ssh_session` / `cmd_ssh`: FDR 0019.
- **Predictor and renderer are orthogonal axes** (`predict/`): the model
  decides WHAT is predicted and hands the renderer a `RenderAdvice`; the
  renderer honors or disregards it. The default `always` model plus
  `POSH_PREDICTION_SHOW=always` means `adaptive` and `always` LOOK identical
  while recording different advice — so "it painted" proves nothing about
  which model is live. `always` also bypasses the RFC 0007 §5.1 safety gate
  by design (transient password glyphs included). FDR 0006; the levers are
  in `man posh-client`.

## Conventions: posh's deltas from eng

The eng conventions are authoritative in the `eng-*(7)` man pages. Read them
there. What follows is only what is specific to this repo.

- **Versioning** (`man eng-versioning`): `version.env` (`POSH_VERSION`) is
  the single source of truth. Crate manifests carry an inert `0.0.0`
  placeholder (`version.workspace = true`) and each `build.rs` flows the
  version in at compile time, so there is no `Cargo.toml` version to keep in
  lockstep. `flow()` also composes `POSH_BUILD` = `<version>+<sha>` — the
  build identity every surface renders, joined there and nowhere else,
  because a version alone does not identify a build (`just
  debug-posh-builds` censuses the several one host runs). `+` is SemVer
  metadata: equality, NO ordering. The **only** independent lineage is the
  vendored `zz-mosh/` tree, which keeps upstream's `1.4.0`; everything else
  flows `POSH_VERSION`, the Go modules included (`posht` via `-ldflags -X`,
  github #71). `version.env`
  rebase conflicts resolve to the **higher semver** via `scripts/version-merge`
  (declared in `.gitattributes`); register it per-clone with `just
  install-merge-driver` (the sweatfile's `[hooks].create` does this for
  fresh worktrees).
- **Man pages** (`man eng-manpages`): hand-written scdoc under `doc/*.scd`,
  compiled by the flake's `postInstall`. Lint with `just lint-doc`. scdoc
  pitfalls: a line starting with `[` collides with table syntax (escape as
  `\[`), and a literal `*` inside `_italic_` is a parse error.
- **Formatting and linting** (`man conformist`): `conformist.nix` is the
  single config source — there is **no** committed `conformist.toml`. `nix
  fmt` / `just codemod-fmt` repair; `just lint-fmt` checks. The impure
  git-state lane (agents-md, git-remotes, sweatfile, clippy, …) is `just
  lint-worktree`; posh's clippy gate is the workspace `cargo clippy
  --all-targets -- -D warnings`. The git hooks are store-pinned wrappers, so
  they format with the same toolchain as `nix fmt` rather than silently
  skipping a file type the ambient PATH lacks.
- **Justfile** (`man eng-design_patterns-justfile`): verb-noun leaf recipes
  under bare aggregates; `[group(...)]` attributes; `default` is first. A
  recipe's comment block is its documentation — write it for the agent who
  will run the recipe without reading its body.
- **Docs:** significant designs get a record — ADR for a decision, RFC for a
  wire or file-format contract, FDR for a user-facing feature. `docs/README.md`
  defines the kinds and the status vocabulary, and the rule that **status
  moves in the commit that moves the code.**

## Debugging

Every triage path is a `debug`-group recipe, and **each recipe's comment
block is its documentation** — what it reads, how to read the output, and the
signature that matters. `just --list` is the catalogue. Do not re-summarize
them here.

The one framing that is not in any recipe: a roaming `posh-server`
(`remote/server.rs`) owns its PTY directly, mosh-server style, and has NO
local session-daemon socket — so a wedged *remote* session is triaged from
the process table, the kernel UDP table, and `/proc`, not from `posh list`.
A wedge looks like `S` in `do_sys_poll`, not `D` or a spin, and both the
server and its shell child stay alive: the transport is stuck, not the
process. Start at `just debug-posh-procs`, then `just debug-posh-dump <pid>`
for the on-demand SIGUSR2 transport snapshot (FDR 0007; also `man
posh-server` SIGNALS).

A live *local* session that emits `↑`/`↓` on the wheel instead of scrolling
is not framed — full write-up in `docs/wheel-scroll-behavior.md`.

## When working here

- You are almost always in a spinclass worktree (`.worktrees/<name>`).
  Operate only within it; never touch the root git directory. After
  `merge-this-session`, start the next work from the same worktree.
- `nix build` on a dirty tree sees only git-TRACKED files — `git add` new
  files (new crates, new `doc/*.scd`, `version.env`) before building, or the
  sandbox won't see them.
- `direnv reload` does not work mid-session; if the devShell needs new
  packages, ask for the session to be restarted.
