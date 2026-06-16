---
status: accepted
date: 2026-06-16
decision-makers: sfriedenberg
---

# Use mosh's C++ as an FFI oracle for differential testing of posh

## Context and Problem Statement

posh is a working full Rust rewrite of mosh (the bug-fix campaign in #34): the roaming UDP transport, scrollback sync, the frame-sync codec seam, and the predictive-echo engine (`crates/posh/src/remote/predict/mosh.rs`, a hand port of mosh's `PredictionEngine`) all run today. But the subsystems where mosh is the *correctness reference* — predictive local echo and terminal-emulation compatibility — carry tricky bugs that are hard to chase purely from the Rust side, because the only thing that says "this is what the output should be" is mosh itself, read as prose. The question this ADR settles: how do we get mosh's behavior as a checkable oracle for those subsystems without restarting the rewrite?

## Decision Drivers

* mosh is the correctness reference for predictive echo and VT emulation; "what should the bytes be?" currently has no executable answer in-tree.
* posh already has the right seams — `Predictor`/`PredictionRenderer` traits (`predict/mod.rs`) and `FrameApplier`/`FrameEncoder` (`framesync/`) — so a second, mosh-backed implementation can sit beside the Rust one with no architectural change.
* The Rust rewrite is substantial and works; a strategy that discards it has a very high bar.
* Determinism: bugs in echo/emulation must reproduce byte-for-byte, in-process, without a network or encryption in the way.
* Honesty about cost: any C++-in-the-build choice partially walks back "pure Rust" and adds toolchain/devShell/nix burden.

## Considered Options

* **Option 1 — Pure-Rust reimplementation only (status quo).** Keep porting mosh's algorithms into Rust and chase echo/emulation bugs by reading mosh source and reasoning about divergence.
* **Option 2 — mosh C++ behind posh's traits as an FFI oracle.** Compile a slice of vendored mosh (terminal emulator now; predictor later) into a C-ABI shim, expose it through posh's existing `Predictor`/`FrameApplier` traits, and use it as a differential-test oracle (with an optional drop-in fallback impl).
* **Option 3 — Restart as incremental C++→Rust oxidation of mosh.** Abandon the from-scratch rewrite and strangler-fig mosh itself, rewriting parts in Rust over time.
* **Option 4 — Black-box differential testing via a lossy container (netem).** Run mosh and posh under an injected-loss container and compare rendered output, without any FFI.

## Decision Outcome

Chosen option: **Option 2 (mosh C++ as an FFI oracle behind posh's traits)**, because it makes mosh's behavior an executable, byte-for-byte oracle for exactly the subsystems where it is the reference, reusing seams posh already has — accepting a staged C++ build dependency and a small amount of behavior-preserving mosh refactoring.

A tracer bullet (`crates/mosh-ffi`, task #6) has already validated the riskiest part — the build/link plumbing: `g++` from the devShell compiles eight vendored mosh terminal `.cc` files plus a C-ABI shim (`csrc/shim.cc`) into a static lib via the `cc` crate, libstdc++ links automatically, and a Rust test drives mosh's real VT parser + `Terminal::Emulator` (a CUP-positioning test passes through the shim). The terminal carve-out required **zero** changes to mosh.

Option 3 was rejected: the rewrite already exists and works, so "transform mosh from C++" would be a *second* rewrite that discards working Rust — the opposite of incremental. Option 4 was rejected as the primary path: netem loss is non-deterministic, mosh's UDP is AES-encrypted (passive capture can't see diffs), and it cannot isolate the resolver; it remains useful only as a complementary realistic-loss check. Option 1 stays the default for everything *except* the reference-critical subsystems — the oracle augments it, it does not replace it.

### Scope: oracle first, drop-in only if justified

The default use is **oracle-only**: mosh's impl runs in tests next to posh's Rust impl on identical input, and divergence localizes the bug in the Rust code, which is then fixed. mosh C++ does **not** ship in the production client by default. Promoting a mosh-backed impl to a runtime drop-in (e.g. `MoshFfiPredictor` selected behind `POSH_PREDICTION_MODEL`) is a separate, later step gated by the criteria below.

### Consequences

Good:

* Executable, deterministic, in-process oracle for predictive echo and VT emulation — divergence is a failing test, not a prose argument.
* Reuses existing trait seams; the Rust impls stay the default, so no architectural disruption.
* The terminal slice needs no mosh changes and no extra system deps (C++ toolchain only).
* Keeps mosh as a correctness check without adopting its C++ as the substrate (Option 3 avoided).

Bad / costs accepted:

* posh's build gains a C++ toolchain dependency now, and — when the predictor slice lands — ncurses/protobuf in the devShell and nix (`Cargo.lock` + vendoring updates). devShell changes require a session restart (direnv reload mid-session is unsupported here).
* The predictor slice requires two small, behavior-preserving mosh decouplings: extract `Network::timestamp()` into a minimal `src/network/timing.h`, and parameterize `Network::ACK_INTERVAL` out of `NotificationEngine`. These must be guarded by characterization tests (see #75 / the poshterity harness) so the refactor cannot drift mosh's behavior.
* Two implementations of each subsystem to keep building (maintenance surface), justified only while mosh remains the reference.
* GPL/licensing: linking mosh C++ is consistent with posh's GPL-3.0-or-later, but binding scope must stay deliberate.

## Confirmation

* `cargo test -p mosh-ffi` builds both slices and asserts rendered output through the shim. It is gated in nix/CI by the `.#checks.<system>.mosh-ffi` flake check — isolated from the shipped `.#posh` build via workspace `default-members` — and wired into `just test` via `test-mosh-ffi`, so the merge hook exercises the oracle.
* The decision is honored when reference-critical bugs are chased via a differential test against the mosh oracle rather than by reasoning alone.

### Criteria to go beyond oracle-only

Promote a mosh-backed impl to a shipped runtime drop-in only if: (a) the Rust impl has a divergence we cannot economically fix and that materially harms users, and (b) the build-dependency cost (C++ + ncurses/protobuf on every build product) is accepted in an updated ADR, and (c) the licensing/binary-size implications are reviewed.

## More Information

* Tracer bullet: `crates/mosh-ffi/` (shim `csrc/shim.cc`, `build.rs`), task #6.
* Trait seams: `crates/posh/src/remote/predict/mod.rs` (`Predictor`), `crates/posh/src/remote/framesync/mod.rs` (`FrameApplier`).
* Characterization harness for the guarded mosh refactor: #75 (let poshterity reach the transport layer), #56 (poshterity epic).
* Rewrite campaign umbrella: #34.
* Entanglement finding: mosh's `src/terminal/` is a clean library (no network/statesync/protobuf deps); only `terminaldisplayinit.cc` needs ncurses, and the emulator does not require `Display`. The predictor (`src/frontend/terminaloverlay.*`) has exactly two network couplings, both surgically removable.
