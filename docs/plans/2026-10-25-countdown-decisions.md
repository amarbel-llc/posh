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
