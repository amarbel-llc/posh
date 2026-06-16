# mosh characterization harness — design

Status: implemented — terminal + predictor slices built, the decouple refactor landed (timing.h), gated in nix/CI via `.#checks.mosh-ffi`.
Date: 2026-06-16
Related: ADR 0004 (FFI oracle), #75 (poshterity transport reach), #56 (poshterity epic), task #4.

## Goal

Capture **deterministic, byte-for-byte goldens** of mosh's terminal and (later)
predictor behavior, so that (a) the small mosh decouple refactor needed for the
predictor FFI slice is provably behavior-preserving, and (b) posh's own Rust
emulator/predictor can be diffed against mosh as an executable oracle.

## Why not record mosh over the network

The obvious "record `mosh-client` output and golden it" path is rejected as the
primary mechanism: mosh is a UDP server/client pair, its protocol carries
RTT/seq adaptation, its datagrams are AES-encrypted (passive capture can't see
diffs), and its rendered output under prediction is timing-dependent — none of
which is byte-for-byte reproducible. It survives only as a complementary
realistic-loss check.

## Architecture (one shape, both slices)

- **Driver = the FFI shim** (`crates/mosh-ffi`), not a network capture. Feed a
  fixed input script → dump mosh's state → golden it. In-process: no UDP, no
  crypto, no timing race.
- **Injected clock.** mosh's predictor is entirely timing-driven via
  `Network::timestamp()`. The shim *provides* that symbol (we do not link
  `network.cc`), settable from Rust before each step, making the predictor
  deterministic by construction. This is the property the network path cannot
  give and the single reason the harness is shim-based.
- **Reuse poshterity's golden format.** Render mosh's framebuffer into poshterity's
  diff-friendly `Grid` shape (and later its style sidecar) so mosh goldens read
  like the rest of the suite and can be diffed directly against posh's own
  emulator output. poshterity `.castx` recordings become the realistic input
  corpus alongside handcrafted VT edge-case scripts.
- **Guard = bless-before / assert-after.** Bless goldens from current mosh → do
  the refactor → assert byte-identical. Identical goldens ⇒ provably
  behavior-preserving.

## The terminal/predictor asymmetry (the spine)

`terminaloverlay.h` (the predictor) includes both `network.h`
(→ `src/crypto/crypto.h`) and `transportsender.h`
(→ `src/protobufs/transportinstruction.pb.h`). So including the predictor header
drags crypto + protobuf at compile time. The terminal library (`src/terminal/`)
has neither.

| slice | shim builds today? | deterministic? | guards |
| --- | --- | --- | --- |
| terminal (`Emulator`) | yes (tracer proved it; no crypto/protobuf) | yes, inherently | nothing to refactor — serves as oracle + regression net |
| predictor (`terminaloverlay`) | not cleanly — header drags crypto+protobuf | yes, *with* the injected clock | the `timing.h` / `ACK_INTERVAL` decouple refactor |

## Build order

1. **Terminal characterization harness — now.** Clean, unblocked, immediately
   useful as the oracle for posh's own emulator. Golden test over fixed VT
   scripts, `bless`/`assert`, no new system deps. (This document's first
   deliverable.)
2. **Predictor slice — after.** Requires the decouple refactor. Two ways to keep
   the refactor honest:
   - (a) a one-time heavier *pre-refactor* shim (crypto/protobuf on the include
     path; link only our injected `Network::timestamp()`), bless goldens,
     refactor, rebuild the lightweight shim, assert identical. Rigorous.
   - (b) lean on mosh's own test suite + review for the tiny refactor, then take
     fine-grained goldens from the lightweight post-refactor shim. Faster.
   Lean (a); it hinges on whether `transportinstruction.pb.h` is in-tree or a
   protoc build artifact — **open question, resolve before committing to (a).**

## Terminal harness shape (as built)

- `crates/mosh-ffi/tests/fixtures/*.in` — escape-encoded VT input scripts
  (`\e`, `\xNN`, `\r`, `\n`, `\t`), decoded by the test. Human-editable; no raw
  control bytes in the repo.
- `crates/mosh-ffi/tests/fixtures/*.grid` — blessed golden renders (per-row
  trailing whitespace trimmed for golden stability; the shim's `render()` stays
  faithful).
- `crates/mosh-ffi/tests/characterization.rs` — feeds each script at its
  declared size, renders, compares; re-blesses when `MOSH_FFI_BLESS` is set.
- `just debug-mosh-bless` regenerates goldens; `just debug-cargo test -p
  mosh-ffi` asserts (the normal dev loop).

## nix/CI

`mosh-ffi` is excluded from the shipped `.#posh` build (workspace
`default-members`) and gated by a dedicated `.#checks.<system>.mosh-ffi`
derivation, wired into `just test` via `test-mosh-ffi` so the merge hook runs
it (the `build(nix)` commit under ADR 0004).
