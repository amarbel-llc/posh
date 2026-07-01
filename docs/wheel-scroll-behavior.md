# Wheel scroll behavior in local posh sessions (the alternate-scroll passthrough)

A debugging note, not a design record. It captures a behavior that surprises
agents and users: **in a default posh local session, scrolling the wheel emits
arrow keys, not scrollback.** This is not a posh bug — posh is a passthrough in
that configuration — but the symptom looks like one, so it is written down here
with the diagnostic that distinguishes the layers.

## The symptom

Inside a posh local session (e.g. one clown self-wraps you in — clown's default
multiplexer is posh; see below), scrolling the mouse wheel at a **bare prompt**
prints `↑`/`↓` to the shell (and on the **alt screen** the same, driving whatever
the TUI does with cursor keys). It does *not* open a scrollback view. Under a
previous multiplexer (e.g. tmux) the wheel gave real scrollback, so the change
reads as a regression introduced by adopting posh.

## Why it happens (verified)

posh has a full wheel-intercept + local scroll-view feature
(`crates/posh/src/remote/scrollview.rs`, shared by the local and roaming
clients; FDR 0005). But on the **local session path** it is behind the
`POSH_SESSION_FRAMES` daemon gate, which is **default-OFF**
(`crates/posh/src/session/daemon.rs`, `parse_frames_gate` /
`session_frames_enabled`; the `frames_gate_defaults_off_and_parses_truthy`
test):

- Gate off ⇒ the daemon never builds a `FrameProducer` ⇒ it sends raw
  `Tag::Output`, never `Tag::Frame`.
- The client therefore never builds a `FrameRenderer`
  (`crates/posh/src/session/client.rs`) ⇒ the entire wheel-intercept /
  scroll-view / `MouseFilter` path is **inert**.
- stdin forwards **verbatim** to the daemon (only the detach matcher sits
  between raw stdin and `Tag::Input`, and it passes wheel bytes through
  untouched — the `gate_off_forwards_wheel_bytes_to_daemon_unchanged` test).

So the wheel bytes pass straight through posh to the shell's PTY. The arrow keys
are produced by the **outer terminal's alternate-scroll mode** (`DECSET ?1007`):
the terminal itself converts wheel-up/down into `↑`/`↓` when no mouse tracking
is active. This fits both observed cases — alt-screen (where alternate-scroll is
designed to fire) and bare prompt (where the terminal leaves it on). The
previous multiplexer was *catching* the wheel for its own scrollback; posh at
gate-off does not, so the terminal's native arrows now reach the shell.

Note also: posh's local client, even with frames ON, only ever *scrolls* on the
wheel — it never translates to arrows. The wheel→arrow grab
(`POSH_GRAB_MOUSE`, ADR-0002) is a **remote-client-only** path and is default-off
regardless. clown/eng set no `POSH_SESSION_FRAMES` and no `POSH_GRAB_MOUSE`
anywhere; the only `POSH_*` var in the eng tree is `POSH_DIR` (a socket-path
fix, `eng/home/spinclass.nix`).

## How clown launches posh

clown's `default-clownfile` sets `multiplexer = "posh"` and launches
`posh attach {id} {entry}` — a **local** session (the `session/*` path), not a
remote roaming session. It sets none of the `POSH_*` gates, so the session runs
with `POSH_SESSION_FRAMES` unset ⇒ off.

## Diagnosing it (distinguishing the layers)

At a bare prompt inside the session, run `cat -v` and scroll:

- Bytes arrive as `^[[A` / `^[[B` (CSI cursor keys) ⇒ the **outer terminal**
  already translated the wheel before posh saw it; posh forwarded verbatim.
  This is the alternate-scroll passthrough described here — expected in the
  default (gate-off) config.
- Bytes arrive as SGR mouse form `^[[<64;…M` / `^[[<65;…M` ⇒ the terminal is
  emitting raw wheel events and something *downstream* is translating them.
  That is a different investigation — the passthrough story above does not
  apply.

## Making the wheel scroll instead

Enable posh's own scroll-view by setting `POSH_SESSION_FRAMES=on` in the
daemon's launch env (the client then builds the `FrameRenderer` and the wheel
drives the local scrollback view at a bare prompt). This is the built, tested
FDR 0005 path — but it is a bigger change than a config toggle: the local
FrameRenderer is a minimal Phase-1/2 consumer (no resync/prediction/palette),
so flipping it on fleet-wide should be validated for maturity first, not assumed
production-ready from the gate alone.
