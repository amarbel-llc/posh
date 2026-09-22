---
status: accepted
date: 2026-09-22
---

# Converge on M2; relay and Architecture A are superseded

## Context and Problem Statement

A roaming client reaches a remote session by one of three routes:

1. **Architecture A** (`remote/server.rs`'s `server_loop`): the remote
   `posh-server` owns the PTY itself, with its own `posh_term::Terminal` and
   `FrameProducer`. There is no session daemon, so the session is neither
   shareable with a local attach nor visible to `posh list`.
2. **The relay** (`remote/relay.rs`): a per-invocation process on the remote
   host that bridges one roaming client to one session daemon. It owns no
   terminal model — the daemon is the single frame producer.
3. **The M2 mux session channel** (`remote/mux.rs`): the attach rides the
   long-lived per-destination mux connection as one channel among several.
   Default on since 2026-09-03.

Maintaining three ingress paths means every session-level feature must either
be implemented three times or silently behave differently depending on which
route an attach happened to take.

## Decision Drivers

* **Features are already becoming transport-dependent.** FDR 0016's session
  stacking needs the client to learn the target of an FDR 0012 re-home. M2
  sends it (`SESSION_WIRE_SWITCH`, `remote/server.rs`); the relay deliberately
  does not, because its own comment reasons that "a relay is single-session and
  dies with its attach, so there is no reconnect to survive" — true for
  reconnect continuity, but the target is now also needed for viewport
  bookkeeping. Without a third implementation, the same user action stacks or
  does not depending on the transport.
* **Architecture A is a second terminal model**, which is precisely what the
  relay's single-model invariant exists to eliminate. It also carries its own
  halves of RFC 0013 and RFC 0014 (`<base>/remote/<pid>.status.sock`,
  `posh status remote-<pid>`) that exist only because it has no session dir.
* **The legacy paths accumulate silently.** A census of one developer host
  (`just debug-posh-builds`) found twelve live Architecture-A roaming servers,
  all on a build several releases old, none of them visible to `posh list`.
* **M2 already covers the case the others exist for.** Durable, shareable
  sessions are the daemon's, and M2 carries them over one connection per
  destination rather than one per invocation.

## Considered Options

1. **Keep all three**, and implement each session-level feature on every path.
2. **Remove Architecture A, keep the relay** as M2's fallback.
3. **Converge on M2** and retire both.

## Decision Outcome

Chosen option: **3, converge on M2.**

Option 1 is what produced the divergence this record exists to stop. Option 2
keeps the path that cannot express re-home notification, so features stay
transport-dependent — the specific problem, unfixed.

### The working rule

Until the code is deleted:

* **No new features on the relay or Architecture A. Bug fixes only.**
* Anything new lands on M2.
* A feature that cannot be expressed over M2 is a signal to fix M2, not to
  extend a legacy path.

This rule is the point of the record. The removal is scheduled below, but the
rule takes effect immediately, because the cost being avoided is paid at
feature time, not at deletion time.

### Sequence

Disabling the alternatives is **not** a prerequisite for M2 covering every
attach; it is the reverse. A forwarding-off attach is itself a non-M2 path
today (the mux endpoint is spawned keyed to an agent source), so disabling
first would break exactly the attaches M2 does not yet carry.

1. **Decouple the M2 session channel from the agent source**, so an endpoint
   can carry session channels with no agent. After this, M2 covers every attach
   shape.
2. **Delete the `POSH_MUX_SESSIONS` opt-out**, keeping the code reachable only
   by runtime fallback.
3. **Soak on the existing fallback warning** (`mux-fallback`, posh#156): a
   quiet fleet is the evidence that M2 covers everything; a noisy one names the
   host still to upgrade.
4. **Disable the non-M2 paths**: the fallback becomes a hard error,
   Architecture A refuses to start, `POSH_MUX=0` / `POSH_RELAY=0` stop being
   escape hatches.
5. **Delete the code**, which by then is provably unreachable.

Steps 3 and 4 are deliberately not merged: this host currently serves live
Architecture-A peers, and they should be named by a warning rather than
discovered by a breakage.

### What goes, at step 5

* `crates/posh/src/remote/relay.rs`.
* `remote::server::server_loop` and the Architecture-A halves of
  `remote/server.rs` (`agent_only_loop` and the mux peer path **stay** — they
  serve M2).
* The `POSH_RELAY` and `POSH_MUX_SESSIONS` gates.
* `<base>/remote/<pid>.status.sock`, its pidfile, and `posh status
  remote-<pid>` / `posh status <pid>`, with the RFC 0013 and RFC 0014 sections
  that specify them.

### Consequences

* Good: one ingress path, so a session-level feature is implemented once and
  behaves the same everywhere.
* Good: the single-model invariant becomes structural rather than a property
  the relay upholds while a sibling module violates it.
* Bad: after step 4, a remote too old to speak M2 is unreachable rather than
  slow. That is a deliberate forcing function for the fleet upgrade, and the
  soak in step 3 is what makes it a scheduled event rather than an outage.
* Bad: the debugging escape hatches (`POSH_MUX=0`, `POSH_RELAY=0`) go away.
  Their replacement is the mux daemon's own instrumentation — `posh mux ls`,
  the per-key logs, and the SIGUSR2 status dump.
* Neutral: nothing is deleted before step 5, so the retreat from any step is
  to stop, not to restore.

## More Information

* FDR 0012 (session layer collapse) specifies the in-place switch whose
  notification gap motivated this.
* RFC 0011 §5 and the M1/M2 rollout notes in `remote/mux.rs` describe the
  endpoint and its channel model.
* `just debug-posh-builds` censuses the builds and paths live on a host,
  including the Architecture-A servers under `<base>/remote/`.
* posh#209 tracks the sequence above; posh#156 is the fallback-warning
  instrumentation step 3 soaks on.
