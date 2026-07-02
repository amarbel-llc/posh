---
status: accepted
date: 2026-07-02
decision-makers: sfriedenberg
---

# posh-term does not background-color-erase on scroll

## Context and Problem Statement

Over a remote posh session, a scroll that happens while a non-default background pen is active leaves a stuck colored line on the client — a background the application never intended (posh#100). The classic trigger is Claude Code scrolling its transcript while a text-selection highlight is active: the newly-exposed line is painted with the selection's steel-blue background and never clears. A local (raw-passthrough) attach of the same app is clean; only the remote model→frame→client path bleeds.

The cause is a background-color-erase (BCE) on scroll in posh-term's model. `Terminal::scroll_up_n`/`scroll_down_n` (`crates/posh-term/src/terminal.rs`) fill the newly-exposed rows with `blank_style()`, whose background is the current pen's (`cursor.style.bg`). posh-term inherited this faithfully from mosh, whose `Framebuffer::newrow()` likewise uses `ds.get_background_rendition()`. `new_frame` then renders those cells into the client verbatim (as literal background spaces, since it makes no BCE assumption about the outer terminal), so the client shows the pen background on the scrolled-in line.

The terminals posh actually drives do not do this. `xterm-kitty` has `bce` unset (verified via `infocmp`; `just debug-term-bce xterm-kitty`); kitty does not background-color-erase a scroll, so on a real kitty the same byte stream leaves the scrolled-in line at the default background. posh-term's contract is *"fixed kitty-parity capabilities"* (`terminfo.rs`), so BCE-on-scroll is a divergence from posh-term's own intent.

This is not mosh-specific quirk avoidance: `mosh` rendering the same synthetic into a local kitty bleeds identically (confirmed empirically). mosh "seems clean" only because it grew up on BCE-capable terminals, where the scrolled-in background is the terminal-correct result; kitty's deliberate omission of `bce` is the new variable. mosh's renderer has a `has_bce` gate (`Display::can_use_erase = has_bce || default-pen`) that posh dropped, but that gate only changes *how* a background-blank run is drawn on a BCE client (EL vs literal spaces) — both show the background, both correct there — and does nothing on a non-BCE client, where mosh bleeds too.

The question this ADR settles: should posh-term's model carry the pen background onto scrolled-in lines?

## Decision Drivers

* posh-term's stated contract is kitty-parity, and kitty is non-BCE.
* #100 reproduces deterministically and is common in practice (any scroll under an active highlight).
* mosh is a *porting reference*, not a behavioral target; matching kitty is the goal, and mosh bleeds on kitty too.
* #42 (the inverse — backgrounds wrongly *dropped*) must not regress. Its root cause was a missing `TERM`/`COLORTERM`, unrelated to BCE, but changes near background fills should be checked against it.
* posh-term's public API (`lib.rs`) is frozen (callers may add, not change), so the fix must be internal to the model.

## Considered Options

* **Option 1 — Keep BCE on scroll (status quo).** Faithful mosh port. Bleeds on every non-BCE client (kitty), which is what posh drives.
* **Option 2 — Scrolled-in lines use the default background (non-BCE on scroll).** `scroll_up_n`/`scroll_down_n` fill with `Style::default()`. Matches kitty; a deliberate, documented divergence from mosh.
* **Option 3 — Full non-BCE (erase too).** Also drop BCE from erase/insert/delete. True kitty-parity, but larger blast radius, flips the codified `bce_erase_uses_background` expectation, and is not what #100 reproduces.
* **Option 4 — Port mosh's `has_bce` renderer gate.** Detect the client's `bce` and choose EL-vs-literal-spaces accordingly. Does not fix the model-level artifact on a non-BCE client (the pen bg is still in the model), so does not fix #100.

## Decision Outcome

Chosen: **Option 2** — posh-term does not background-color-erase on scroll. `Terminal::scroll_up_n` and `scroll_down_n` fill newly-exposed rows with `Style::default()` instead of `blank_style()`. This covers every scroll that reaches those functions: `\n`/IND at the bottom margin (`index`), RI at the top (`reverse_index`), CSI SU/SD, and DECSTBM region scrolls.

This is a deliberate divergence from the mosh reference, justified because posh's rendering target (kitty) is non-BCE and posh-term's contract is kitty-parity. mosh could assume a BCE-matched client; posh cannot.

Scope is **scroll only**. In-place erase (EL/ED/ECH) keeps BCE, matching the existing `bce_erase_uses_background` test and the common "draw a background bar via erase-under-pen" idiom. Erase-under-bg and IL/DL-under-bg are the same latent divergence class from kitty, but are out of scope for #100 (which reproduces on scroll), less commonly hit, and would flip a codified expectation. Whether posh-term should be *fully* non-BCE is left as a follow-up to revisit if erase-side bleeds surface.

### Consequences

Good:

* Fixes #100 at the model level, so every render path (full repaint, incremental diff, scroll shortcut) is correct — the artifact is simply not in the model. This is why the `render.scroll_opt` shortcut toggle never helped: the spurious background was in the model, not the shortcut.
* Matches kitty, posh-term's parity target.
* Guarded by a deterministic unit test (`scroll_does_not_bce_the_new_line`, the dual of `bce_erase_uses_background`) and reproducible end-to-end via `poshterity render` into a real kitty.

Bad / accepted:

* A deliberate behavioral divergence from the mosh reference — the differential mosh-ffi oracle (ADR 0004) will disagree on scroll-under-bg fills; that characterization must encode the intended divergence rather than treat it as a regression.
* posh-term is now non-BCE on scroll but BCE on erase — an internal inconsistency, accepted as scoped; full kitty-parity (non-BCE erase) is deferred.

## Confirmation

* `scroll_does_not_bce_the_new_line` (`crates/posh-term/tests/terminal.rs`) asserts a scroll under a non-default bg pen leaves the scrolled-in line default. It fails on the status quo and passes after the fix.
* `poshterity render --raw` turns the synthetic (`just debug-posh-bleed-scroll`) into the exact client tty bytes; before the fix it paints the scrolled-in line steel-blue, after it renders clean — confirmed by `cat`ing into a real kitty (`just debug-posh-bleed-render`).
* The merge hook's `cargo test --workspace` runs the guard.

## More Information

* Bug + full diagnosis: posh#100.
* Not #42 (backgrounds *dropped*, caused by a missing `TERM`/`COLORTERM`) nor #86 (SGR passthrough).
* mosh reference: `zz-mosh/src/terminal/terminalframebuffer.{h,cc}` (`newrow()`, `insert_line`/`delete_line`), `terminaldisplay.cc` (`can_use_erase`/`has_bce`). mosh-ffi oracle: ADR 0004.
* kitty capability: `infocmp xterm-kitty` (`bce` absent) vs `xterm-256color` (`bce` present); `just debug-term-bce <term>`.
* Repro + round-trip tooling: `just debug-posh-bleed-scroll` (synthetic), `poshterity render` / `just debug-posh-bleed-render` (server→client bytes).
