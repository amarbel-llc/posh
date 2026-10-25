# posh lock bump: readiness note for the operator (2026-10-25)

What a bump of the fleet's posh input to master `14c56d9` (or later) brings, what
to check on flic afterwards, and how to roll back. Read from `git log` and the
code; **nothing here was run on a fleet host**, and I do not know which posh
revision the fleet currently locks. The delta below is counted from the v0.4.3
tag (`eaf5932`), which is an assumption: substitute the real locked rev.
`version.env` still says `0.4.3`, so the version string does not change; the
build identity is `0.4.3+<sha>` and only the sha moves.

## What it brings (v0.4.3 `eaf5932` .. `14c56d9`)

Default-on behaviour changes:

- **Switch routing (posh#242, `14c56d9`).** A session switch is no longer routed
  to a connection that never sent `Init` (a one-shot `posh list` probe). The only
  change a session that never uses the app modal can notice.
- **Predictor population loading (`3d7fd3a`, `5b719ac`, `dfe4f10`).** The RFC 0007
  engine dependency moves from private `mephisto` to public `galapagos`, with
  mephisto references dropped; `3d7fd3a` keeps loading populations saved under the
  legacy mephisto schema tag. Worth a glance if anyone relies on a saved
  population (not exercised by me).

Opt-in, off unless an env var is set:

- **App modal (`d31961c`, `0a9323d`, `81010ef`).** `POSH_APP_MODAL=1`, read from
  the environment of the process that CREATED the session (daemon) and of the
  attaching viewer. Off: requests are answered `disabled` (exit 13) and no viewer
  advertises the cap. Proven inert: `modal_integration` (10 tests) includes
  `a_session_that_never_calls_modal_carries_no_modal_traffic_gate_off_or_on`, an
  800 ms window with the gate off and on; it asserts no `Tag::Modal`, not that a
  live connection exists. See `2026-10-25-first-live-countdown-runbook.md`.
- **Session confinement (`0379629`, FDR 0022).** `POSH_CONFINE=1` refuses a peer in
  an agent-scope cgroup. Off by default. Note: with it on, `posh modal` called
  from clown (an agent scope) would also be refused, because the man page says the
  command is refused like every daemon command; the exit status for that refusal
  was not checked. Do not turn both on for the first countdown test.

No runtime effect:

- poshterity tooling (`ed692c4`, `7f0f3f9`: `record --script`, `seek`, ...), docs,
  tests, flake input bumps (`b5c5293`, `7cbc32b`, `67ceba1`, `04bb8e0`,
  `ac4c106`), and `x86_64-darwin` exposure (`967ea71`).
- `d36f729`, `f93996f`, `3bb4f5e` are prototype commits (debug recipes), in
  history but not product behaviour.

## Check on flic after the bump

1. `posh version` prints the new `0.4.3+<sha>`; `just debug-posh-builds` lists the
   builds running on the host (old daemons keep running until their sessions end).
2. A new session attaches, takes input, and detaches/reattaches as before (nothing
   opt-in is set).
3. `posh modal --probe <session>` on a NEW session with no gate: expect exit 13
   (`disabled`). On a session whose daemon predates the bump (unverified): expect
   something other than a clean answer, since an old daemon does not know the
   modal tag. I did not test an old daemon, and clown kills a hung `posh` after
   45 s, so this is worth one manual try before clown's restart relies on it.
4. Ctrl-^ → Switch session… still moves an idle viewport while `posh list` runs in
   another shell (the #242 case).

## Rollback

Re-lock the previous posh rev and rebuild. Sessions created under the new build
keep their new daemon until they exit. With no opt-in env set, the new build adds
no wire traffic to an old client (the inertness tests cover same-build only);
mixed-version attach was not tested by me, so prefer ending new sessions on
rollback. Nothing is written to disk by the opt-in features.

## Not covered

Anything between the locked rev and `eaf5932`, daemon/client skew across the
bump (the existing version-skew tests in `daemon.rs` are about older clients, not
this delta), and the live behaviour of any opt-in feature on a fleet host.
