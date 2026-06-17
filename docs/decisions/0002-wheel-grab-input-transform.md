---
status: accepted
date: 2026-06-12
decision-makers: sfriedenberg
---

# Transform grabbed wheel input with a byte-fed state machine, not per-read buffer scanning

## Context and Problem Statement

`POSH_GRAB_MOUSE` (posh#50) makes the roaming client grab the wheel on the outer terminal and translate wheel-up/down into arrow keys, dropping other mouse events — so scrolling behaves the same across terminals instead of being at the mercy of each terminal's alt-screen wheel handling (kitty turns the wheel into arrows and ignores DECSET 1007; see ADR-adjacent issues #3/#28). The client reads stdin in 4 KB batches (`crates/posh/src/remote/client.rs`), and an SGR mouse sequence (`ESC [ < Cb ; Cx ; Cy (M|m)`) can split across two `read()` calls. The question (posh#52): how should the input transform be structured so a split sequence is reassembled rather than leaking raw `ESC[<…` bytes to the session app — without corrupting or delaying real keystrokes?

## Decision Drivers

* Never corrupt real input. Arrows, ctrl-keys, function keys, UTF-8 — all must round-trip byte-for-byte; a regression that mangles keystrokes is far worse than the cosmetic leak being fixed.
* Handle a sequence split at *any* byte boundary, not just the common mid-body case.
* Match the architecture the codebase and its lineage already use, rather than inventing a parallel mechanism.
* Keep the change scoped to the (default-off) grab path; don't add machinery to the hot input loop for an edge.

## Considered Options

* **Option 1 — Held-partial buffer on a per-read scan.** Keep the original buffer-scan; when a batch ends in a complete `ESC[<` prefix with no terminator, hold that tail and prepend it to the next batch. Bound the held buffer so an unterminated `ESC[<` can't grow forever.
* **Option 2 — Reuse `posh_term::Parser`.** Drive the existing VT500 parser (already used to decode server output) over the input stream, inspect emitted `Action::Csi { private: b'<', .. }` for mouse events, and re-serialize everything else back to bytes.
* **Option 3 — Purpose-built byte-fed `MouseFilter` state machine.** A small persistent state machine (`Ground → Esc → Bracket → Body → terminator`) fed one byte at a time, state living in `ClientState`. Only bytes that are part of a live `ESC[<…` match are withheld; the instant a match fails or overflows a cap, every buffered byte is flushed verbatim. Modeled on mosh's `UserInput` (`zz-mosh/src/terminal/terminaluserinput.{h,cc}`), which holds parser state across single-byte calls.

## Decision Outcome

Chosen option: **Option 3 (byte-fed `MouseFilter`)**, because it reassembles a split sequence at every byte boundary while passing all non-mouse input through losslessly, accepting that a lone trailing `ESC` is withheld until the next byte resolves it (the classic Esc-vs-escape-sequence ambiguity — see below).

Option 1 was implemented first, then backed out: it only fixed the mid-body split and had to *punt* the mid-prefix split (a bare trailing `ESC`/`ESC[` is ambiguous with a real Esc/arrow, so holding it would swallow real input) — leaving a residual leak and carrying a `MAX_GRAB_PARTIAL`-as-leak-guard hack. Option 2 was investigated and rejected: the parser is a *consume-and-dispatch* output decoder, so reusing it on the input side forces a faithful `Action → bytes` re-serializer for the *entire* input stream (real arrows/ctrl-keys must round-trip exactly), which is more surface and more corruption risk than the bug. Option 3 gets Option 2's architectural benefit (native split handling via persistent state, mosh's proven model) without the re-serialization risk, because non-mouse bytes are never "parsed" — they pass through untouched.

### The lone-ESC tradeoff

