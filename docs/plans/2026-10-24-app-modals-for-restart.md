# App-presented modals in posh sessions (for a restart countdown)

Status: design note, read-only research (no code, no merge). Date: 2026-10-24.
Origin: spinclass#419 (session self-/child-restart) wants attached, posh-wrapped
sessions to show a "restarting in N s — [Cancel]" before clown restarts the agent.
Scope: posh's side only. Clown's and spinclass's sides are other lanes.

Claim labels: **V** observed by running something, **R** read in code/docs,
**I** inferred. Nothing below is **V**: this session was read-only and ran no
posh binary (no build, no tmux drive). Every **R** names its source. Anything
marked **I** is a theory to verify before building on it.

## 1. Does posh have a command palette? (yes, client-side)

- **R** Yes. FDR 0009 (`docs/features/0009-command-palette.md`, status
  experimental) and RFC 0005 (`docs/rfcs/0005-palette-control-protocol.md`).
  `Ctrl-^` opens it. The user's `Ctrl-^` is intercepted by the client's raw
  stdin loop (raw `0x1e` and the kitty CSI-u forms, `session/client.rs`
  `EscapeEvent::Palette`).
- **R** Who renders: **only the client**. The client spawns `posh-palette`
  (a separate Go/Bubble Tea v2 module, `posh-palette/`) on its own PTY plus a
  control socketpair (child's fd 3), tracks that PTY's screen with a
  `posh_term::Terminal`, and composites the non-blank region onto the session
  `Snapshot` (`remote/palette.rs`, `composite_palette`). The renderer stays
  resident between summons. The server/daemon never sees the palette; there is
  no wire format for it (RFC 0005 intro: "the rendering path needs no new wire
  format").
- **R** Both clients have it. FDR 0009's Limitations say "roaming client
  only", but `session/client.rs` now has `open_local_palette`,
  `palette_commands`, and `raise_pop_notice` (posh#87 convergence landed). The
  FDR text is stale on this point. Gate: the local attach palette is
  **frames-on only** (a client that negotiated `CAP_PROTOCOL_VERSION`);
  gate-off, `Ctrl-^` stays in the input stream (`EscapeEvent::feed`,
  `palette_enabled`).
- **R** Views the renderer implements (RFC 0005 §3.2): `palette` (filterable
  command list), `dialog` (read-only text + Copy + dismiss), `picker`
  (filterable table; session switcher), `notice` (must-dismiss, shown
  unprompted, lists the session stack). Selecting a command makes the renderer
  issue the command's `action` as a JSON-RPC request back to the client
  (`{method, params}`, RFC 0005 §4.1/§7: `echo.set`, `logging.set`,
  `shell.open`, `client.suspend`, `app.quit`, `session.list`, `session.switch`,
  `session.pop`, `record.set`, `debug.banner`). Esc yields `ui.cancelled`.
  The renderer is deliberately generic: opaque `name` + `action`.
- **R** Precedent for an UNPROMPTED modal: the `notice` view. The client raises
  it itself after an automatic pop (`Palette::show_notice`, `remote/palette.rs`;
  `raise_pop_notice`, `session/client.rs`). It must be dismissed and MUST NOT
  dismiss on an unbound key, because a keystroke meant for the session must not
  be swallowed (RFC 0005 §3.6). The restart countdown has exactly this shape.
- **R** While the palette is open it owns the keyboard; the session keeps
  running underneath but receives no input (FDR 0009 Limitations). For a
  countdown this is a feature for Cancel and a hazard for typing (see §3).
- **R** Fallback when `posh-palette` cannot launch: `Ctrl-^` becomes a one-shot
  emergency prefix that can only quit (`Ctrl-^ .`) or send a literal `Ctrl-^`.
  No modal exists then.

## 2. Can an app (or the session's parent) ask posh to show something?

**Not today.** There is no server→client "show this view" message and no
app-facing modal API. What does exist:

- **R** The renderer control channel (RFC 0005) is client↔renderer only (fd 3
  socketpair). Apps cannot reach it.
- **R** The session daemon IPC socket (Unix, same user, hardened socket dir;
  `session/ipc.rs`). Tags today: Input, Output, Resize, Detach, DetachAll, Kill,
  Info, Init, History, Run, Ack, Exit, Frame, Shell, FrameAck, ClientCaps,
  SwitchRequest, Switch, ExitCause. None presents UI. Relevant:
  - `Run` types a command into the PTY as input (`posh run`); it is input
    injection, not a modal.
  - `SwitchRequest` (in-session `posh attach <sibling>`, FDR 0012) is the
    existing **app → daemon → one viewport** request whose reply is `Switch`
    routed to "most-recent-input attached client". It is the closest precedent
    for "a process asks, the daemon forwards to a viewport".
  - `Info`/`posh status` is the RFC 0014 status socket (read-only).
- **R** Daemon → client signalling today rides the RFC 0001 capability table on
  `ServerFrame`s: `SESSION_KIND` (id 20), `EXIT_CAUSE` (19), `SESSION_ACTIVITY`
  (15), `PUSH_CMD` offer (21), plus `Tag::Switch`/`Tag::Exit` on the local
  socket. The registry runs to id 24 (`PACED`, `SCROLLBACK2_EXTENT`).
  A new server→client entry follows an established pattern, including the rule
  that an unknown entry is skipped (old clients ignore it).
- **R** In-band path: apps can emit escape sequences; posh-term parses OSC 0/1/2
  (title; surfaces as the `SESSION_ACTIVITY` terminal-title field), 4, 7 (cwd),
  8, 10-12, 52, 66, 133 (`posh-term/src/osc.rs`). There is no OSC that raises a
  posh UI. The client never reads app escape sequences for UI; it renders the
  daemon's snapshot.
- **R** Viewport-side introspection is a status socket
  `<base>/viewports/<pid>.status.sock` (RFC 0014), and `posh status --viewport`
  lists the overlay views (`palette`/`picker`/`leave`) currently showing. Read
  only.
- **R** FDR 0022 confinement: with `POSH_CONFINE=1` the daemon refuses
  connections from peers in a denied cgroup (checked at `accept`, via
  `SO_PEERCRED` and `/proc/<pid>/cgroup`; `session/confine.rs`). **This applies
  to every verb on the daemon socket.** A modal-request verb called from an
  agent-scoped process (e.g. the claude/clown process itself) would be refused,
  by design. The restarter must run outside the agent scope or be allowed.
  (**I**: spinclass/clown would be the orchestrator outside the agent's
  cgroup, not the agent. Needs confirming in their lanes.)
- **R** The posh server in roaming mode with `posh server` owns its PTY directly
  (no daemon socket) and is single-peer (CLAUDE.md "Key design facts"). The
  default remote attach is the M2 mux path where a session daemon sits behind
  the server, so the daemon-socket verb is reachable remotely via the daemon on
  the session's host. The ephemeral mode has no daemon and no such path.

## 3. Smallest design: app-presented modal reusing the palette

Principle: the client already owns the renderer; add exactly one new
**request path to the client** and one new **renderer-agnostic view** (or reuse
`dialog`/`notice` with an action list). Do not give apps the renderer.

### 3.1 Request path (app → daemon → viewports)

- New daemon IPC verb `Tag::Modal` (id 19, next free): payload = JSON (or the
  caps-style TLV) request. Sender: any same-user process with the session name
  (the `posh` CLI is the paved path: `posh modal <session> ...`; env
  `POSH_SESSION`/`POSH_GROUP` for in-session callers). Subject to FDR 0022.
- The daemon fans the request to **every attached, modal-capable viewport** (not
  "most recent input" as `Switch` does: a restart countdown concerns all
  viewers; see §4 for why every viewer must be able to cancel).
- Daemon → viewport: a new server cap entry, e.g. `APP_MODAL` (id 25, server→
  client, payload = the request), gated by a client capability offer exactly
  like `PUSH_CMD`: the viewport sends an empty `APP_MODAL` entry in its Init if
  it can render modals; the daemon never sends it to a viewport that did not.
  The relay does not forward unknown entries (ADR 0007 pattern from
  `PUSH_CMD`), so a relayed viewport is not offered it. **I**: the relay
  exclusion is a copy of RFC 0016 §2; verify the exact rule for a new id.
- Viewport → daemon: the user's choice returns as a client cap entry
  `APP_MODAL_RESULT` (id 26: token + chosen action id), mirroring
  `PUSH_CMD_REQUEST`: tokened, re-sent by a roaming viewport until the daemon
  acks by clearing/superseding the modal (idempotent on a retransmit).
- Daemon → requester: the result is reported on the connection that sent the
  `Modal` request (new reply tag, or the verb blocks and prints the choice;
  see below). If the requester's connection is gone, the daemon still applies
  default-on-timeout and records the outcome (queryable by `posh status`).

### 3.2 Request shape

```
modal {
  id:        <string, caller-chosen idempotency key>
  kind:      "countdown" | "choice" | "notice"
  title:     "Restarting agent"
  body:      "claude restarts in {remaining}s (spinclass: self-restart)"
  buttons:   [ {id:"cancel", label:"Cancel restart", default:false, key:"c"},
               {id:"now",    label:"Restart now"} ]
  timeout_ms: 30000            # countdown length
  on_timeout: "now"            # button id taken when the clock reaches 0
  on_dismiss: "cancel"         # Esc / dismiss; see cancel semantics
  min_viewers: 0               # 0 = proceed even if none attached (see §3.4)
}
```

Result (to the requester, once, final): `{id, outcome: "button"|"timeout"|
"dismiss"|"no-viewer"|"superseded", button: "cancel"|"now"|null, by: <viewport
id or null>}`.

### 3.3 Rendering: reuse the palette renderer

- **Cheapest path**: no renderer change. Map the modal to the existing
  `palette` view: `title`, a `description` (RFC 0005 §3.2 field, "free text
  shown between heading and filter input"), and `commands = buttons`, each with
  an action `{method:"app.modal.choose", params:{id, button}}`. The client adds
  `app.modal.choose` to the §7 method registry (`-32601` on clients lacking it,
  per the registry rule). Esc → `ui.cancelled` → client sends `dismiss`.
  This reuses `Palette::summon`, `composite_palette`, key handling, and
  needs zero changes to `posh-palette`. **I**: confirm `description` is
  honored for `palette` in the current renderer build (the RFC says so;
  not run).
- **Countdown tick**: RFC 0005 says a second `ui.show` while a view is up
  REPLACES it. So the client re-sends `ui.show` once per second with updated
  `description`. **I**: this may flicker the filter-input selection state each
  tick; if so, add a small additive renderer feature (`kind:"countdown"` view,
  or `ui.update` notification) — additive per RFC 0005 §9, an old renderer
  answers `-32602` and the client falls back to a static modal + banner.
- **Input hazard**: while any view is open the renderer eats all keystrokes. A
  countdown that pops up while the user is typing a prompt would swallow their
  typing and send an unintended Enter/letter to the modal. Mitigation (copying
  the `notice` rule): ignore unbound keys (no accidental dismiss), bind buttons
  to **distinct, hard-to-hit-by-accident keys** (`Ctrl-C`, `c`), and prefer
  showing the countdown as a non-key-stealing **banner** for the first N
  seconds with the modal only on explicit `Ctrl-^`. This is a UX decision for
  the operator (Q2 below).

### 3.4 Cancel, timeout, default action

- **Cancel**: any viewer selecting `cancel` ends the modal for everyone
  (first-wins at the daemon; the daemon is the arbiter, not the viewports).
  Other viewers' modals close with a short notice "cancelled by <viewer>".
- **Timeout**: the **daemon owns the clock**, not the viewports. Viewports
  display a countdown derived from the daemon's absolute deadline (carried as
  remaining-ms in each re-sent entry, so skew and roaming do not matter —
  see §4). At 0 the daemon resolves with `on_timeout` and reports it to the
  requester. A viewport crash/disconnect never changes the outcome.
- **Dismiss**: Esc = `on_dismiss`. For the restart case the safe default is
  `cancel` (a stray Esc must not authorise a restart); `on_timeout` is the
  explicit "proceed" (that is the whole point of a countdown). Both are the
  requester's explicit choice, defaults stated in the request.
- **Superseded**: a second `Modal` with the same `id` replaces (idempotent
  retry); a different `id` while one is open queues behind or is rejected
  (`busy`); pick one (Q4).

### 3.5 No client attached (detached session)

- **R** A session is owned by a daemon; with zero clients it keeps running
  (posh(7) PERSISTENCE). Nothing can display or answer.
- Proposal: `min_viewers` in the request. With the default `0`, the daemon
  resolves immediately with `outcome:"no-viewer"` after a short grace
  (e.g. 2 s, to let a mid-reconnect roaming viewport reappear) and the caller
  proceeds (restart now). With `min_viewers>=1` the daemon holds the modal until
  a viewport attaches (it is delivered on attach, §4) or the deadline passes.
  Recommendation for restart: `0` — nobody is watching, so no countdown is
  owed. The caller decides; posh reports facts.
- A detached session is also the case where the caller should not wait the full
  countdown. Caller-side policy, not posh's.

### 3.6 Not posh-wrapped / unsupported path (fallback)

posh must expose a cheap capability probe so the caller can choose a fallback
without guessing:

- `posh modal --probe <session>` → `{daemon:"modal"|"old", viewers:N,
  capable:M, relay:K}`. **I**: derivable from the existing `Info`/status
  response plus the new cap; not in the code today.
- Not in a posh session at all, or the daemon predates the verb (`Info` shows
  no `modal` field → treated as "old"), or the verb is refused by confinement:
  the caller falls back to its own channel (terminal message via `Run`-style
  output is NOT an option — it injects into the app's input). Realistic
  fallbacks, owned by clown/spinclass, not posh: write a banner to the agent's
  stdout/stderr; OSC 9/777 desktop notification via the outer terminal; or
  restart without a countdown. posh should document "no modal means nothing
  was shown", never pretend.
- Viewports that are not modal-capable (old client; relayed viewport; a
  palette-less fallback-prefix client; gate-off local attach): the daemon
  counts them as `viewers - capable`. The caller sees "N attached, 0
  capable" and decides (typically proceed without waiting).

## 4. Roaming and persistence during a countdown

- Principle (**I**, follows from "the session never waits on a viewport" and
  "daemon owns state"): the **modal is session state held by the daemon**, not
  viewport state. Viewports are stateless renderers of "modal M, deadline D".
- **R** Roaming (IP change) keeps the same viewport; the transport resyncs and
  the server re-sends current state on frames. Because the modal rides repeated
  cap entries on frames (like `SESSION_ACTIVITY`/`SESSION_KIND`) and carries
  remaining-ms, a roam or lost datagram self-heals on the next frame. No special
  handling.
- **Reattach / new viewport mid-countdown** (the `Ctrl-^`-less new user, a
  second laptop, or a mux reconnect): the daemon delivers the active modal on
  Init to any modal-capable viewport (same as `PUSH_CMD`'s once-per-attach
  offer). It renders with the *current* remaining time. A viewport that
  reattaches after the deadline sees nothing (the outcome was already
  resolved).
- **Reconnect and `SessionResume`**: **R** RFC 0015 `SessionResume` carries
  durable stream offsets that must stay continuous when the transport is
  rebuilt ("Durable stream ⇒ a `SessionResume` field", CLAUDE.md). The modal is
  **not** a stream, it is current-state idempotently re-asserted, so it should
  **not** need a resume field. The result path (`APP_MODAL_RESULT`) is tokened
  and idempotent (§3.1), so a lost or duplicated choice is harmless. **I**:
  confirm in RFC 0015 that no new field is required.
- **Viewer already inside a pushed session (FDR 0016 stack)**: a modal belongs
  to a *session*, but the viewer may be looking at a *different* session
  (pushed shell, picker). Decision for the operator (Q3): show the modal on the
  viewport regardless of which session it is in (needs a viewport-level channel,
  bigger), or only to viewports currently in the target session (smallest;
  matches the daemon-fanout design above, and a user in a pushed shell simply
  misses the countdown; caller learns `capable:0`).
- **Palette already open**: a modal replaces the open view (`ui.show` replaces)
  and on resolution the client does not restore the prior palette (simplest);
  alternative is a queue. Minor; pick the former.
- **Daemon restart or live cutover (FDR 0017, proposed)**: **I**: the modal
  dies with the daemon; the requester sees its connection drop and must
  re-issue. Acceptable for a restart that is itself about to restart things.

## 5. What would block or complicate this

- **R** No existing server→client UI channel; a new cap entry and a new client
  capability offer are required, plus a new daemon IPC tag. Small, but it
  touches RFC 0001's registry (edit in place), RFC 0005 (§7 method + `app.modal`
  semantics), a new RFC for the cap pair (model on RFC 0016), and an FDR. Per
  `docs/README.md`, status moves in the commit that moves code.
- **R** Mux channel limits do not block: RFC 0011 §3.4 bounds concurrent
  channels per kind (agent: 8/connection) and total buffered bytes, but the
  modal is a few hundred bytes riding the *existing* session channel's message
  as capability entries; it opens no channel and adds no buffering of note.
  (**I**: the per-message cap-entry budget, `len: u8` = 247 bytes per entry
  (RFC 0011 §5), is the real ceiling. A modal bigger than that must be chunked
  or shortened; a countdown + 2 buttons fits. Verify the entry length limit
  for the server direction.)
- **R** posh#225 / flood delivery (FDR 0021, stages 1-4 shipped,
  `MAX_CLIENT_BACKLOG` 16 MiB): the risk for a modal is **delivery under load**,
  not loss of the viewport. A paced viewport is sent at most one screen at a
  time and that screen is the newest; cap entries ride frames, so the modal
  arrives on the next send opportunity (≤ `PACED_ACK_WAIT_MS` 250 ms
  typical; worse at high RTT, with the documented ack-lost stall). Consequences:
  (a) a countdown shown during a flood may lag, so remaining-ms must be sent
  with each frame (done) rather than relying on viewport-local ticking from a
  stale base; (b) because the daemon, not the viewport, owns the clock,
  a flooding session cannot make a restart fire early or late;
  (c) a flood-dropped viewport (backstop only now, "not told why", posh#226)
  simply misses the modal; same as "viewport not capable".
  The unpaced paths (local `posh attach`, relay) keep per-read delivery and have
  the old backlog risk in a flood; **R** FDR 0021: "A viewport that is dropped
  is not told why".
- **R** The palette is a frames-on, client-side, renderer-subprocess feature.
  A client without `posh-palette` installed has only the quit-only fallback
  prefix; the modal does not display there (`capable` excludes it).
- **R** Keyboard capture while open (§3.3): needs the UX decision.
- **R** FDR 0022 confinement applies to the new verb (§2).
- **R** Confirmed-in-docs gotcha: a roaming `posh server` (ephemeral) has no
  daemon, so no modal. Only durable sessions (the default, FDR 0011) work.
- **Unverified (**I**) points I could not settle read-only**: (1) whether
  `description` and rapid `ui.show` replacement are flicker-free; (2) exact relay
  rule for a new cap id; (3) the server→client entry size ceiling; (4) that
  `RFC 0015` needs no change. Each is a one-recipe check (a `debug-` recipe
  driving `posh-palette` in tmux, like `debug-palette-e2e`) before committing to
  the design.

## 6. Smallest implementation slice (suggested order)

1. Doc: RFC (new) for `APP_MODAL`/`APP_MODAL_RESULT` + `Tag::Modal`; RFC 0001
   registry rows; RFC 0005 §7 gains `app.modal.choose`; FDR for the user-facing
   behavior. (Design records first; status `proposed`.)
2. Daemon: `Tag::Modal` handler, deadline clock, fan-out, first-wins arbiter,
   `posh status` `modal=` field (also serves the probe).
3. Client (both): advertise the offer, render via the existing `palette`/
   `description` view, send the tokened result; countdown re-`ui.show` ticks.
4. CLI: `posh modal` (blocking; prints the outcome JSON; exit code per outcome).
5. Only if flicker (Q1 check) is real: additive `countdown` renderer view.

No change to the transport, mux channel kinds, `SessionResume`, or posh-term.

## Questions for the operator

1. **Reuse vs new view.** Is "palette view with a description and buttons,
   re-sent each second" acceptable, or do you want a purpose-built `countdown`
   view in `posh-palette` (additive, small Go change)? (We will know after a
   10-minute flicker check.)
2. **Key stealing.** Should the countdown steal the keyboard at once (modal),
   or show as a non-modal banner first and become a modal only on `Ctrl-^`?
   A modal can swallow whatever the user is typing mid-sentence.
3. **Which viewports see it.** Only viewports currently in the target session
   (smallest), or every viewport of the user's stack whatever session it is in?
4. **Concurrent modals.** One at a time (reject `busy`), or queue?
5. **Who may call.** Is it acceptable that FDR 0022 confinement blocks an
   agent-scoped caller, so the restarter must run outside the agent's scope
   (spinclass/clown lanes)?
6. **Detached default.** Confirm: no viewers means proceed immediately
   (`min_viewers:0`), the caller never waits for nobody.
7. **Timeout default.** Restart countdown: Esc/dismiss = cancel, timeout =
   proceed. Is that the intended asymmetry?
8. **General vs restart-only.** Is a general app-modal API (kinds: countdown,
   choice, notice) wanted now, or build the restart countdown only and
   generalize later? The request shape above generalizes at no extra cost; the
   cost is the RFC surface.

## Operator answers (2026-10-24)

- Key capture: **banner first**, modal only on `Ctrl-^` (Q2).
- Audience: **target session only** (Q3).
- Confinement: **acceptable** that the restarter runs outside the agent scope (Q5).
- Scope: **restart countdown first, general app-modal API eventually**
  (Q8). Defaults (dismiss=cancel, timeout=proceed, no viewers=proceed) were
  bundled into that question and not separately confirmed; treat as proposed.
- Process: **prototype first**; the operator gives UX feedback before anything
  is locked down or the real build starts. So: no RFC/FDR status moves and no
  registry edits until then; the §6 slice order applies to the prototype, with
  the design records deferred.
- Q1 decided: **a new `countdown` view** in `posh-palette`, not the `palette`
  view re-sent each tick. This supersedes §3.3's "cheapest path" and makes the
  flicker check moot for the view choice (it was only needed to justify a new
  view). The view is additive per RFC 0005 §9: an old renderer answers
  `-32602`, and the client then degrades to the banner alone (no modal), which
  is also the "banner first" state, so the fallback costs nothing new.
- **Refactor flag (follow-up, not part of the prototype's critical path):**
  `posh-palette/main.go` is a single `model` whose `Update` switches on view
  (`palette`, `dialog`, `picker`, `notice`) and each case re-initialises shared
  fields by hand (title, description, input, selection). A fifth view would add
  another case in the same shape. The palette should be refactored to expose
  building blocks the countdown (and later app-modal kinds) can compose:
  panel/frame layout and anchoring, title + wrapped description, a button row
  with focus and key bindings, a dim hint line, a per-view `Update`/`View`
  interface instead of one switch, and an in-place update message that keeps
  focus. Intent: build `countdown` on those blocks, extracting them as needed,
  rather than copying the `notice`/`dialog` code. Whether to refactor first or
  extract while building the prototype is open (Q9).
- Still open: Q4 (concurrent modals), Q9 (refactor-first vs extract-as-you-go),
  and the unverified items in §5 (items 1's flicker part no longer blocks).

## Prototype status (2026-10-25)

Branch `proto-app-modal-countdown`, unmerged. Demo and checks:

- `just debug-modal-demo <scenario>`: scripted tmux walkthrough (captures are
  text only and timing-dependent), plus `manual` for trying it by hand.
- `just debug-modal-record <scenario>`: the same walks recorded with
  `poshterity record` and asserted on the replay; PASS/FAIL per check. Observed
  passing for cancel, esc, hide, timeout.

### poshterity shortcomings, sequenced as work (posh#255)

Found while driving a scripted viewer; in the order to fix them. 1. markers
cannot be recorded; 2. write indices are unstable run to run, so checks scan by
pattern; 3. `step` replays from the start every call (hundreds of processes per
scan); 4. scripted input is `sleep`-based, no wait-for-screen; 5. no size flag
(24x80 only because stdout is not a tty); 6. the final screen is the one after
the alt-screen exit; 7. no "screen contains" assertion, and wall-clock text
defeats goldens; 8. styles (the reverse-video banner) unasserted, untried;
9. stdin EOF does not end a recording.

Status (2026-10-25): all nine gaps are addressed in poshterity, each with tests:
`record --mark-fifo` (1), `seek --contains/--lacks` (2, 3, 7: one pass,
content-based, so write indices no longer matter), `record --size` (5),
`record --end-on-eof` (9), `record --script` (4: an expect-style driver whose
`wait-for`/`wait-lacks`/`send`/`sleep`/`mark` steps watch the program's live
screen), `replay --before-alt-exit` (6) and `seek --styled TEXT=attrs` (8).
`debug-modal-record` uses `--script`, `--size`, `seek --contains/--lacks/--styled`
and has no sleeps: it also waits for the viewer to probe as capable
(`posh modal --probe`) before raising the modal, instead of guessing a delay.

### Product finding from the recording

The banner outliving the closed view is timing-dependent: 1 write in two runs,
0 in another, so it is a race between the client's choose and the daemon's
clear arriving in the same read, not a fixed extra frame.

After a choice the view closes one write before the daemon's `clear` removes the
banner (one extra frame with the banner and no view). Observed for cancel and
esc. Not decided whether it needs fixing: an optimistic client-side clear would
be wrong if another viewer's answer won.

## Sources

Man pages: posh(1) SESSION COMMANDS/REMOTE COMMANDS (output was truncated by
the tool; the `doc/posh.1.scd` source was grepped and partly read instead),
posh(7) PERSISTENCE/TERMINAL TAKEOVER, posh-client(1) KEYS (truncated).
Docs read in full: FDR 0009, 0020, 0021; RFC 0005, 0016. Docs only grepped:
FDR 0022 (headings and key lines), RFC 0001 (registry table), RFC 0011
(keyword grep). NOT read: FDR 0011, 0016, 0019, RFC 0015, ADR 0007; claims that
lean on them are labelled **I**. Code: `session/ipc.rs`
(Tag enum), `session/client.rs` (palette wiring), `remote/palette.rs`
(`show_notice`, `composite_palette`), `posh-term/src/osc.rs`, `session/daemon.rs`
(`MAX_CLIENT_BACKLOG`, confine accept check). Nothing was built or run.
