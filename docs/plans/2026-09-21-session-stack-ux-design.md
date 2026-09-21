# Session stack UX: session kind, viewport registry, predecessor indicator, leave prompt

**Date:** 2026-09-21
**Status:** design approved, awaiting implementation plan
**Builds on:** FDR 0016 (cross-host session switcher, stacked switching),
FDR 0015 (`ph` front door, the `:+` create sigil), RFC 0005 (palette
control protocol), RFC 0013 §5 (`CAP_SESSION_ACTIVITY`), RFC 0014 (client
introspection; §5 UPSTREAM slot).
**Related:** spinclass#306 (start a top-level posh session on
`sc start` / `sc resume`).

## Problem

FDR 0016's session stack is invisible until the palette is opened, and
even then only the top is offered (*Back to X*). A viewport that entered
posh through an anonymous session (`ph :+`, the picker's create-new row)
and then switched to an existing session leaves that anonymous session
running forever on detach, indistinguishable from one the user meant to
keep. Nothing in the daemon records whether a session was anonymous, so
any rule keyed on the name (`s-N`, a UUID) would make naming
load-bearing.

## Decisions

Recorded from the design conversation:

1. **Kind is daemon-owned state, sequenced first.** A session's kind
   (`anonymous` / `named` / `system`) is stated at create time by the
   creator and stored by the daemon for the session's life. Naming is
   never consulted.
2. **Anonymous means created by `:+` or the picker's create-new row.**
   The front door tags the kind at push time from the daemon's answer,
   with its own `:+` dispatch as the fallback when the daemon predates
   the field.
3. **The viewport is the daemon for system sessions.** The stack, the
   current session, and the live system overlays (palette, picker, leave
   prompt) live in the viewport process, exposed on a per-viewport
   status socket on the viewport host. No session daemon holds another
   viewport's history, so two viewports over one session each pop to
   their own top when it ends. System overlays are never on the stack.
4. **Predecessor indicator: palette heading only.** No banner row, no
   title change.
5. **Leave prompt: ask, default keep**, candidates listed in a
   description block above the options, default behavior a config lever.
6. **Scope: current session plus stack entries.** `ph :+` then detach
   asks the same question with one entry.
7. **Palette presentation is separate from schema.** The command palette
   will be redesigned; the stack's representation in it must be
   changeable without touching the stack model or the wire.

Cross-host stack entries are already permitted (the in-session picker
lists this machine plus every live mux endpoint) and cleanup already
routes per entry (`picker::kill_target`: a local socket, or
`ssh <dest> posh kill --unless-attached`), so no daemon needs to be
reachable for the stack to be cleaned up after the current session ends.

## Section 1: daemon session kind

**Model.** `SessionKind { Anonymous, Named, System }` in `posh_proto`
(the picker, the daemon, and both clients need it). Sessions never
change kind. `Unknown` is the reading of a daemon that predates the
field.

**Wire.** `session::ipc::SessionInfo` gains `kind`, appended after the
RFC 0013 §5 activity label as one byte (`0` unknown, `1` anonymous,
`2` named, `3` system), decoded as `Unknown` when absent, the same
skew-tolerant append the activity label used. `posh list --json` emits
`"kind":"anonymous"` and omits the field when unknown, so the
field-tolerant remote parser (`remote_entries`) and the zmx-shape test
keep working. `PickerEntry` carries it; the picker's status cell and the
`posh ls` table show a KIND column. The relay and the M2 bridge forward
it to a roaming client inside the `CAP_SESSION_ACTIVITY` frame (id 15),
extended additively; an old server leaves it unknown.

**Create plumbing.** `posh attach --create` and `posh start` accept
`--kind <anonymous|named>`. `cmd_start` derives it from
`classify_start_target` (`LocalAuto` / `RemoteAuto` = anonymous, the
named forms = named) and threads it through `cmd_start_local`,
`start_remote_auto`, the `remote_session_argv` bootstrap tail, and
`daemon::ensure_session`. An absent flag means `Named`, so every
existing caller (clown's `posh start <uuid>`, spinclass) is unchanged.
Nothing creates `System` in this section; the value is reserved for a
daemon-owned session that hosts a tool instead of a shell, should one
ever be needed.

**Rollback.** Additive on every surface: an older client ignores the
trailing byte, an older daemon yields unknown, the flag is optional.

## Section 2: viewport registry

**Model.** `picker.rs`'s `STACK: Vec<String>` becomes
`Vec<StackEntry { target, kind }>`; `current()` carries a kind too. The
attach entry points, which already call `set_current` once the daemon
answered, add the kind they received (`Tag::Info` locally; the extended
activity frame over a relay or bridge). When the daemon answers
`Unknown` and the front door dispatched a `:+` / create-new target, the
front door records `Anonymous` itself: the "created by this viewport"
fallback, and the only case where the viewport's knowledge overrides the
daemon's.

**Exposure.** On its first attach the front door binds
`<base>/viewports/<pid>.status.sock` plus `<pid>.status.pid`, the shape
of an Architecture-A server's `remote/<pid>.status.sock`, answering the
RFC 0014 §4 one-shot with:

```
viewport pid=<pid> current=box:dev kind=named
stack depth=1 target=flac:s-1 kind=anonymous
stack depth=2 target=box:s-3 kind=anonymous
overlay kind=palette over=box:dev
```

`posh status --viewport <pid>` reads it. `posh list` grows nothing yet.
Reaped like the `remote/` sockets. Specified as a new RFC 0014 §6 (the
§5 UPSTREAM entry is the nested-session case and stays as it is), amended
in the same change.

**System overlays.** The palette, the picker, and the leave prompt
register as overlays in the viewport while open and unregister on close.
They never push to the stack. A registered viewport-local entity with
daemon-side data (the session's kind and activity) and no PTY is the
whole "system session" concept for now.

**Rollback.** The socket is diagnostic only; `POSH_VIEWPORT_STATUS=0`
skips the bind. Stack semantics are unchanged.

## Section 3: predecessor indicator

**Schema, not presentation.** The stack's view model is a plain value
the front door computes and both clients pass to the palette layer:

```rust
pub struct StackView {
    pub top: Option<StackEntry>,   // the session *Back* returns to
    pub depth: usize,
    pub current: Option<StackEntry>,
}
```

`picker::stack_view()` is the only producer. Everything that renders it
(the Commands palette title, the picker heading, the *Back to X* row and
its position, the RFC 0005 `description` block of the leave prompt) is a
pure function from `StackView` (plus the existing palette inputs) to
RFC 0005 JSON, living in one module (`remote/palette_view.rs`, shared by
both clients through `crate::`). The upcoming palette redesign changes
those functions and nothing else: no stack mutation, no wire, no client
loop code refers to a row label or a title string.

**This iteration's presentation.**

- Commands palette title: `Commands · back: flac:s-1` at depth 1,
  `Commands · back: flac:s-1 +2` deeper, targets abbreviated by
  `short_session` / `short_host` to stay under the renderer's ~42
  content columns. The rtt / echo suffix keeps its place.
- *Back to X* is the first row of the Commands palette while the stack
  has a top, so Ctrl-^ Enter is "go back".
- The picker (`session.list`) heading keeps its depth count.
- Nothing else changes; the auto-pop notice on the next first frame is
  as before.

## Section 4: the leave prompt

**Trigger.** In `run()`, after an attach ends with no switch to
dispatch: on `Quit` (detach, quit, `Ctrl-\`), and on `Ended` / `Lost`
once `auto_pop` found nothing to pop. Candidates
(`picker::leave_candidates`) are every `Anonymous` stack entry plus the
current session when it is `Anonymous` and still alive; an `Ended`
current is not a candidate. No candidates: silent exit, as today.

**Policy lever.** `POSH_LEAVE_ANONYMOUS=ask|keep|kill`, default `ask`,
read once by the front door. `keep` skips the prompt; `kill` runs the
kills unasked and prints the notices. The env var is the config surface
until posh has a config file; the config key is the same name. A
signal-driven end, or a viewport without a tty, degrades `ask` to `keep`
plus one stderr line naming the sessions left running.

**Prompt.** `palette::choose_standalone` on a blank frame (the
standalone `ph` picker path), rendered by `palette_view` from the
`StackView`: the RFC 0005 `description` block lists the candidates one
per line (`flac:s-1  anonymous  detached`,
`box:s-3  anonymous  attached (1)`), and the rows below are:

- *Keep them running* (first, so Enter keeps)
- *Kill them (kept if other viewports are attached)*
- *Kill them even with other viewports attached*

**Execution.** A kill runs `kill_target` per candidate in stack order,
remote ones over ssh with `--unless-attached` unless forced, one notice
line each; a failed host fails only its entry. Notices print on stderr
after the tty is restored. The current session, when a candidate, is
killed last and only after its attach has fully ended.

**Two viewports.** Nothing consults another viewport's stack. The
`--unless-attached` refusal protects a session another viewport is still
in, and its notice says so.

## Section 5: testing

- `ipc.rs`: `SessionInfo` round-trip with and without the kind byte;
  a pre-kind record decodes to `Unknown`.
- `session/mod.rs`: `json_list` emits `kind` only when known;
  `remote_entries` reads it and tolerates absence.
- `main.rs`: `classify_start_target` to kind; the bootstrap argv carries
  `--kind`; an absent flag is `Named`.
- `picker.rs`: entries carry kinds; `leave_candidates` over the six
  `(end, current kind, stack)` combinations; `stack_view` values.
- `palette_view.rs`: the JSON for each `StackView` shape, pinned so a
  redesign changes these tests and nothing else.
- `daemon.rs`: the in-process daemon tests assert a created session
  reports its kind over `Tag::Info`; the relay / bridge tests assert the
  extended activity frame.
- Viewport socket: bind, one-shot answer, reap, and the
  `POSH_VIEWPORT_STATUS=0` skip, in the style of the
  `remote/<pid>.status.sock` tests.
- Manual: a `debug-verify-leave-prompt` recipe driving `ph :+`, a
  keep-switch to a named session, and a detach in a tmux pane, capturing
  the prompt and the resulting `posh list`.

## Section 6: tuning levers and rollback

| lever | now | change signal |
|---|---|---|
| `POSH_LEAVE_ANONYMOUS` | `ask` | users always answer the same way, or object to the prompt on every detach |
| unattended fallback | `keep` | anonymous sessions accumulate on hosts after dropped ssh links |
| heading depth suffix | `+N` | the suffix crowds the rtt out on narrow terminals |

Rollback is per section: the kind field is additive, the registry socket
is env-gated, the heading change is a `palette_view` edit, and the prompt
is disabled by `keep`.

## Sequencing

1. Section 1 (kind column, create plumbing, list / picker surfaces).
2. Section 2 (typed stack, viewport status socket, overlay registration,
   RFC 0014 §5 amendment).
3. Section 3 (`StackView` + `palette_view`, heading and row order).
4. Section 4 (leave prompt, lever, kills), then the FDR 0016 amendment
   and the `posh(1)` / `posh(7)` man page updates.

## Out of scope

- A daemon-owned `System` session running a tool (reserved value only).
- Forward navigation, deduplication of repeated targets, or persisting
  the stack across viewport exits (FDR 0016 trade-off row stands).
- `posh list` showing viewports.
- The palette redesign itself.
