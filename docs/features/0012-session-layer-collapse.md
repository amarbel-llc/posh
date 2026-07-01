---
status: exploring
date: 2026-07-01
promotion-criteria: >
  exploring -> proposed: the collapse trigger (server-side detection vs.
  client-initiated), the detach semantics (replace vs. stack), and the "offer
  vs. automatic" UX are decided and drafted as an RFC 0008 amendment. Blocked on
  the relay existing: this feature is unimplementable under Architecture A (the
  double model has no relay to retarget) and MUST NOT promote past `proposed`
  until FDR 0011's relay (RFC 0008 §3) is at least `experimental`.
  proposed -> experimental: a remote client attached through the relay can
  collapse into an inner local session started on the far host (the clown/
  spinclass case), with a `Full` keyframe reset, and detach behaves per the
  chosen semantics. experimental -> testing: in daily use on the dev-host
  worker flow with no fallback to nested double-attach.
---

# Session layer collapse (attach through a tunnel, don't nest)

## Problem Statement

When you are already inside a posh session that tunnels to a host, and something
on that host starts or attaches to *another* posh session, you get two stacked
posh layers instead of one. The motivating case: `posh user@dev-host` (a
remote roaming session) whose shell runs `sc start`, which execs clown, which —
because posh is clown's default multiplexer — self-wraps in `posh attach
<session>`. You now have a remote posh layer with a local posh layer nested
inside it: two terminal models, two detach keys, two alt-screen takeovers, and
keystrokes/frames threading through both. The user's intent was "attach me to
the clown session on dev-host," not "run a second posh inside my first posh."
posh should recognize the nested-attach and collapse to a single attached
session reached *through* the existing tunnel.

## Interface

**The idea.** A posh client attached to a host through the roaming transport can
be *retargeted* at a different session daemon on that same host, reusing the one
transport instead of nesting a second client inside the first session's shell.
When a local `posh attach` is invoked inside an existing posh session (detected
via `$POSH_SESSION`, the signal the current nesting guard already reads), posh
collapses the two layers: the outer transport re-homes onto the inner session's
daemon, and the outer session's shell is left running underneath.

This is a UX feature whose shape drives the implementation; three interface
decisions are open and are the substance of this record.

### Decision 1 — offer vs. automatic

Today the nesting guard hard-errors: *"cannot attach to a session from within a
session"* (bypassed by `--detach`, which clown's *spawn* template uses; the
*start*/*resume* templates do not). Options, in order of least to most magic:

- **Offer (recommended).** On detecting an in-session `posh attach`, prompt:
  *"You're already in a posh session tunneled to <host>. Attach <session>
  directly through this tunnel instead of nesting? [Y/n]"* — a TTY-gated prompt
  matching the FDR 0011 picker's non-TTY discipline (never prompt on a
  non-TTY; error with the candidate action instead). Preserves the current
  safety (no silent surprise) while making the good path one keystroke.
- **Automatic.** Collapse without asking whenever the inner target resolves to a
  session reachable through the current tunnel. Cleanest when it is right;
  surprising when the user genuinely wanted a nested session (rare, but real —
  e.g. debugging posh-in-posh itself).
- **Keep erroring, add a flag.** `posh attach --collapse <session>` opts in
  explicitly. Safest, least discoverable; effectively documentation, not a UX
  improvement.

### Decision 2 — replace vs. stack detach semantics

After collapsing into the inner session, what does `Ctrl-\` (detach) do?

- **Replace.** Detaching from the inner session tears down the transport; the
  outer session's shell (still running `sc start` underneath) is left detached
  and reattachable separately. Simpler: the relay tracks one daemon target at a
  time. The outer shell is not lost — it is a durable session — but you return
  to your *local* prompt, not to the outer session.
- **Stack (dive/pop, recommended for the mental model).** The relay holds a
  *stack* of daemon targets; collapsing pushes the inner session, detaching pops
  back to the outer session (its shell reappears where you left it). Matches the
  intuition "I dove into clown; detaching brings me back to where I was." Costs
  the relay real state (a target stack) and raises a roaming question (what
  happens to the stack across a network roam — see Limitations).

### Decision 3 — the state reset is a `Full` keyframe (settled)

Retargeting the transport at a new daemon is, on the wire, identical to a fresh
attach: the new daemon sends a `Full` keyframe (RFC 0008 §2) and the client
repaints. **The screen/state reset on collapse is expected and is exactly this
keyframe** — no new sync mode, no divergence handling. This is the same reset
that already happens on every first attach and every roam reconnect.

## Examples

The motivating flow, with the recommended offer + stack semantics:

    laptop$ posh user@dev-host          # remote roaming session (the tunnel)
    dev-host$ sc start                 # clown self-wraps in `posh attach w1`
    ┌ posh ────────────────────────────────────────────────┐
    │ You're already in a posh session tunneled to          │
    │ dev-host. Attach `w1` directly through this tunnel    │
    │ instead of nesting? [Y/n]                              │
    └────────────────────────────────────────────────────────┘
    # Y: the transport re-homes onto w1's daemon; a Full keyframe
    #    repaints; you are now driving clown in w1 over the SAME
    #    roaming transport — one client, one model on the far side.
    dev-host$ ... work in clown ...
    <Ctrl-\>                            # stack: pop back to the outer
    dev-host$                          # outer session's shell, where sc start ran

Non-TTY (a script or command substitution) never prompts — it errors with the
action it would have taken, mirroring the FDR 0011 picker discipline:

    $ some-script-that-runs-posh-attach-inside-a-session
    posh: refusing to nest a session on a non-TTY; run `posh attach --collapse w1`
          to attach through the current tunnel, or `--detach` to spawn detached

## Limitations

- **Requires the relay (Architecture B).** Unimplementable under today's double
  model (FDR 0001 Architecture A): there is no relay to retarget — the outer
  `posh-server` owns a full PTY + terminal model the inner client genuinely
  types into. This feature is a natural addition to the RFC 0008 §3 relay and
  should be designed into that contract while it is still `proposed`, but it
  cannot ship before the relay does.
- **Stack semantics interact with roaming.** If the transport holds a target
  stack and the network roams (mosh-style reconnect), the stack must survive the
  roam — the client reconnects to the relay, which must still hold (or
  reconstruct) the stack. Replace semantics sidestep this entirely. This is the
  main cost of the stack option and a key input to Decision 2.
- **Widens the agent-forwarding gap (#103).** RFC 0008 §3 binds the forwarded
  agent (`agent/sock`) at the relay, and FDR 0011 already notes a session whose
  shell was spawned without a forwarding connection does not pick up a later
  attach's agent. Collapsing into such a session inherits that gap: the inner
  session sees whatever `SSH_AUTH_SOCK` it was spawned with, not the tunnel's
  forwarded agent. Not a blocker; a known edge the collapse makes more common.
- **Only same-host collapse in v1.** Collapse retargets within the host the
  transport already reaches. Chaining across hosts (collapse, then the inner
  session is itself a tunnel to a third host) is out of scope.
- **A genuinely-wanted nested session needs an escape hatch.** Whatever
  Decision 1 lands on, there must remain a way to *actually* nest (for debugging
  posh-in-posh, or an intentional inner session) — the `--detach` spawn path
  already provides one; an explicit non-collapsing attach may be needed too.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| collapse trigger UX | offer (prompt) | preserves the current no-surprise safety while making the good path one keystroke | users routinely answer Y and want it automatic, or the prompt interrupts a known-good flow (clown could pre-answer) |
| detach semantics | stack (dive/pop) | matches the "dove in, come back" mental model | the roaming-survival cost of a per-transport target stack proves too high; replace is the fallback |
| non-TTY behavior | error with the action | deterministic scripts, mirrors FDR 0011 picker | a scripted flow needs collapse-by-default without a TTY |

## More Information

- **FDR 0011** (`0011-unified-durable-sessions.md`) — the unification this
  feature attaches to the tail of; its relay (Architecture B) is the enabling
  step, and this record is the layer-collapse UX that the relay makes
  expressible.
- **RFC 0008** (`docs/rfcs/0008-unified-session-frame-transport.md`) — §3 (the
  relay contract) is where a retarget trigger and the replace/stack target
  model would be specified; §2 (`Full` keyframe on attach) is the state-reset
  mechanism this feature reuses.
- **FDR 0001** (`0001-unified-host-session-namespace.md`) — the A→B→C transport
  progression; this feature is a B-and-beyond capability.
- **FDR 0010** (`0010-remote-detached-spawn.md`) — the `--detach` spawn path
  that bypasses the current nesting guard and is one escape hatch for
  intentional nesting.
- The current nesting guard: `crates/posh/src/session/client.rs`
  (`cmd_attach`, the `$POSH_SESSION` check that today hard-errors and would
  instead trigger the collapse offer).
- clown's multiplexer defaults and the `start`/`resume`/`spawn` templates that
  produce the nesting: clown's `default-clownfile` (`multiplexer = "posh"`).
