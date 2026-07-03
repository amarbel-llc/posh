# Wheel scroll behavior in local posh sessions

A note on a behavior that trips up agents and users, and the diagnostic that
distinguishes the layers. Since the session frame transport became **default-ON**
(RFC 0008, the fleet gate-flip), scrolling the wheel at a **bare prompt** in a
local posh session drives **posh's own scroll-view** (scrollback) — like tmux,
and unlike a bare terminal. The inverse is the thing to recognize: if the wheel
instead emits **arrow keys** (`↑`/`↓`) to the shell, posh's frames are **disabled**
for that session (`POSH_SESSION_FRAMES=0`, or an old / pre-frames daemon) and posh
has fallen back to a passthrough.

## The two behaviors, by gate state

Inside a posh local session (e.g. one clown self-wraps you in — clown's default
multiplexer is posh; see below), scrolling the mouse wheel at a **bare prompt**:

- **frames on (the default):** opens posh's scroll-view and scrolls its
  scrollback ring — the tmux-like behavior most users expect.
- **frames off (`POSH_SESSION_FRAMES=0`, or an old daemon):** prints `↑`/`↓` to
  the shell (and on the **alt screen** the same, driving whatever the TUI does
  with cursor keys) — posh is a passthrough and the *outer terminal* translates
  the wheel. Before frames were the default this was the only behavior, which is
  why it can read as a regression to someone who remembers it.

## Why it happens (verified)

posh has a full wheel-intercept + local scroll-view feature
(`crates/posh/src/remote/scrollview.rs`, shared by the local and roaming
clients; FDR 0005), driven on the local session path by the `POSH_SESSION_FRAMES`
daemon gate, now **default-ON** (an opt-out) (`crates/posh/src/session/daemon.rs`,
`parse_frames_gate` / `session_frames_enabled`; the
`frames_gate_defaults_on_and_parses_falsey` test):

- Gate on (the default) ⇒ the daemon builds a `FrameProducer` per frame-capable
  client and sends `Tag::Frame` ⇒ the client builds a `FrameRenderer`
  (`crates/posh/src/session/client.rs`) whose wheel-intercept / scroll-view /
  `MouseFilter` path is live, so the wheel scrolls posh's scrollback in place.
- Gate off (`POSH_SESSION_FRAMES=0`) ⇒ no `FrameProducer` ⇒ raw `Tag::Output`,
  never `Tag::Frame` ⇒ the client never builds a `FrameRenderer` ⇒ the whole
  wheel-intercept path is **inert**, and stdin forwards **verbatim** to the daemon
  (only the detach matcher sits between raw stdin and `Tag::Input`, and it passes
  wheel bytes through untouched — the
  `gate_off_forwards_wheel_bytes_to_daemon_unchanged` test). The wheel bytes then
  reach the shell's PTY, and the **outer terminal's alternate-scroll mode**
  (`DECSET ?1007`) converts wheel-up/down into `↑`/`↓` when no mouse tracking is
  active — the passthrough that predated the flip.

Note also: posh's local client, even with frames on, only ever *scrolls* on the
wheel — it never translates to arrows. The wheel→arrow grab
(`POSH_GRAB_MOUSE`, ADR-0002) is a **remote-client-only** path and is default-off
regardless. clown/eng set no `POSH_SESSION_FRAMES` and no `POSH_GRAB_MOUSE`
anywhere, so a clown-launched session runs on the default (frames on); the only
`POSH_*` var in the eng tree is `POSH_DIR` (a socket-path fix,
`eng/home/spinclass.nix`).

## How clown launches posh

clown's `default-clownfile` sets `multiplexer = "posh"` and launches
`posh attach {id} {entry}` — a **local** session (the `session/*` path), not a
remote roaming session. It sets none of the `POSH_*` gates, so the session runs
with `POSH_SESSION_FRAMES` unset ⇒ **on** (the default) ⇒ posh's scroll-view.
Set `POSH_SESSION_FRAMES=0` in the launch env to restore the old passthrough.

## Diagnosing it (distinguishing the layers)

If the wheel emits arrows when you expected scrolling, run `cat -v` at a bare
prompt inside the session and scroll:

- Bytes arrive as `^[[A` / `^[[B` (CSI cursor keys) ⇒ the **outer terminal**
  translated the wheel before posh saw it; posh forwarded verbatim ⇒ frames are
  **off** for this session (`POSH_SESSION_FRAMES=0`, or an old / pre-frames
  daemon). Turn frames on (the default — unset the var, or upgrade the daemon) to
  get posh's scroll-view back.
- No bytes reach the shell and the view scrolls instead ⇒ frames are **on** (the
  default) and posh's scroll-view is handling the wheel — working as intended.
- Bytes arrive as SGR mouse form `^[[<64;…M` / `^[[<65;…M` ⇒ the terminal is
  emitting raw wheel events and something *downstream* is translating them. That
  is a different investigation — the passthrough story above does not apply.

## Getting the old terminal-native wheel back

posh's scroll-view is now the default. To restore the pre-flip behavior — the
wheel passing through to the outer terminal's alternate-scroll arrows — set
`POSH_SESSION_FRAMES=0` in the daemon's launch env; the daemon then serves raw
`Tag::Output`, the client builds no `FrameRenderer`, and the wheel reaches the
shell exactly as on a bare terminal. The default `FrameRenderer` path also carries
the command palette (FDR 0011 Phase 2.4: `Ctrl-^`, with Suspend / Detach /
Shell out); resync and prediction are absent on the reliable local socket by
design (reliable-as-degenerate, RFC 0008 §2), not a gap.
