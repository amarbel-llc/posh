# Wheel scroll behavior in local posh sessions

A note on a behavior that trips up agents and users, and the diagnostic that
distinguishes the layers. Under the session frame transport (RFC 0008),
scrolling the wheel at a **bare prompt** in a local posh session drives
**posh's own scroll-view** (scrollback) — like tmux, and unlike a bare
terminal. The inverse is the thing to recognize: if the wheel instead emits
**arrow keys** (`↑`/`↓`) to the shell, that session is **not framed** (a
pre-frames daemon, or a baseline client) and posh is a passthrough.

> **Historical note (2026-08-25).** Frames used to be gated daemon-side by
> `POSH_SESSION_FRAMES` (default on, `=0` to opt out). That gate was retired
> for local/remote parity (posh#171; the roaming server never had one) and the
> variable is now ignored. Everything below that used to say "gate off" now
> means "an older daemon that predates frames".

## The two behaviors

Inside a posh local session (e.g. one clown self-wraps you in — clown's default
multiplexer is posh; see below), scrolling the mouse wheel at a **bare prompt**:

- **framed (a current daemon + a current client):** opens posh's scroll-view
  and scrolls its scrollback ring — the tmux-like behavior most users expect.
- **not framed (a pre-frames daemon, or a baseline client):** prints `↑`/`↓`
  to the shell (and on the **alt screen** the same, driving whatever the TUI
  does with cursor keys) — posh is a passthrough and the *outer terminal*
  translates the wheel. Before frames existed this was the only behavior,
  which is why it can read as a regression to someone who remembers it.

## Why it happens (verified)

posh has a full wheel-intercept + local scroll-view feature
(`crates/posh/src/remote/scrollview.rs`, shared by the local and roaming
clients; FDR 0005), driven on the local session path by frame negotiation
(`crates/posh/src/session/daemon.rs`, `is_frame_capable` /
`maybe_enable_frames`):

- Frame-capable client (its `Tag::Init` carries a capability table with
  `CAP_PROTOCOL_VERSION`) ⇒ the daemon builds a `FrameProducer` for it and
  sends `Tag::Frame` ⇒ the client builds a `FrameRenderer`
  (`crates/posh/src/session/client.rs`) whose wheel-intercept / scroll-view /
  `MouseFilter` path is live, so the wheel scrolls posh's scrollback in place.
- No frames (an old daemon that ignores the table, or a baseline client) ⇒
  raw `Tag::Output`, never `Tag::Frame` ⇒ the client never builds a
  `FrameRenderer` ⇒ the whole wheel-intercept path is **inert**, and stdin
  forwards **verbatim** to the daemon (only the detach matcher sits between
  raw stdin and `Tag::Input`, and it passes wheel bytes through untouched —
  the `gate_off_forwards_wheel_bytes_to_daemon_unchanged` test). The wheel
  bytes then reach the shell's PTY, and the **outer terminal's
  alternate-scroll mode** (`DECSET ?1007`) converts wheel-up/down into `↑`/`↓`
  when no mouse tracking is active — the passthrough that predated frames.

Note also: posh's local client, even when framed, only ever *scrolls* on the
wheel — it never translates to arrows. The wheel→arrow grab
(`POSH_GRAB_MOUSE`, ADR-0002) is a **remote-client-only** path and is default-off
regardless. clown/eng set no `POSH_GRAB_MOUSE` anywhere; the only `POSH_*` var
in the eng tree is `POSH_DIR` (a socket-path fix, `eng/home/spinclass.nix`).

## How clown launches posh

clown's `default-clownfile` sets `multiplexer = "posh"` and launches
`posh attach {id} {entry}` — a **local** session (the `session/*` path), not a
remote roaming session. With a current daemon and client it is framed, so the
wheel drives posh's scroll-view. There is no launch-env switch back to the
passthrough any more; the passthrough only occurs with a pre-frames daemon.

## Diagnosing it (distinguishing the layers)

If the wheel emits arrows when you expected scrolling, run `cat -v` at a bare
prompt inside the session and scroll:

- Bytes arrive as `^[[A` / `^[[B` (CSI cursor keys) ⇒ the **outer terminal**
  translated the wheel before posh saw it; posh forwarded verbatim ⇒ the
  session is **not framed** (a pre-frames daemon — check `posh status`'s
  `daemon=` build — or a baseline client). Upgrade/restart the daemon to get
  posh's scroll-view back.
- No bytes reach the shell and the view scrolls instead ⇒ the session is
  framed and posh's scroll-view is handling the wheel — working as intended.
- Bytes arrive as SGR mouse form `^[[<64;…M` / `^[[<65;…M` ⇒ the terminal is
  emitting raw wheel events and something *downstream* is translating them. That
  is a different investigation — the passthrough story above does not apply.

## The terminal-native wheel

There is no longer a supported way to make a current daemon serve the
passthrough: the `FrameRenderer` path is the session transport, and it also
carries the command palette (FDR 0011 Phase 2.4: `Ctrl-^`, with Suspend /
Detach / Shell out). Resync and prediction are absent on the reliable local
socket by design (reliable-as-degenerate, RFC 0008 §2), not a gap.
