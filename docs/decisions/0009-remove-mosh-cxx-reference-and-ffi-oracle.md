---
status: accepted
date: 2026-09-30
decision-makers: sfriedenberg
supersedes: 0004
---

# Remove the vendored mosh C++ tree and its FFI oracle

## Context and Problem Statement

ADR 0004 (2026-06-16) vendored mosh 1.4.0 under `zz-mosh/` and added
`crates/mosh-ffi`, which compiled a slice of that C++ (the terminal emulator
and the predictive-echo `PredictionEngine`) behind a C-ABI shim. The goal was
an executable oracle for **differential** testing: drive identical input
through mosh and posh, and treat divergence as a bug in posh's Rust.

Three and a half months later:

* The differential step was never built. `mosh-ffi` depends on no posh crate;
  its tests (`characterization.rs`, `predict_characterization.rs`,
  `predict_trace.rs`) assert mosh against goldens blessed from mosh itself.
  They prove "mosh still behaves like mosh" and say nothing about posh.
  The comparison is posh#82, open with no activity.
* Nothing shipped depends on either tree. No workspace crate references
  `mosh-ffi` or its fixtures; `MoshPredictor` (`predict/mosh.rs`) is a pure
  Rust port.
* Both trees were merge-gate lanes (`build-nix`/`test-nix` for `.#mosh`,
  `test-mosh-ffi` for the oracle), pulling a C++ toolchain, protobuf,
  abseil, openssl, ncurses and an autotools stack into every merge — plus
  clang-format config, a version-lineage exception, and devShell weight.
  On 2026-09-30 a routine nixpkgs bump (abseil-cpp 20260817 requiring C++20)
  broke the `.#mosh` lane and blocked a merge that touched no C++ at all.

The question: keep paying for an oracle that is not used as one, or drop it?

## Considered Options

* **A — Remove both** `zz-mosh/` and `crates/mosh-ffi`.
* **B — Remove `mosh-ffi` and stop building `zz-mosh`,** keeping the source as
  a read-only reference in-tree.
* **C — Keep both** and build posh#82 (the differential test) to realize
  ADR 0004's payoff.

## Decision Outcome

Chosen option: **A**, because the oracle's cost is real and recurring while
its benefit was never realized; the reference source remains available
upstream (https://github.com/mobile-shell/mosh, tag `mosh-1.4.0`) and in this
repository's history (last present at the parent of the commit that lands
this ADR).

B was rejected: an unbuilt, untested vendored tree drifts silently and still
carries formatter/exclude config, for reading material that upstream already
hosts. C was rejected for now: the Rust ports have matured without the
oracle, and nothing currently points to a predictor or emulator divergence
that needs byte-for-byte localization. If one appears, the ADR 0004
approach can be revived from history (see Consequences).

### Consequences

Good:

* The merge gate drops the C++ lanes and their dependency closure; nixpkgs
  bumps no longer break merges through C++ toolchain churn.
* The devShell drops the autotools/protobuf/ncurses/openssl/clang-tools set.
* posh has one version lineage (`POSH_VERSION`); the mosh `1.4.0` exception
  is gone.

Bad / costs accepted:

* No executable mosh oracle. A future echo/emulation bug that needs one must
  resurrect `crates/mosh-ffi` and the needed `zz-mosh` slice from git
  history — including the two posh-local mosh changes (the
  `src/network/timing.h` extraction and the `MOSH_PREDICTION_LOG` trace)
  that are not upstream.
* posh#82 (differential test) and posh#62 (wire mosh's tmux emulation tests
  into CI) become moot and are closed.
* Records that cite `zz-mosh/src/...` paths (ADR 0002, ADR 0005, RFC 0004,
  FDR 0002, FDR 0003) now refer to upstream mosh 1.4.0 at the same paths.

## Confirmation

`just` (the merge gate) no longer builds `.#mosh` or `.#checks.*.mosh-ffi`;
`rg 'zz-mosh|mosh-ffi'` outside `docs/` finds nothing.

## More Information

* Superseded: ADR 0004 (`0004-use-mosh-cxx-ffi-oracle.md`).
* Closed as moot: posh#82, posh#62.
