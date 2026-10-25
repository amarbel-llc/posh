# First live `clown restart` countdown over posh: runbook

For the operator's first live test of the experimental app modal. Nothing here
was run against a real posh: it is read from posh's code (`session/modal.rs`,
`session/daemon.rs`, `session/client.rs`) and from clown/pure-teak's description
of `cmd/clown/restart_countdown.go` (read in code, not run; clown's own docs are
`man/man1/clown-restart.1` and `docs/plans/2026-10-24-idle-poke-restart-verb.md`
in the clown repo). Anything about clown below is that session's report.

## What must be true for the countdown to show

All four, or the modal is skipped (see "Outcomes"):

1. **The posh daemon was created with `POSH_APP_MODAL=1` in its environment.**
   The gate is read once when the daemon starts (`daemon.rs`, `modal::enabled()`).
   A session already running does not pick it up: create a new session.
   The daemon logs `app modal enabled (POSH_APP_MODAL)` at startup, which is the
   quickest check.
2. **The viewer's own `posh` process also has `POSH_APP_MODAL=1`.** The client
   advertises `CAP_APP_MODAL` only when its own environment has the gate
   (`client.rs`). Setting it only on the daemon side means the daemon accepts
   requests but no viewer is capable, so every request resolves as "no viewer".
3. **The viewer is a local session-attach client on a frames-on session**
   (the `CAP_PROTOCOL_VERSION` client). Roaming, relay and pushed-session
   viewers are not supported (decision 5 in `2026-10-25-countdown-decisions.md`).
4. **clown can reach posh:** `POSH_SESSION` is set in clown's environment and
   `posh` is on its `PATH`.

clown never sets or reads `POSH_APP_MODAL` (its tree has no occurrence), so the
operator sets it where the posh daemon and the attaching client start.

## The opt-in knob

`POSH_APP_MODAL=1`. Any other value, including `maybe`, leaves the feature off
(test `an_unrecognized_gate_value_leaves_the_feature_off`). With it off, posh
behaves as before; a request is answered `disabled`
(`a_session_that_never_calls_modal_carries_no_modal_traffic_gate_off_or_on` and
`without_the_gate_the_daemon_refuses_...` in `tests/modal_integration.rs`).

## What clown calls

1. Only if `POSH_SESSION` is set and `posh` is on `PATH`:
   `posh modal --probe $POSH_SESSION`. clown goes on to the modal only if this
   exits 0 and prints JSON with `"modal":"ok"`.
2. `posh modal $POSH_SESSION --title "Restarting agent" --body "restart requested: <reason>" --timeout 30 --id clown-restart-<clown pid>`.
3. clown kills a hung `posh` after 45 s (30 + 15).

The posh daemon owns the 30 s clock; clown just waits for the command.

## Outcomes, by exit code of `posh modal`

| exit | meaning (posh, `modal::exit_code`) | what clown does (its report) |
|---|---|---|
| 0 | proceed: timeout, "now", or no viewer to ask | restarts |
| 10 | a viewer cancelled | no restart |
| 11 | busy (another modal running) | inline countdown fallback |
| 12 | invalid request | inline countdown fallback |
| 13 | gate off in that daemon | falls under "anything else": proceeds with no second wait. clown reaches the modal only after an `ok` probe, and a disabled probe exits 13, so this is reachable only if the gate changes between the probe and the call |
| other / killed at 45 s | the modal ran | proceeds with no second wait |

The probe prints `"modal":"ok"` for any enabled daemon, including one with no
capable viewer attached (`capable` is a count in the reply). So with the gate on
and **nobody attached**, the probe passes, the modal call returns 0 at once, and
clown restarts immediately, with no countdown shown anywhere. That is
by design today (decision 4) and matches clown's detached behaviour, but it
means a countdown test needs a viewer attached.

## Session not wrapped by posh

No `POSH_SESSION`, or posh missing/probe failing, or exit 11/12: if clown has an
interactive terminal it prints `clown: restart pending (<reason>); restarting in
30s` to stderr, waits 30 s, and cannot be cancelled. If it is not interactive it
restarts at once. The probe collapses disabled, no-viewer and busy into the same
fallback on clown's side; clown only knows 11/12 from the modal call.
A turn starting during the countdown (UserPromptSubmit) aborts that restart if
the modal said proceed; the requester must ask again.

## Steps for the test (not run)

1. In a shell with `POSH_APP_MODAL=1`, create a fresh session
   (`posh attach <name>`), so both the daemon and this viewer have the gate.
2. Check the daemon log for `app modal enabled`.
3. From inside that session (so `POSH_SESSION` is set), trigger `clown restart`.
4. Expect the banner at once, a Ctrl-^ opening the `countdown` view, and:
   let it expire (exit 0, clown restarts); cancel with the button or Esc
   (exit 10, no restart).
5. Repeat with the viewer detached: expect an immediate restart, no countdown.
6. Repeat with no `POSH_APP_MODAL` anywhere: expect the clown inline fallback.

Known blemish: the banner can outlive the closed countdown view by 0 or 1
writes (decision 1 in the decisions doc).

## Rollback

Unset `POSH_APP_MODAL` and create sessions without it. No wire or config state
persists.