A byte-fed machine cannot know whether a trailing `ESC` begins a mouse sequence until the next byte arrives, so it holds it. Under `POSH_GRAB_MOUSE=on` (and only when the inner app has set no mouse mode — a bare prompt), a *solo* `Esc` keypress is therefore withheld until the next key. This is the universal VT input ambiguity (why vim has `ttimeoutlen`, readline `keyseq-timeout`); mosh's `UserInput` holds `ESC` the same way. The other standard resolution — a millisecond timeout flush — was **deliberately not added**: it would put a deadline in the client poll loop for a default-off feature's edge, where a lone Esc at a bare prompt rarely matters.

### Consequences

* Good, because a wheel sequence split across reads reassembles at *any* boundary, with no raw-byte leak — verified by a test that exercises every split point.
* Good, because all non-mouse input (Esc, arrows, ctrl-keys, UTF-8) round-trips losslessly: a byte is withheld only while part of a live `ESC[<…` match, and any match failure flushes the buffer verbatim.
* Good, because it mirrors mosh's `UserInput` and posh-term's own parser — one incremental-state-machine idiom for input, not a bespoke buffer hack.
* Good, because the buffer is bounded (`MAX_MOUSE_SEQ`): an unterminated `ESC[<…` is flushed raw, never held or grown indefinitely.
* Bad, because a solo `Esc` keypress is delayed by one input byte under grab — the accepted ambiguity tradeoff above.
* Neutral, because the filter is only consulted while `grab_active` (policy on + app without its own mouse mode). The persistent state adds one obligation the buffer-scan didn't have: if grab flips off mid-sequence (the app enables its own mouse mode between reads), the held partial must be *drained back into the stream*, not dropped — otherwise the prefix is lost and a corrupt tail leaks to the app. Handled via `take_pending` at the call site.

### Confirmation

`just build-rust` (hermetic `cargo test --workspace`) plus the `remote::client` tests: `grabbed_wheel_becomes_arrows_and_other_events_drop`, `grabbed_split_sequence_reassembles_at_any_boundary` (loops over every split index), `non_mouse_escape_sequences_round_trip_losslessly` (incl. the held-then-flushed lone ESC), `grab_flip_mid_sequence_hands_back_the_held_partial`, and `grabbed_partial_is_bounded_and_flushed_not_held_forever`. Live-verified over a loopback server+client pair in kitty (`just debug-verify-grab on`): wheel ticks arrive as arrows in every bare-prompt state, with no stray escape bytes.

## Selection coexistence (the grab vs. native text selection tradeoff)

The grab this ADR transforms has a cost the original framing did not record:
while posh holds the outer terminal in mouse reporting (`?1000h ?1006h`) at a
bare prompt — to harvest the wheel for client-side scrollback (FDR 0005) — the
terminal forwards *all* mouse events (clicks, drags) to posh, so the
terminal's own click-drag text selection stops working. posh only consumes the
wheel (`out.wheel`) and drops clicks/drags, so it does not *want* those events;
the grab is simply coarser than the need. This section records what the
terminal-side options actually are, verified against kitty's documentation, so
a future reader does not re-derive it.

The verified capability map (kitty):

