# Countdown modal: decisions awaiting the operator

Status of the code: the prototype is on master, opt-in (`POSH_APP_MODAL=1`),
experimental cap id 240. See `2026-10-24-app-modals-for-restart.md` for the
findings. Each item: the question, the options, a recommendation. Every "R:"
line is a judgment, not a finding.

1. **Banner vs. view flicker.** The recorded run shows the banner can outlive
   the closed `countdown` view by 0 or 1 writes (a race between the view's
   `ui.hide` and the daemon's next `show` tick). Options: (a) accept it;
   (b) have the daemon send `clear` before the next `show` whenever a viewer
   reports the view closed; (c) make the banner draw the single source of
   truth and let the view only overlay it.
   R: (c). It removes the race by construction and costs one draw path.

2. **Concurrent modals.** Today: one at a time, a second request is `busy`
   (exit 11). Options: (a) keep `busy`; (b) queue the second; (c) preempt by
   priority. R: (a). A restart countdown is the only caller; queued or
   preempting modals invite a restarter whose timeout expires while waiting.
   Revisit when a second caller exists.

3. **Dismiss meaning.** Today `Esc` = cancel by default, `--on-dismiss hide`
   keeps the countdown running. R: keep cancel as the default. Dismissing a
   destructive countdown should fail safe (the restart does not happen).

4. **Last viewer leaves.** Today the countdown resolves `no-viewer` and the
   caller proceeds. Option: keep running and apply `on_timeout`. R: keep
   proceeding; add a `--hold-without-viewer` flag only when a real caller
   needs the other behavior. Not needed now.

5. **Roaming, relay, and pushed-session viewers** are unsupported. R: leave
   unsupported until the restarter actually needs them; the daemon-owned clock
   makes adding them additive.

6. **Landing form.** An RFC for `Tag::Modal` and a registered cap id would
   replace the experimental 240. R: no RFC until UX feedback is in; the
   removal checklist in `session/modal.rs` stays valid until then.

## Re-read against clown 720ffd7 / 613bf9a (2026-10-25)

What clown implements, as reported by clown/pure-teak from
`cmd/clown/restart_countdown.go` (read in code, nothing run against a real
posh). Full account in `2026-10-25-first-live-countdown-runbook.md`.

- clown probes (`posh modal --probe`), requires `"modal":"ok"`, then calls
  `posh modal ... --timeout 30`. Exit 10 = cancelled; 11/12 = fall back to an
  inline 30 s countdown; anything else = proceed. The daemon owns the clock.
- clown never sets `POSH_APP_MODAL`; the operator must.

What changes in the recommendations:

- **Decision 2 (concurrent modals): stronger.** clown already treats `busy` (11)
  and `invalid` (12) as "posh showed nothing, use the inline countdown", so
  keeping `busy` costs the only caller nothing. Unchanged: (a).
- **Decision 3 (dismiss): confirmed.** clown maps exit 10 to "no restart" and
  every other code to proceed, so cancel-by-default is the fail-safe mapping.
  Unchanged: keep cancel. Note `hide` (countdown keeps running) would end in a
  timeout, i.e. exit 0 and a restart.
- **Decision 4 (last viewer leaves): the cost is now visible.** With the gate on
  and nobody attached, the probe still says `ok` (`capable` is a count), the
  modal returns 0 at once, and clown restarts with no countdown. Same as clown's
  detached behaviour, so still R: keep proceeding. But clown's probe cannot tell
  "no viewer" from "viewer present"; if the operator wants a countdown to be
  mandatory when posh is in use, the probe's `capable` count is the field clown
  would have to read. That is clown's change, not posh's.
- **New 7. Where the gate is set.** clown does not set it, and it must be in both
  the daemon's creation environment and the attaching client's. R: for the first
  live test, set it by hand per the runbook; decide a default (or a clown-side
  knob) only after that test. A default-on gate changes every posh session on
  the fleet, which the opt-in design exists to avoid.
- **New 8. Probe resolution.** clown collapses disabled, no-viewer and busy into
  one fallback. R: no posh change; if clown wants the distinction it can read
  `modal`, `capable` and `active` from the probe JSON that posh already prints.
