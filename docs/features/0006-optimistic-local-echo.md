---
status: experimental
date: 2026-06-14
promotion-criteria: A/B'd against `adaptive` over a real ~100ms link across line editing, autosuggestions, a password prompt, and a full-screen app, with no echo-leak or flicker regressions.
---

# Optimistic local echo (local echo correction)

## Problem Statement

posh inherits mosh's *predictive* local echo: keystrokes are echoed as
speculative overlay cells that stay hidden until an epoch / confirmation /
credit handshake decides they are trustworthy. That machinery is intricate and
has sharp edges — typing along a fish autosuggestion starved confirmation credit
and hid local echo entirely (fixed separately), and the first keystroke after
any control key or Enter is invisible for a full round trip while a fresh epoch
re-confirms. This proposes the opposite model: write the echo to the screen
*immediately* and let the next authoritative server paint correct it — reframing
"predict, then reveal once confident" as "echo now, correct on repaint."

## Interface

`POSH_PREDICTION` gains a new value, `optimistic`, alongside the existing
`adaptive` (default), `always`, `never`, and `experimental`.

In `optimistic` mode:

- Each printable keystroke is written to the local display at once at the cursor
  (advancing the cursor), with **no tentative/epoch gating** — there is no
  hidden-until-confirmed state, no credit accounting, no glitch/flag triggers.
- Every server frame repaints authoritatively. Where the optimistic echo already
  matches the paint, the user sees nothing change; where it differs, the paint
  silently corrects it.
- Echo is **suppressed** whenever either gate says so:
  - the server's **alternate screen is active** (a full-screen app — vim, less,
    a pager), or
  - the remote PTY's **`ECHO` termios flag is off** (password prompts, raw-mode
    line editors).

The `ECHO` flag is a new signal posh does not have today. The server reads it
(`tcgetattr` on the pty master) and forwards it to the client on the server
frame (the same per-frame capability channel used by the exit-status and
scrollback caps), re-sent promptly whenever it flips.

## Examples

Typing a command at a fish prompt over a 100 ms link — every character appears
instantly; autosuggestions arrive from the server paints; no hidden first char:

    $ ls -la            # each keystroke echoes locally at once; the paint confirms

A `sudo` password prompt — the server reports `ECHO` off, the client suppresses
local echo, so nothing is shown locally (no leak), matching the remote:

    [sudo] password for user:     # keystrokes not echoed locally (ECHO-off gate)

A `vim` session — the alternate screen is active, so optimistic echo is
suppressed and navigation keys never flash a literal character before the app
repaints.

## Limitations

- **ECHO-flip race.** The server's `ECHO`-off signal must reach the client
  before the next keystroke. Between landing on a password prompt and the flag
  arriving (up to ~½ RTT), a keystroke could be echoed locally and then
  corrected — a brief leak window. mosh's adaptive model has an analogous
  window. Mitigation: send the flag promptly (not pacing-gated) and, optionally,
  a short post-mode-switch guard (see Tuning Levers).
- **Wrong echoes flicker for up to ~1 RTT.** In the gaps the gates do not cover
  (e.g. an app that stops echoing without clearing `ECHO`), an optimistic echo is
  visible until the correcting paint lands — the deliberate trade for
  always-instant echo. `adaptive` hides these at the cost of latency.
- **Cursor-only.** Optimistic echo handles printable insert/overwrite and cursor
  advance. It does not predict the *result* of Enter, control keys, or escape
  sequences — those wait for the paint, same as `adaptive`.
- **Opt-in, not a replacement.** `adaptive` remains the default; `optimistic` is
  selected explicitly until the A/B and promotion criteria settle.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| dim optimistic echo | off | unconfirmed echo *could* be visually marked, but dimming every keystroke is noisy and unlike a local terminal | users report flicker/uncertainty, or conversely find dimming distracting |
| post-mode-switch guard | 0 ms (none) | rely on the `ECHO` flag alone; any guard adds keystroke latency | an observed password leak in the `ECHO`-flip race |
| insert vs overwrite | insert | matches typical shell line editing | shells/apps where insert mispaints the line |

## More Information

- The `adaptive` prediction engine — a port of mosh's `terminaloverlay.cc`
  (epochs, confirmation, SRTT/glitch/flag triggers) — is the model this replaces
  for opt-in users. The fish-autosuggestion credit-starvation fix that motivated
  this exploration restored credit by making the no-credit guard rendition-aware.
- The `ECHO` flag rides the same per-frame capability mechanism as RFC 0001's
  exit-status cap and RFC 0002's scrollback cap.
- Validated by `PredictHarness`, the deterministic state machine that drives the
  prediction path through the real `dump_vt` re-parse round-trip; it asserts both
  the adaptive credit invariant and (once built) the optimistic gating.
