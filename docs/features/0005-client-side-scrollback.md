---
status: proposed
date: 2026-06-12
promotion-criteria: a working implementation exists where, attached to a
  real remote session over a lossy link, the wheel scrolls back through
  the session's primary-screen history with no perceptible per-tick
  latency, the steady-state per-frame wire cost while output scrolls is
  bounded (does not grow with scrollback depth), and a window resize
  mid-scroll resyncs cleanly without corrupting the view. At that point
  promote to `experimental` and open the lever table for measurement.
---

# Client-side scrollback (wheel-scroll the session's off-screen history)

## Problem Statement

An attached posh client can see only the session's visible grid: the
shell's scrollback — every line that scrolled off the top — is
unreachable, because the outer terminal is pinned to its own alternate
screen for the whole connection (FDR 0002) and the inner session's
history never reaches it. The user's terminal scrollbar shows the
takeover-screen's emptiness, not the session. This feature makes the
wheel scroll the *session's* primary-screen history locally, the way it
would if the session were running in the terminal directly. Covers
github #43 (with root-cause analysis in #3/#28).

## The state-model change this rests on

The roaming client today holds `server_term`, a `posh_term::Terminal`
that `apply_frame` **reconstructs from scratch every frame**
(`Terminal::with_scrollback(rows, cols, 0)` then `process(dump_vt)`,
`crates/posh/src/remote/client.rs`). Two properties of that design make
scrollback structurally impossible, independent of bandwidth:

1. **Zeroed ring.** The terminal is built with `max_scrollback = 0`, so
   even though the incoming `dump_vt` stream replays the server's full
   ring (`crates/posh-term/src/dump.rs` replays scrollback rows then grid
   rows as one continuous flow), the rows that scroll off the fresh
   client grid are discarded instead of saved.
2. **Rebuilt-per-frame.** `server_term` is a pure function of the latest
   frame — there is no accumulation across frames. Whatever the newest
   `dump_vt` doesn't carry, the client doesn't have.

The core change is to make the client's local model **persistent and
monotonically accumulating**: a long-lived `Terminal` with a real
scrollback ring, into which frames are *applied* (advancing it) rather
than *replacing* it. The client thereby becomes an explicit **offset,
partial, accumulating view** of the server's logical row-space — not a
faithful full replica. Three consequences follow directly from that
framing, and they are the design:

- **The client may hold less history than the server, and know it.**
  On a fresh attach the client's ring starts empty and grows as the
  session produces output; it is not required to back-fill the server's
  entire 10k-row ring. "Incomplete/truncated view" is a first-class
  state, not an error.
- **Local history is write-once and never dropped on eviction-from-server.**
  Once a row has entered the client's ring it stays (subject to the
  client's own ring cap), even after the server has aged it out of the
  window it actively syncs. A long-running attached session therefore
  accumulates a deep *local* history that can exceed what any single
  frame carries — the client is the durable reader, the server is the
  live source.
- **Reflow stays the server's job.** The client never recomputes wrap.
  It replays server-produced bytes through `posh_term`'s own autowrap
  (which regenerates soft-wrap flags by actually wrapping on apply), the
  same mechanism that already keeps the *visible* screen correct. Layout
  ambiguity is resolved by letting the server's already-reflowed
  serialization do the work; a width change is a renumber-and-resync
  event (see Limitations), not a client-side rewrap.

## Interface

From the user's seat, scrollback behaves as it would in a local terminal:

- **Wheel-up** at a bare session prompt enters the scrollback view and
  scrolls back through the session's primary-screen history; **wheel-down**
  scrolls forward; reaching the bottom returns to the live view.
- Scrolling is **local and immediate** — no server round-trip per tick —
  because the client renders from its own accumulated ring.
- The wheel is only intercepted under the same predicate as the
  `POSH_GRAB_MOUSE` transform (ADR 0002): when the inner application has
  set no mouse mode of its own. An app that wants the wheel (vim, less,
  an fzf pane) still gets it; scrollback view applies to the bare-prompt
  primary screen, which is the screen that *has* scrollback. The
  alternate screen has none by construction (`posh_term` gives the alt
  `Screen` `max_scrollback = 0`), so there is nothing to scroll there.
- While scrolled up, the live view is **frozen** (as in tmux copy-mode
  and less): new session output accumulates into the ring but does not
  yank the viewport back to the bottom until the user returns to live.

## Examples

    # Attached to a remote session; ran a long build, output scrolled past.
    posh host:build
    # ... build emits 500 lines, the prompt returns ...
    # Wheel up: the viewport scrolls back through the build output,
    # instantly, served from the client's own ring — no network stall
    # even on a satellite link.
    # Wheel back to the bottom (or press a key): live view resumes.

    # Detach and reattach later: the client ring starts empty and the
    # session's *new* output grows it. History from before the reattach
    # that the server has aged out of its sync window is not back-filled —
    # the client accumulates forward from attach. (Whether to offer an
    # explicit "pull older history" request is a deferred extension; see
    # Limitations.)