* **Shift+drag-to-select while grabbed is a kitty default**, and a documented
  headline feature ("select text with kitty even when a terminal program has
  grabbed the mouse by holding down the `Shift` key"). So "selection regardless
  of mouse mode" already works *with* the `Shift` modifier today — posh's grab
  does not defeat it. This is the no-cost coexistence path.
* **DECSET 1007 (alternate scroll) is the clean wheel-only-without-grab
  mechanism, but kitty ignores it** (posh#3/#28) — which is the root reason the
  `?1000` grab exists at all. Crossed off, not overlooked.
* **There is no wheel-only mouse-reporting mode.** The wheel arrives as SGR
  buttons 64/65 *inside* `?1000`+, all-or-nothing with clicks/drags. No DECSET
  reports the wheel while leaving click/drag to the terminal.
* **The kitty keyboard protocol does not carry scroll/mouse events** (keys
  only), so it is not a route to wheel-without-grab either.

Terminal-side mitigations (kitty `kitty.conf`, user-controlled — not posh
code): adding `grabbed` to the selection `mouse_map` makes plain (un-modified)
drag select even under the grab —
`mouse_map left press grabbed,ungrabbed mouse_selection normal` (plus the
`doublepress`/`triplepress` word/line variants). Tradeoff: a plain left-drag
then no longer reaches the grabbing app — harmless for posh (it drops drags),
but it would take left-drag from a different grabbed app, which is why kitty's
default reserves the grabbed state for `shift+left`. For per-app scoping
without a global change, launch posh in a separately-configured kitty
(`kitty -o "mouse_map …" posh …` or `kitty -c posh.conf posh …`, both
verified). A true in-one-window per-app `mouse_map` via `--when-focus-on` is
**unverified** (the conditional-mapping mechanism is documented only for the
keyboard `map` directive) and would additionally need posh to emit an
`OSC 1337;SetUserVar` in lockstep with its grab predicate.

This stays terminal-configuration guidance rather than a posh decision: the
recommendation to enable selection-under-grab in kitty is tracked in eng#36.
A posh-side runtime toggle that *drops* the grab on demand (restoring plain
native selection at the cost of wheel scrollback) remains a revisitable option
— it is one gated decision (`wheel_active` in
`crates/posh/src/remote/client.rs`), not a rearchitecture — but is not adopted
here.

## Pros and Cons of the Options

### Option 1 — Held-partial buffer

* Good, because it is the smallest change over the original buffer-scan.
* Neutral, because it needs a `MAX_GRAB_PARTIAL` bound so an unterminated prefix can't grow forever.
* Bad, because it only handles the mid-body split; the mid-prefix split (bare trailing `ESC`/`ESC[`) is left leaking, because holding those would risk swallowing a real Esc/arrow.
* Bad, because "hold a tail and prepend next time" is a hand-rolled reimplementation of incremental parsing the codebase already does properly.

### Option 2 — Reuse `posh_term::Parser`

* Good, because it reuses the existing, well-tested VT500 state machine and handles all splits natively.
* Bad, because the parser is built to consume-and-dispatch output; using it for an input *filter* requires re-serializing every non-mouse `Action` back to its exact original bytes.
* Bad, because that re-serializer is a second hand-rolled encoder whose any-mismatch failure mode is corrupting real keystrokes — strictly more dangerous than the cosmetic leak.

### Option 3 — Byte-fed `MouseFilter`

* Good, because persistent state reassembles splits at any boundary with no special-casing.
* Good, because non-mouse bytes are never parsed/re-encoded — they pass through verbatim, so real input cannot be corrupted.
* Good, because it matches mosh's and posh-term's incremental model.
* Bad, because it holds a lone trailing `ESC` until the next byte (the accepted ambiguity tradeoff).

## More Information

* Feature: posh#50 (`POSH_GRAB_MOUSE`), motivated by posh#3/#28 (kitty ignores DECSET 1007). Split-reassembly: posh#52.
* Implementation: `crates/posh/src/remote/client.rs` — `MouseFilter` / `MouseState`, `grab_active`, the `process_user_input` grab branch; state held in `ClientState.mouse_filter`.
* Lineage: mosh `zz-mosh/src/terminal/terminaluserinput.{h,cc}` (byte-fed `UserInput` with persistent `state`); posh-term `crates/posh-term/src/parser.rs` (Williams VT500 machine), the same incremental pattern this filter mirrors.
* Not chosen, revisitable: if a future need makes the lone-Esc delay perceptible in practice, the standard fix is a timeout flush in the client poll loop (the `ttimeoutlen` approach) — add it then, not pre-emptively.
* Generalized by ADR-0003 (`0003-stream-reassembly-across-reads.md`): `MouseFilter` is one of five instances of the repo-wide "carry partials across reads" convention; 0003 records the pattern and why the instances are kept separate rather than abstracted.
* Selection coexistence (above): the kitty capability map and the
  enable-selection-under-grab recommendation are tracked in eng#36. The grab's
  effect on native text selection, the dead-end of DECSET 1007 on kitty, and
  the `mouse_map grabbed,ungrabbed` mitigation were verified against kitty's
  mouse-protocol/mapping docs.
