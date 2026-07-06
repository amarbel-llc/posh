---
status: experimental
date: 2026-07-06
promotion-criteria: With a kitty-keyboard-aware app inside the session (Claude Code or neovim) over a real link, Escape and modified/chorded keys reach the app with no swallow, no double-register, and no residual outer-terminal kitty state after detach/exit — validated against a terminal that supports the protocol (kitty/ghostty/WezTerm/iTerm2) and one that does not (mode-sync must be a no-op there).
---

# Kitty keyboard protocol pass-through

## Problem Statement

posh forwards keyboard input as an opaque legacy byte stream. The client puts
the outer terminal in plain termios raw mode (`RawMode::enable(STDIN)`) and never
negotiates the kitty keyboard protocol on stdin, so the outer terminal keeps
sending legacy encodings — including a **bare `\x1b` for Escape**. That bare
Escape is the root of two latency problems:

1. **posh's own detach ambiguity (posh#126).** The `DetachMatcher` recognizes
   Ctrl-\ as its kitty CSI-u forms (`\x1b[92;5u`), so a lone `\x1b` looked like a
   possible detach prefix and was held back until the next keystroke. The
   byte-level fix (require `\x1b[` before treating input as a detach-in-progress)
   removes the swallow, but the *cause* — Escape arriving bare and ambiguous —
   remains.
2. **The inner app's own bare-Escape disambiguation.** A TUI running inside the
   session (Claude Code, vim) that would use the kitty protocol to get an
   unambiguous Escape (`\x1b[27u`, recognized with 0ms) instead receives bare
   `\x1b` and applies its own escape-timeout (Claude Code hardcodes 50ms; see
   anthropics/claude-code#29129). posh sits between the app and the real
   terminal, so even a kitty-capable outer terminal never gets asked.

posh-term is already kitty-keyboard-*aware*: it parses the protocol's
push/pop/set/query requests (`csi.rs`, tracked per-screen in `KittyKeyStack`) and
`dump_vt` replays the stack (`dump.rs`). What is missing is the **plumbing** to
surface the session app's negotiated kitty state to the client and have the
client mirror it onto the real outer terminal. This FDR proposes exactly that
mirror — and nothing more.

## Interface

When an application inside the session enables the kitty keyboard protocol
(pushes flags via `CSI > flags u`, sets via `CSI = flags ; mode u`), posh mirrors
the *current* active-screen flag set onto the outer terminal, the same way it
already mirrors bracketed paste, app-cursor-keys, mouse mode, focus reporting,
and alternate scroll from the server `Snapshot`.

Concretely:

- **`Snapshot` gains `kitty_keyboard_flags: u8`**, populated in
  `Snapshot::from_term` from `term.kitty_flags().0` (both already public in the
  frozen posh-term API).
- **`new_frame` gains a sync block** that diffs `kitty_keyboard_flags` against the
  last drawn Snapshot and, on a change, writes the outer-terminal request:
  - flags become non-zero (or change value) → `CSI = flags ; 1 u` (set the outer
    terminal's flags to exactly the session's current flags — mode 1 = set),
  - flags return to zero → `CSI = 0 ; 1 u` (disable).

  Using the absolute *set* form (`= ; 1`) rather than push/pop keeps the mirror
  stateless: the client asserts the session's current flag value on every change
  without tracking a parallel stack.
- **Teardown resets it.** `display::close_with` (exit) and the local client's
  detach reset sequence add `\x1b[=0;1u` (disable kitty keyboard) alongside the
  existing mouse/paste/focus resets, so a detached or exited session never leaves
  the user's terminal in kitty keyboard mode.

Both the roaming client (`remote/client.rs`) and the local client
(`session/client.rs`) render through the shared `display::new_frame_opt`, so the
single sync block covers both paths.

The effect: when the session app turns kitty keyboard mode on, the user's real
terminal turns it on too. Escape then arrives from the outer terminal as
`\x1b[27u`, flows through posh's byte pipe untouched, and reaches the app
instantly — with no posh detach ambiguity and no inner-app escape timeout.

## Examples

Claude Code running in a posh session on a kitty-capable terminal (ghostty):
Claude enables the kitty protocol; posh mirrors `\x1b[=N;1u` to ghostty; pressing
Escape sends `\x1b[27u`, which posh forwards verbatim and Claude recognizes with
no 50ms delay.

    <Esc>        # outer terminal sends \x1b[27u; instant interrupt, no timeout

Detaching (Ctrl-\) or the session exiting: the client writes `\x1b[=0;1u` in the
teardown sequence, so the user's shell is back in legacy keyboard mode, exactly
as before the session.

An outer terminal with no kitty keyboard support: it ignores the `CSI = ; u`
request (unknown CSI), keeps sending legacy `\x1b`, and posh#126's byte-level fix
still forwards that bare Escape immediately. The mirror is a safe no-op.

## Limitations

- **Pass-through only, not a local key model.** posh mirrors the session app's
  *requested* kitty state to the outer terminal; it does not itself parse the
  outer terminal's kitty input into `KeyEvent`s. Optimistic echo of modified keys,
  key remapping, and client-originated key events are a separate, larger effort
  (approach B) with its own design record — this FDR deliberately does not touch
  the client's raw-byte input path or the prediction engine.
- **Depends on the app opting in.** The latency win only materializes when the
  in-session app actually enables the kitty protocol. An app that never requests
  it still gets bare Escape (and its own timeout, if any). posh does not
  force-enable kitty mode on the outer terminal on the app's behalf.
- **Alt-screen flag independence.** posh-term keeps separate kitty stacks for the
  primary and alternate screens. The mirror follows the *active* screen's flags,
  so switching screens re-syncs — consistent with how mouse_mode already behaves.
  A rapid screen flip could momentarily assert a stale flag set until the next
  frame; the same one-frame settling the other mode-syncs have.
- **Outer terminal is the source of truth for capability.** If the outer terminal
  claims kitty support but implements it partially, Escape encoding is between the
  app and that terminal; posh is a transparent conduit and does not normalize it.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| enable form | `CSI = flags ; 1 u` (absolute set) | stateless mirror; no parallel stack to drift | a terminal that mishandles `= ; 1` but honors push/pop |
| teardown reset | `\x1b[=0;1u` | leave the user's shell as we found it | a terminal that needs an explicit pop-all (`\x1b[<u` repeated) |
| force-enable on capable terminals | off | never enable a protocol the session app didn't ask for | a decision to make posh itself kitty-native (folds into approach B) |

## More Information

- posh-term parse side: `crates/posh-term/src/csi.rs` (the `b'u'` CSI handlers for
  `>`/`<`/`=`/`?` intermediates) and `KittyKeyStack` in
  `crates/posh-term/src/kitty_keys.rs`. Encode side (unused on this path):
  `encode_key` in the same file, with `KeyEventType::{Press,Repeat,Release}`.
- Mode-sync precedent: `new_frame` in `crates/posh-proto/src/display.rs` already
  mirrors bracketed paste / app-cursor / mouse / focus / alternate-scroll from the
  `Snapshot`; this adds one more field in the identical shape.
- Shared render path: both clients call `display::new_frame_opt`
  (`remote/client.rs`, `session/client.rs`), so one sync block covers roaming and
  local.
- Detach interaction: with kitty mode on end-to-end, Ctrl-\ arrives as
  `\x1b[92;5u` (already a full detach match) and Escape as `\x1b[27u` (distinct);
  composes cleanly with the posh#126 byte-level fix.
- Related: anthropics/claude-code#29129 (the inner app's 50ms bare-Escape
  timeout this pass-through neutralizes for kitty-capable terminals).
