---
status: accepted
date: 2026-07-03
decision-makers: sfriedenberg
---

# posh-term does not background-color-erase

## Context and Problem Statement

Over a remote posh session, a "blank" created under a non-default background pen — by scrolling, erasing, or inserting/deleting lines or characters — was filled with the *pen's* background instead of the default. On a scroll under an active highlight (a text selection scrolling out of a transcript) this left a stuck colored line on the client (posh#100); the same happens for erase-under-pen and the other edit ops (posh#110). A local raw-passthrough attach was clean; only the remote model→frame→client path bled.

The cause is a background-color-erase (BCE) in posh-term's model. Every blank-creating op routes through `Terminal::blank_style()` (`crates/posh-term/src/terminal.rs`), which returned a style whose background was the current pen's (`cursor.style.bg`). posh-term inherited this faithfully from mosh, whose `Framebuffer::newrow()` / `reset_cell` likewise use `ds.get_background_rendition()`. `new_frame` then renders those cells into the client verbatim (as literal background runs, since it makes no BCE assumption about the outer terminal), so the client shows the pen background on the blanked cells.

The terminals posh actually drives do not do this. `xterm-kitty` has `bce` unset (verified via `infocmp`; `just debug-term-bce xterm-kitty`); kitty does not background-color-erase. So on a real kitty the same byte stream leaves blanked cells at the default background. posh-term's contract is *"fixed kitty-parity capabilities"* (`terminfo.rs`), so BCE is a divergence from posh-term's own intent.

This is not mosh-specific quirk avoidance: `mosh` rendering the same synthetic into a local kitty bleeds identically (confirmed empirically). mosh "seems clean" only because it grew up on BCE-capable terminals, where the filled background is the terminal-correct result; kitty's deliberate omission of `bce` is the new variable. mosh's renderer has a `has_bce` gate (`Display::can_use_erase = has_bce || default-pen`) that posh dropped, but that gate only changes *how* a background-blank run is drawn on a BCE client (EL vs literal spaces) — both show the background, both correct there — and does nothing on a non-BCE client, where mosh bleeds too.

The question this ADR settles: should posh-term's model carry the pen background onto cells it blanks?

> **Scope history.** This decision initially shipped scoped to scroll only (posh#100, the reproduced symptom), deferring the in-place-erase paths. It was then expanded to *all* blank-creating ops (posh#110) — the record below reflects the final, broader decision.

## Decision Drivers

* posh-term's stated contract is kitty-parity, and kitty is non-BCE across the board.
* The bleed reproduces deterministically and is common in practice (any scroll/erase under an active highlight).
* mosh is a *porting reference*, not a behavioral target; matching kitty is the goal, and mosh bleeds on kitty too.
* #42 (the inverse — backgrounds wrongly *dropped*) must not regress. Its root cause was a missing `TERM`/`COLORTERM`, unrelated to BCE, but changes near background fills should be checked against it.
* posh-term's public API (`lib.rs`) is frozen (callers may add, not change), so the fix must be internal to the model.

## Considered Options

* **Option 1 — Keep BCE (status quo).** Faithful mosh port. Bleeds on every non-BCE client (kitty), which is what posh drives.
* **Option 2 — Non-BCE everywhere: blanked cells take the default background.** `blank_style()` returns `Style::default()`, so erase (ED/EL/ECH), scroll, IL/DL, ICH/DCH, and clears all fill default. One chokepoint; matches kitty; a deliberate, documented divergence from mosh.
* **Option 3 — Scroll only.** Fix just the scroll path, keep erase-BCE. This was the initial #100 scope; it leaves the same latent divergence on the in-place ops (#110) and a posh-term that is inconsistently half-BCE.
* **Option 4 — Port mosh's `has_bce` renderer gate.** Detect the client's `bce` and choose EL-vs-literal-spaces accordingly. Does not fix the model-level artifact on a non-BCE client (the pen bg is still in the model), so does not fix the bug.

## Decision Outcome

Chosen: **Option 2** — posh-term does not background-color-erase. `Terminal::blank_style()` returns `Style::default()`, and every blank-creating op takes its fill from `blank_style()`: erase (`erase_display`/`erase_line`/`erase_chars`), scroll (`scroll_up_n`/`scroll_down_n`, and thus IND/RI/SU/SD/region scrolls), line insert/delete (IL/DL), char insert/delete (ICH/DCH), and the screen clears.

This is a deliberate divergence from the mosh reference, justified because posh's rendering target (kitty) is non-BCE and posh-term's contract is kitty-parity. mosh could assume a BCE-matched client; posh cannot. Centralizing the decision in `blank_style()` also makes the one place a future **client-cap-aware** BCE (BCE when the client's `TERM` has `bce`, non-BCE otherwise) would live — tracked in #115.

### Consequences

Good:

* Fixes the bleed at the model level, so every render path (full repaint, incremental diff, scroll shortcut) is correct — the artifact is simply not in the model. This is why the `render.scroll_opt` shortcut toggle never helped: the spurious background was in the model, not the shortcut.
* Matches kitty, posh-term's parity target, and is internally consistent (one non-BCE policy, not half).
* Guarded by deterministic unit tests (`erase_does_not_bce`, `erase_line_and_insert_line_do_not_bce`, `scroll_does_not_bce_the_new_line`) and reproducible end-to-end via `poshterity render` into a real kitty.

Bad / accepted:

* A deliberate behavioral divergence from the mosh reference — the differential mosh-ffi oracle (ADR 0004) will disagree on any blank-under-pen fill; that characterization must encode the intended divergence rather than treat it as a regression.
* On a client terminal that **is** BCE-capable (the xterm lineage), an app's *deliberate* background fill via erase/scroll-under-pen (e.g. `\x1b[48;5;236m\x1b[2J` to paint a themed background) is now dropped — the inverse-#42 tradeoff, accepted for the common (kitty) case. #115 explores restoring it per the actual client `bce`.

## Confirmation

* `erase_does_not_bce`, `erase_line_and_insert_line_do_not_bce`, and `scroll_does_not_bce_the_new_line` (`crates/posh-term/tests/terminal.rs`) assert that ED/EL/IL/scroll under a non-default bg pen leave the blanked cells default. They fail on the status quo and pass after the change.
* `poshterity render --raw` turns the synthetic (`just debug-posh-bleed-scroll`) into the exact client tty bytes; before the change it paints the scrolled-in line steel-blue, after it renders clean — confirmed by `cat`ing into a real kitty (`just debug-posh-bleed-render`).
* The merge hook's `cargo test --workspace` runs the guards.

## More Information

* Bug + full diagnosis: posh#100 (scroll); posh#110 (the rest).
* Follow-up: posh#115 (client-cap-aware BCE — support both non-BCE and BCE, chosen per client terminal).
* Not #42 (backgrounds *dropped*, caused by a missing `TERM`/`COLORTERM`) nor #86 (SGR passthrough).
* mosh reference: `zz-mosh/src/terminal/terminalframebuffer.{h,cc}` (`newrow()`, `insert_line`/`delete_line`), `terminaldisplay.cc` (`can_use_erase`/`has_bce`). mosh-ffi oracle: ADR 0004.
* kitty capability: `infocmp xterm-kitty` (`bce` absent) vs `xterm-256color` (`bce` present); `just debug-term-bce <term>`.
* Repro + round-trip tooling: `just debug-posh-bleed-scroll` (synthetic), `poshterity render` / `just debug-posh-bleed-render` (server→client bytes).
