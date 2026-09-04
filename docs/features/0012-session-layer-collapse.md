---
status: proposed
date: 2026-07-01
promotion-criteria: >
  exploring -> proposed (MET 2026-09-04): the trigger, detach semantics, and
  offer-vs-automatic UX are decided (automatic on a typed in-session attach;
  replace; per-viewport targeting via the most-recent-input connection) and
  drafted as an RFC 0008 amendment; FDR 0011's relay is experimental.
  proposed -> experimental: an in-session `posh attach <sibling>` switches the
  issuing viewport in place — a LOCAL client re-dials, and a viewport attached
  through the M2 mux channel (or the per-invocation relay) is retargeted on
  the session host — with a `Full` keyframe reset, the previous session left
  running detached, and other attached viewports untouched.
  experimental -> testing: in daily use on the fleet worker flow (jump from
  `clown list` to any worker's session) with no fallback to nested
  double-attach and no force-synced sibling viewports.
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

This record's three interface decisions were RESOLVED 2026-09-04 (the
in-place-switch design review; the diagrams live in the review's `.tmp/`
sketches and the mechanism below). The switch generalizes the original
collapse: it covers the tunneled case (the clown self-wrap) AND the plain
local case (a viewport on the session host jumping between siblings), through
one mechanism.

### The mechanism (decided): daemon-routed, per-viewport retarget

An in-session `posh attach <sibling>` (detected via `$POSH_SESSION`, the
signal the nesting guard reads today) becomes a **switch request**: the inner
command validates the target exists (strict-attach semantics kept), sends the
request over the current session daemon's own IPC socket, and exits. The
daemon routes a `Switch` to exactly ONE attached connection — **the one whose
input arrived most recently** (tmux's current-client heuristic; the daemon
already sees which connection each input frame came from). That connection
re-homes:

- a **local client** re-dials the target daemon's Unix socket;
- an **M2 mux session channel** (the post-mux-promotion common case) or a
  per-invocation **relay** re-homes its daemon-side connection on the session
  host — the FDR 0012 retarget proper. The AEAD-UDP wire to the viewport's
  host is untouched: no reconnect, no key change, no new process, no new hop.

Because every relay/channel serves exactly one viewport, per-connection
targeting IS per-viewport targeting: sibling viewports on the same session
are never force-synced. The target daemon's attach handshake sends the
`Full` keyframe (Decision 3 below) — the repaint is the ordinary attach path.

### Decision 1 — offer vs. automatic: AUTOMATIC (decided 2026-09-04)

For a typed in-session `posh attach`, switching without a prompt is what the
user meant — the command is the consent. The offer prompt (drafted below for
history) is demoted to a tuning lever, aimed at the *programmatic* surprise
case (clown's self-wrap) if it proves jarring in practice; the non-TTY
discipline stands regardless (a non-TTY invocation errors with the action
rather than switching silently under a script).

### Decision 2 — replace vs. stack: REPLACE (decided 2026-09-04)

Detaching after a switch tears down the viewport's attachment; you return to
your local prompt, and BOTH sessions remain durable and reattachable. The
switch workflow is sibling navigation, not diving — and replace keeps the
relay/channel single-target (no stack to carry across roams, the risk the
stack option's roaming question exposed). Stack (dive/pop) is demoted to a
tuning lever with the roaming-survival question as its admission price.

### Decision 3 — the state reset is a `Full` keyframe (settled)

Retargeting the transport at a new daemon is, on the wire, identical to a fresh
attach: the new daemon sends a `Full` keyframe (RFC 0008 §2) and the client
repaints. **The screen/state reset on collapse is expected and is exactly this
keyframe** — no new sync mode, no divergence handling. This is the same reset
that already happens on every first attach and every roam reconnect.

## Examples

The fleet-navigation flow (the 2026-09-04 motivating case), with the decided
automatic + replace semantics — the viewport is on `laptop`, the sessions on
`box`, attached through the M2 mux wire:

    laptop$ posh box:dev                # viewport rides a mux session channel
    box:dev$ clown list                # ...spot a worker you want to visit
    box:dev$ posh attach s-1           # in-session attach = SWITCH
    switching to s-1
    # the channel re-homes onto s-1's daemon on box; a Full keyframe
    # repaints; dev keeps running, detached; the UDP wire never blinks.
    # Any OTHER viewport watching dev stays exactly where it was.
    box:s-1$ ... work ...
    <Ctrl-\>                            # replace: detach to the LOCAL prompt
    laptop$ posh box:dev               # both sessions still there, reattach at will

The original tunneled collapse (clown's self-wrap) is the same mechanism —
`sc start` execs clown, clown runs `posh attach w1`, the viewport switches to
`w1` instead of nesting a second posh layer.

Non-TTY (a script or command substitution) never switches implicitly — it
errors with the action it would have taken, mirroring the FDR 0011 picker
discipline:

    $ some-script-that-runs-posh-attach-inside-a-session
    posh: refusing to switch on a non-TTY; run `posh attach --detach w1` to
          ensure the session detached, or attach from an interactive terminal

## Limitations

- **Requires the relay (Architecture B) — now met.** Under Architecture A
  there is no relay to retarget; FDR 0011's relay is `experimental` and the
  M2 mux channel table (the post-promotion common case) applies the same §3
  contract per channel, so the retarget lives in both. The `--ephemeral`
  Arch-A residual keeps the old hard error.
- **The most-recent-input heuristic can misfire.** Two people typing into the
  same session within the routing window could switch the other's viewport.
  Accepted for v1 (the shared-session case is rare and the misfire is
  recoverable — reattach); per-viewport identity (RFC 0014 idents on the
  attach) is the precise fix if it bites.
- **Retarget during a wire reconnect must be defined.** A Switch arriving
  while the mux wire is in the posh#162 reconnecting state interacts with the
  retained-channel re-OPEN logic; the implementation must pin an order
  (simplest: the retarget updates the channel's stored target, so the re-OPEN
  drives the NEW target).
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
| switch trigger UX | automatic (a typed in-session attach is the consent) | the command IS the intent; a prompt would tax the fleet-navigation flow | the programmatic self-wrap case (clown) proves jarring — reintroduce the offer prompt there, or clown pre-answers |
| detach semantics | replace (detach to the local prompt) | sibling navigation, single-target relay state, no stack-across-roam question | "dove in, pop back" is missed often enough to pay for a roam-surviving target stack |
| viewport targeting | most-recent-input attached connection | per-connection = per-viewport (one relay/channel per viewport); no identity machinery needed | shared-session misfires (see Limitations) — key the switch on RFC 0014 client idents instead |
| non-TTY behavior | error with the action | deterministic scripts, mirrors FDR 0011 picker | a scripted flow needs switch-by-default without a TTY |

## More Information

- **FDR 0011** (`0011-unified-durable-sessions.md`) — the unification this
  feature attaches to the tail of; its relay (Architecture B) is the enabling
  step, and this record is the layer-collapse UX that the relay makes
  expressible.
- **RFC 0008** (`docs/rfcs/0008-unified-session-frame-transport.md`) — §3.1
  (amended 2026-09-04) specifies the retarget: the daemon-routed trigger, the
  per-viewport most-recent-input routing rule, and the single-target
  (replace) model; §2 (`Full` keyframe on attach) is the state-reset
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
