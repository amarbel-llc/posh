---
status: accepted
date: 2026-06-12
decision-makers: sfriedenberg
---

# Reassemble multi-byte structures across reads; do not abstract the reassemblers

## Context and Problem Statement

posh reads every byte stream — the local tty, the session PTY, the IPC socket, the roaming UDP transport — into a fixed buffer per `read()`/`recvfrom()`. The structures carried on those streams (escape sequences, framed IPC messages, fragmented datagrams) are multi-byte and can straddle a read boundary: the kernel can return a buffer that ends partway through one. A consumer that scans each buffer in isolation, assuming each structure arrives whole, corrupts or leaks the straddling one. posh#52 was exactly this bug — `translate_grabbed_mouse` scanned each tty read for SGR mouse sequences and leaked the raw bytes of any sequence split across reads. This ADR records the convention that prevents the whole class, and the decision *not* to unify the (deliberately different) implementations of it.

## Decision Drivers

* Correctness across read boundaries is non-negotiable for a terminal tool: a split escape sequence that leaks can flip the outer terminal off posh's screen (FDR 0002), corrupt input to the session app, or desync a frame.
* New stream consumers should fall into the safe pattern by default, not re-learn the hazard each time (posh#52 was a fresh feature that missed it).
* Avoid premature abstraction: a shared "reassembler" type is only worth it if the instances share *mechanism*, not merely a one-sentence goal.

## Considered Options

* **Option 1 — Name the pattern as a convention; keep the implementations separate.** Document the invariant once (here) and require new stream scanners to follow it; leave the five existing reassemblers as the right-sized, independent shapes they are.
* **Option 2 — Extract a shared `trait Reassembler` / generic buffer type** that all stream consumers implement, to eliminate the apparent duplication.
* **Option 3 — Do nothing (rely on review/luck).** Leave the pattern implicit; catch regressions case by case.

## Decision Outcome

Chosen option: **Option 1 (name the convention, keep implementations separate)**, because the five instances share a single *invariant* but genuinely different *mechanisms*, so a shared abstraction would unify the narration while the mechanism stayed divergent — accepting that the convention lives in prose (this ADR + the per-site doc comments) rather than in a type the compiler enforces.

### The pattern

> **A stream consumer accumulates bytes across reads and acts on a structure only once it is provably complete, holding any trailing partial for the next read, with a bound so a hostile or garbage stream cannot grow the hold buffer without limit.**

Two micro-architectures realize it, chosen by the wire format:

* **Accumulate-and-test** — buffer bytes, test for a complete structure by an explicit signal (length prefix, fragment offset, prefix-match against a fixed key set). Use when completeness is a simple count or lookup.
* **Byte-fed state machine** — feed bytes one at a time into a parser whose state persists across reads; the structure completes when the machine reaches a terminator. Use for free-form escape streams, where "is it complete?" is itself a parse. This is the mosh `UserInput` / VT500 model.

### The five instances (one pattern, five shapes)

| site | guards | completeness signal | shape | bound |
|---|---|---|---|---|
| `session/ipc.rs` `FrameBuffer` | IPC socket | length prefix in header | accumulate-and-test | `MAX_FRAME_LEN` |
| `remote/sync.rs` `FragmentAssembly` | UDP payloads | fragment offsets/count | accumulate-and-test | `MAX_FRAGMENTS` |
| `session/client.rs` `DetachMatcher` | tty input (detach key) | prefix-match vs. fixed key set | accumulate-and-test (`carry`) | keys ≤8 bytes |
| `session/daemon.rs` `ScreenSwitchFilter` | PTY output (alt-screen/RIS excision) | the VT model's `take_screen_switch` / `mid_escape` | byte-fed (drives `Terminal`) | `MAX_HELD` |
| `remote/client.rs` `MouseFilter` | tty input (wheel grab) | own SGR grammar reaching `M`/`m` | byte-fed state machine | `MAX_MOUSE_SEQ` |

The completeness signal is intrinsic to each wire format and not factorable; the pass-through-vs-consume semantics also differ (framing consumes all; the input/output filters forward most bytes and intercept a few). That is why they are not collapsed.

### Consequences

* Good, because every existing stream consumer is split-safe, and the convention gives new ones a clear target — "carry partials across reads, bound the hold."
* Good, because each reassembler stays the simplest shape for its format (counting for framed protocols, a parser for escape streams) instead of contorting to fit one interface.
* Good, because UDP and IPC get this nearly for free (datagrams are atomic at the socket; length-prefix framing is inherently split-safe), so the convention's real teeth are on the two free-form escape-stream filters.
* Bad, because the invariant is enforced only by convention and review, not by the type system — a future scanner can still regress it (as posh#52 did). The mitigation is this ADR plus `/code-review`'s removed-behavior angle.
* Neutral, because there is genuine surface-level repetition (five `held`/`carry`/`pending` buffers) that a casual reader may mistake for duplication ripe for extraction; this ADR exists partly to forestall that premature refactor.

### Confirmation

Each instance carries a regression test asserting split-safety against its format (e.g. `MouseFilter`'s `grabbed_split_sequence_reassembles_at_any_boundary` loops over every split index; `FrameBuffer`/`FragmentAssembly` have partial-delivery tests). A new stream consumer is expected to add the equivalent. `just build-rust` runs them all.

## Pros and Cons of the Options

### Option 1 — Convention, separate implementations

* Good, because each reassembler is right-sized to its wire format.
* Good, because it documents the hazard where a future author will hit it (this ADR + per-site comments).
* Neutral, because the shared part is a one-sentence invariant, which lives in prose rather than code.
* Bad, because nothing mechanically prevents the next scanner from regressing it.

### Option 2 — Shared `trait Reassembler` / generic buffer

* Good, because it would put the invariant in one compiler-checked place.
* Bad, because the five instances share no mechanism — completeness is variously a byte count, an offset, a prefix-match, a VT-parser callback, and a bespoke grammar — so the trait would need a completeness-callback and a pass-through-policy hook so generic they add indirection without removing logic.
* Bad, because it is premature: Rule-of-Three applies to *mechanism*, and only two instances (the byte-fed escape-stream filters) share enough to plausibly merge.

### Option 3 — Do nothing

* Good, because zero work now.
* Bad, because the pattern stays implicit and the next feature repeats posh#52.

## More Information

* Prompting bug: posh#52 (split SGR mouse sequence leaked raw); its fix and the byte-fed-vs-buffer-scan decision are ADR-0002, which this ADR generalizes.
* Instances: `session/ipc.rs` (`FrameBuffer`), `remote/sync.rs` (`FragmentAssembly`), `session/client.rs` (`DetachMatcher`), `session/daemon.rs` (`ScreenSwitchFilter`), `remote/client.rs` (`MouseFilter`).
* Revisit trigger: if a **third** free-form escape-stream byte-fed filter appears (the `MouseFilter`/`ScreenSwitchFilter` shape — not the length-framed protocols), those share enough mechanism — drive a parser, hold a `Vec` until it settles, bound it — that a small shared helper may then earn its keep. Re-evaluate Option 2 at that point, scoped to just those filters.
* Lineage: the discipline came in with the mosh/zmx ports (mosh's `Parser`/`UserInput`, zmx's framed IPC); posh#52 was a fresh feature that didn't reach for it, which is the failure mode this ADR addresses.
