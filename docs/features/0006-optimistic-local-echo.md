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

## Slow-link auto-escalation (2026-08-23): the A/B, run live

The promotion criterion above asks for an A/B against `adaptive` over a real
~100 ms link. A field report ("this link should be using local echo
prediction exclusively, the latency is very bad") surfaced a fact worth
recording: on a slow link `adaptive` and `always` paint the SAME thing —
adaptive's `srtt_trigger` is already on past mosh's 30 ms threshold, and
both keep the tentative-epoch gate, so the first keystrokes after Enter or
any control key stay hidden for a full RTT. That gap is what a slow-link
user feels, and `optimistic` is the one model that removes it while keeping
the ECHO-off / alt-screen gates (mosh's `experimental` shows tentative
cells but has no ECHO gate).

So the client now runs the A/B itself: with the model left on its default,
`predict::EchoEscalation` switches the session to `optimistic` once the
wire's SRTT has held above 150 ms for 3 s, and back to `adaptive` after it
has held under 80 ms for 15 s (hysteresis + asymmetric holds so a jittery
link does not flap and a mid-outage good moment does not yank the echo).
Any explicit model — `POSH_PREDICTION_MODEL` (even `adaptive`: naming it
is a choice, unlike leaving it unset), or the palette's `Echo:` commands —
pins and bypasses it; the palette's `Echo: adaptive` is the one re-arm.
`POSH_ECHO_ESCALATE=0` opts out (default-on gate shape). The machine acts
only on a MEASURED SRTT (the estimator's 1000 ms pre-sample placeholder
would otherwise escalate every fresh connection), and a link that settles
between the thresholds stays escalated by design (optimistic is no worse at
120 ms; recovering at the escalate threshold would flap). The palette
heading carries the live `rtt` and `echo: optimistic (auto)` while
escalated (kept under posh-palette's ~42 content columns); "Show echo
prediction stats" reports the state and thresholds; each switch is
bannered (never over a sticky wedge notice) and logged (`echo` tag) with
the SRTT at the edge AND the outgoing model's outcome counters — the
predictor is rebuilt on a switch, so that log line is where the A/B data
survives.
`remote::predict::tests::echo_escalation_holds_hysteresis_and_yields_to_explicit_choices`
and
`remote::client::tests::echo_set_pins_the_model_against_slow_link_escalation_and_adaptive_rearms`
pin it.

**First A/B finding (2026-08-23, the same day): the cursor sat visibly
offset from its real position on the escalated link.** Two causes, both
fixed:

1. The default relay bootstrap (RFC 0008 §3) stamped `echo_ack = input_ack`
   — a `TODO(3.1b)` shortcut justified by "the happy path has no
   optimistic-echo client". Acking input as echoed the instant the relay
   received it retired optimistic's predictions BEFORE the frame carried the
   echo: the predicted cursor was dropped, the display snapped back to the
   frame's pre-echo cursor, and the real echo frame landed an RTT later — a
   cursor that jumped backwards on every keystroke. The relay (and the M2
   per-channel bridge) now keep mosh's `EchoAck` maturity like the roaming
   `server_loop` always did, with two deliberate divergences from it: a
   matured ack is delivered even while a frame is outstanding (the relay's
   held bytes are pre-encoded with the older ack, where `server_loop`
   re-encodes on retransmit and so gates its force-ack behind
   no-outstanding-frame), and the queue-then-flush loops restamp pending
   entries under daemon backpressure (`EchoAck::restamp_pending` — the
   grace period counts from when the buffer last drained, since
   `server_loop` writes to the PTY before recording and never queues).
   RFC 0008 §3 now states the contract. (Adaptive was mis-validated by the
   same mirrored ack — a contributor to its nocredit/reset noise, posh#91.)
2. Optimistic never validated CURSOR predictions against the frame: it
   retired them on ack only, so when the server contradicted an acked one (a
   prompt redraw, an autosuggestion, the post-Enter prompt) the newer
   predictions chained from the wrong spot stayed painted until each was
   acked in turn. `OptimisticPredictor::cull` now drops the whole chain the
   moment the server contradicts an acked prediction and re-seeds from the
   frame on the next keystroke (cells stay optimistic; only the cursor
   defers — optimistic never argues with the server about where the cursor
   is). Counted in `mispredict_resets` as the A/B gauge.

**Second A/B finding (2026-08-23, later the same day): "local echo has
stopped entirely" on the escalated link (SRTT ~3.5 s).** The `ECHO`-flag
gate this record specifies was never produced on the default path: the
only writer of `FLAG_ECHO` was the roaming `server_loop` — the session
DAEMON set `flags: 0` on every frame, so on the relay path the client's
gate read "echo off" for the entire session and optimistic predicted
nothing. The escalation therefore downgraded a slow link from
working-adaptive to silently-dead-optimistic. Fixed: the daemon stamps the
active pty's ECHO state onto every frame it produces (visible and
scrollback; `ClientConn::echo_flag`, refreshed per loop iteration,
overlay-aware like `server_loop`), and the relay and M2 bridge re-stamp
the last daemon frame's `FLAG_ECHO` onto their own Empties — the client
reads the gate off EVERY frame's flags, so a bare-0 Empty (heartbeat, ack
carrier) flipped the gate off between visible frames. Log review of the
incident session remains queued to confirm nothing else contributed.

What this buys the promotion question: every slow-link session is now an
A/B sample — `optimistic` in effect exactly where it matters, `adaptive`
everywhere else — with the switch edges in the client log. Observations to
collect against the criterion (echo leaks at password prompts, flicker,
full-screen apps) before deciding whether `optimistic` becomes the default
outright.

**Known limitations of the cursor-verdict + matured-ack machinery** (from
the same review): against an OLD posh-server whose relay still mirrors
`echo_ack = input_ack` instantly, the strict cursor rule can fire on every
frame that races an echo — mixed-version sessions are noisy for optimistic
(they always were broken for it; the premature retirement predates this
work) and their `mispredict_resets` numbers must be excluded from the A/B;
posh#164's cutover design is the answer, not a client-side heuristic. The
verdict now follows mosh exactly: only the chain's NEWEST entry, once the
echo ack passes it, carries a verdict (a mid-chain ack proves nothing about
the head, which stays painted for at most ~1 RTT), an out-of-bounds head
(resize) clears silently without counting, and the relay/bridge defer echo
maturation while a frame is outstanding — so a verdict is only ever formed
against a screen that contains the echo it claims.

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

## Predictor / renderer split (2026-08-25)

Observed on a pinned `always`: no underline, and a visible hold on the first
keystroke after Enter. Both were mosh policy living in the render path — the
slow-link `flagging` (send interval >80 ms) chose the underline, and the
tentative-epoch gate hid a new epoch's first cell for a round trip. These are
render-UX choices, orthogonal to what a model predicts. Adaptive's srtt/glitch
"show" trigger turned out to be the same kind of thing (the model records
predictions regardless; the trigger only gated painting), so all three moved:
the model now hands the renderer a `RenderAdvice { show, flag,
confirmed_epoch }` — a recommendation — and the renderer's `ShowPolicy`
(`POSH_PREDICTION_SHOW`) decides. `always` (default) paints every held
prediction immediately and marks every cell, so `adaptive` and `always` look
identical and the "dim optimistic echo: off" lever below is superseded (`dim`
vs `replace` picks faint vs underline). `advised` honors the advice — mosh's
original behavior as a render choice. The model keeps WHAT: the machinery,
`never` (records nothing), and the safety gate — which is now applied
universally by the client (RFC 0007 §5.1: no model renders while the remote
PTY has ECHO off or the alt-screen is up), since the mosh hold no longer
masks a password prompt's first keystroke.

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