## Limitations

- **No back-fill of pre-attach history (initial scope).** A fresh attach
  grows the client ring forward from the moment of attach; the server's
  existing deep ring is not streamed up front. The session's own
  `posh history` command already serves the full server-side scrollback
  as text, so the gap is "interactive wheel-scroll of pre-attach lines,"
  not "no access to them." A bounded "request older window" round-trip is
  the natural extension once the forward path works.
- **Resize/reflow is a renumber-and-resync event.** A width change
  rewraps the server's ring (`posh_term` reflows scrollback on resize),
  which renumbers the logical row-space: absolute row indices are stable
  only *between* resizes. The client treats a resize as "discard the
  scroll view, return to live, let the ring re-accumulate at the new
  width." This matches tmux/less, which also redraw on resize, and keeps
  all reflow server-side. The alternative (client-side rewrap of its own
  ring) is explicitly rejected — it would duplicate `posh_term`'s reflow
  on the wrong side of the wire.
- **Frozen live view while scrolled.** Consistent with copy-mode/less;
  output is not lost (it accumulates into the ring) but the viewport does
  not auto-follow until the user returns to the bottom.
- **Alt-screen apps are unaffected.** vim/htop/less run on the alternate
  screen, which has no scrollback; their own scroll handling is untouched.

## The wire-shape constraint (specified in RFC 0002)

The forward-accumulation path needs the server→client sync to deliver
scrollback *growth* cheaply. The current frame body is a prefix/suffix
**byte diff over `dump_vt`** (`FrameBody::Diff` in
`crates/posh/src/remote/sync.rs`), and `dump_vt` is **top-anchored**: it
serializes `[colors/title] [oldest-ring-row … newest-ring-row] [grid
rows] [graphics][modes][cursor]`. Verified consequence: when one line
scrolls off the **top**, the oldest row's bytes sit at the *front* of the
row block, so the shared **prefix** with the prior dump is destroyed and
the whole serialization shifts — the byte diff degrades toward
near-full on **every scroll event**. So the intuition that "the diff
should stay compact" does **not** hold for today's top-anchored
`dump_vt`; compactness requires changing what crosses the wire for
scrollback.

The protocol that carries scrollback growth is specified normatively in
**RFC 0002** (`docs/rfcs/0002-scrollback-sync.md`): a capability-gated
(`SCROLLBACK`, no flag day) **bottom-anchored append** frame body
(`BODY_SCROLLBACK`) whose per-frame cost is bounded by inter-frame
growth, not by scrollback depth. RFC 0002 §2.3 records why the
bottom-anchored append was chosen normatively over a row-indexed
structured body (the resize/reflow boundary already forces a
renumber-and-resync, removing the only advantage of absolute row
indices). This FDR records the user-facing feature and the disqualifying
finding about the top-anchored byte-diff; RFC 0002 owns the wire
contract.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| client ring depth | TBD (≤ server's 10k) | bounds client memory for a durable local reader | users scroll past it routinely, or memory is a concern on the client |
| server scrollback-growth coalescing | TBD | a burst of output need not emit one `BODY_SCROLLBACK` per row; the server MAY batch appended rows per frame | per-row frames flood the link during heavy output |
| pre-attach back-fill | none (forward-only) | ships the common case (history made while attached) without a new request path | users frequently want pre-attach lines interactively, not via `posh history` |

## More Information

- Feature: github #43 (no session scrollback access); root cause
  #3/#28 (kitty turns the wheel into arrows on the alt screen and ignores
  DECSET 1007).
- Builds on FDR 0002 (terminal takeover/restore — why the outer
  scrollbar shows the takeover screen, not the session) and the
  `POSH_GRAB_MOUSE` wheel-intercept predicate (ADR 0002).
- Sync internals: `crates/posh/src/remote/sync.rs` (`FrameBody`,
  `make_diff`), `crates/posh/src/remote/client.rs` (`apply_frame`,
  `render`, `server_term`), `crates/posh-term/src/dump.rs` (`dump_vt`
  top-anchored serialization), `crates/posh-term/src/screen.rs`
  (per-`Screen` scrollback ring; primary has depth, alt has zero).
- RFC 0002: posh Scrollback Sync Protocol
  (`docs/rfcs/0002-scrollback-sync.md`) — the normative wire contract
  (the `SCROLLBACK` capability and `BODY_SCROLLBACK` frame body) this
  feature is built on.
