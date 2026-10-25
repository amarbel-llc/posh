# What generalizes beyond the restart countdown (options for the operator)

Design note, no code. Date: 2026-10-25. Claim labels as in
`2026-10-24-app-modals-for-restart.md`: **R** read in code/docs (source named),
**I** inferred, **V** observed by running. Everything here is **R** or **I**
unless stated; the only run is the unit/integration tests listed.

## 1. What the prototype already carries that is not restart-specific

- **R** The wire request (`session/modal.rs`, `parse_request`) is already a
  general shape: `id`, `title`, `body`, `buttons` (a list of `{id,label}`),
  `timeout_ms`, `on_timeout` (must name a button), `on_dismiss` (must name a
  button, or `hide`). Nothing in the daemon fixes the buttons to cancel/now.
- **R** The daemon owns the clock and the arbiter (first answer wins); one modal
  runs at a time (`busy`); no capable viewer resolves `no-viewer` at once;
  `modal_handle`/`modal_drive`/`modal_resolve` in `session/daemon.rs`.
- **R** The viewer sees the banner always and the `countdown` view on Ctrl-^
  (`posh-palette/countdown.go`; `Palette::show_countdown/update_countdown/hide`).
  Buttons arrive as renderer commands that choose (`modal.rs`
  `the_buttons_become_renderer_commands_that_choose`).

## 2. What is restart-specific today

- **R** The CLI. `posh modal` builds the fixed two-button form (cancel/now,
  `modal.rs` ~line 439) from `--title --body --timeout --on-dismiss --id`; it
  does not take a button list.
- **R** The exit codes (`modal::exit_code`): 10 = the button named `cancel`
  was chosen, 0 = everything else that proceeds. For any other button set the
  caller would have to read the printed JSON, because `button` is not mapped to
  a code.
- **R** The view: `countdown` draws a progress bar and a "time remaining"
  line; its wording assumes a clock that runs out into an action.
- **R** The gate and the capability: `POSH_APP_MODAL`, `Tag::Modal = 19`,
  `CAP_APP_MODAL = 240` (experimental band; RFC 0001 says that band must not
  appear in released builds). The removal checklist is in `session/modal.rs`.
- **R** Audience: local `posh attach` viewers on frames-on sessions only
  (roaming, relayed and pushed viewers are never offered it).

## 3. Candidate generalizations

A. **Keep it restart-only.** Rename nothing, document it as "the restart
   countdown". Smallest cost; the wire shape is already general enough that
   widening later is additive.

B. **A confirm/choice modal for any caller (no clock, or a clock with a default).**
   Same request, same daemon; add a `--button id=label` repeatable flag and a
   `choice` rendering of the same view (no progress bar when `timeout_ms` is 0).
   Callers: anything that wants a yes/no before a destructive action in a
   session (a deploy tool, a `sc close` with unmerged work). Needs: a CLI
   button list, an exit-code rule for arbitrary buttons (for instance: print the
   chosen id on stdout, exit 0 on any button, reserve 10-13 for the
   non-button outcomes), and a no-clock rendering. **I**: roughly one small CLI
   change plus a Go branch in the existing view.

C. **A notice (no buttons, must-dismiss).** Server-initiated "something
   happened" shown to the viewer, for example #221 (lost-contact as a modal) or
   #218 (the exited session's last screen). **R** The `notice` view and
   `Palette::show_notice` already exist for the client's own use (pop); the new
   part is letting the DAEMON or a CLI raise one. That is a different trust
   question from B: the viewer would show text an arbitrary same-user process
   chose, over the user's session. FDR 0022 confinement would gate agents;
   nothing gates other same-user processes.

D. **Apps contribute palette entries (#220).** A different mechanism: an app
   registers commands that appear in the Ctrl-^ palette and run an action. It
   shares the renderer and the action protocol (RFC 0005 `action`), but not the
   daemon-owned clock or arbiter. **I**: do not conflate it with B/C; it needs
   its own registration channel, lifetime and trust rules.

E. **Viewer-initiated modals (#194 and similar).** The daemon reports a fact
   (exit cause), the viewer decides what to show. Already how `Tag::ExitCause`
   works; not an app-modal API.

## 4. Decisions the generalization forces (all open)

1. **Who may raise a modal.** The prototype relies on same-user plus optional
   FDR 0022 confinement. Anything wider than the restarter (B, C) makes "which
   caller" a real question. Options: (a) any same-user process (today), (b) only
   processes outside the agent cgroup (needs `POSH_CONFINE`, which is off by
   default), (c) a per-session allow-list. Recommendation: (a) for B, with
   confinement documented as the guard; do not do C until (b) or (c) exists.
2. **Concurrency.** One at a time (`busy`) holds while there is one caller kind;
   with B and C a notice arriving during a countdown is plausible. Options:
   keep `busy`, queue notices behind modals, or let a higher-priority one
   preempt. Recommendation: keep `busy` until a second caller exists
   (the decisions doc, item 2).
3. **Result contract.** Exit codes cannot scale to arbitrary buttons. Options:
   JSON on stdout as the contract and codes only for the non-button outcomes, or
   keep the restart codes as one profile. Recommendation: JSON on stdout is
   authoritative; the restart codes stay documented as the `countdown` profile.
4. **Surface it as a record.** Moving off the experimental id 240 means an RFC
   for `Tag::Modal` plus a registry row. Recommendation: no RFC until the first
   live countdown test has produced UX feedback (the operator's own sequencing
   in the 2026-10-24 note).
5. **Roaming and relayed viewers.** Unsupported today; they matter for
   #221-style notices (a remote viewer is the one that loses contact).
   Recommendation: leave unsupported until a caller needs it; the daemon-owned
   clock makes adding them additive (decisions doc, item 5).

## 5. Recommendation

Do A now, and B only when a second caller exists that is not the restarter. The
request shape needs no change for B, so waiting costs nothing; building B first
adds CLI and exit-code surface with no consumer to shape it. Treat C and D as
separate designs (#221, #220), not extensions of this API, because their trust
and lifetime questions differ from the restart countdown's.
