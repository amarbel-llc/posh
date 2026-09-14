---
status: experimental
date: 2026-09-13
---

# Viewport recording (command-palette Start/Stop → `.castx`)

## Problem Statement

Some client-side rendering bugs are live, round-trip, sub-frame artifacts that a
deterministic snapshot test cannot see. posh#197 is the motivating case: under
the `always` echo model (the default) predicted characters briefly appear BEYOND
the cursor and then walk back. The model → compose → diff → tty-application
layers were proven correct by `crates/posh/src/remote/client.rs`'s
`always_echo_*` tests, so the flash lives below them — a terminal painting
content before the trailing cursor-move within one `render_to` write, or a live
server redraw sequence — observable only in a real, real-RTT session.

`poshterity record --via posh` (and the `just debug-record-echo` recipe) can
capture the client viewport from OUTSIDE, but that needs an external harness and
a chosen host. The user wants it NATIVE: the client records its OWN viewport, on
demand, from inside any session it is already in.

## Interface

- **Trigger:** the command palette's *Start recording* / *Stop recording*
  command (`Ctrl-^`, then select it — FDR 0009). A state-reflecting toggle:
  "Start recording" when off, "Stop recording" while a recording is live.
- **Output:** a poshterity `.castx` (RFC 0003 — asciinema `.cast` v2 superset)
  under the diagnostic-sink directory (`$POSH_DIR` / the socket dir, the same
  scheme as the debug log and the SIGUSR2 dump), named
  `posh-record-<pid>-<unixtime>.castx`. On start and stop the path is presented
  in a copyable, dismissable dialog — the same panel as *About / transport
  info* (`show_debug_info` → `show_dialog`), whose *Copy* puts the path on the
  clipboard via OSC 52 — rather than a fleeting banner.
- **Captured:** the client VIEWPORT — the composed tty output (`o` events, the
  predictions and their walk-back included), the user's raw keystrokes (`i`
  events), and terminal resizes (`r` events). The header carries the emulator
  revision (`posh_term::emu_rev()`) for golden auditing.
- **Replay:** `poshterity replay <file>` (final screen) and
  `poshterity step <file> --by frame --dump vt` (frame-by-frame), so a sub-frame
  overshoot is inspectable off the recording.
- **Wire:** none — recording is entirely client-local (RFC 0005 §7 `record.set`,
  `{"enabled": <bool>}`; nothing is sent to the server).

## Design

- The recorder reuses `poshterity::castx::Recorder` (the `posh` crate already
  depends on the `poshterity` crate for `posh rec`), so there is no new
  dependency and no format duplication.
- `ClientState.record: Option<ViewportRecorder>` (`remote/client.rs`) holds the
  live recorder; `None` unless recording. `ViewportRecorder` wraps the
  `Recorder<BufWriter<File>>` plus an `Instant` start (timestamps are seconds
  since start) and the path. Its `Drop` calls `Recorder::finish()`, so every
  client-exit path finalizes the file — no per-`break` cleanup.
- Tee points, each guarded by `if let Some(rec) = st.record.as_mut()`:
  - **output** — `render_to`, on the full-write branch, tees the exact bytes the
    tty received (a dropped paint forces a resync repaint next tick, recorded
    then, so the recording stays faithful);
  - **input** — `process_user_input`, the raw keystrokes as read;
  - **resize** — the SIGWINCH handler, on a real size change.
- `record.set` dispatch (`dispatch_palette_action`) mirrors `set_logging`:
  enable opens `diag::record_path()` and sets `st.record`; disable takes and
  drops it (Drop finalizes), each with a path notice.
- No `posh-palette` (Go) change: the palette command list is JSON data the
  client builds (`palette_commands`) and sends to the renderer.

## Privacy

Recording captures keystrokes and the screen VERBATIM — including any secret
typed while recording, and (under `always`, which bypasses the RFC 0007 §5.1
gate) transient password-prompt predictions. Recording is never automatic; it is
on only by explicit palette action, the palette shows "Stop recording" while
live and prints the path on start/stop, and `doc/posh-client.1.scd` documents
the exposure.

## Testing

- Unit (`crates/posh/src/remote/client.rs`): the `record.set` dispatch toggle
  (`st.record` Some↔None), the palette entry's Start/Stop labels, and a tee
  round-trip — a rendered server frame lands as `o` events and a keystroke as an
  `i` event in a `.castx` read back with `poshterity::castx::Reader`, header
  carrying the poshterity block.
- Native manual: attach a REAL session (real RTT — a loopback's ~0 RTT confirms
  predictions instantly and won't show #197), *Start recording*, type to
  reproduce, *Stop recording*, then `poshterity step … --dump vt` to find the
  frame with a glyph beyond the cursor.
