---
status: proposed
date: 2026-08-24
promotion-criteria: >
  proposed -> experimental: `ph` exists as a front-door binary routing to
  `posh start` / `posh attach`, the colon-discriminator grammar and the `:+`
  create sigil are implemented, and fish completion (host union + cached remote
  session names) is installed and in daily use on both fleet hosts. The TUI
  modes (bare `ph`, `ph host:`) MAY still error with the candidate list until
  FDR 0016 lands.
  experimental -> testing: fish and bash completion both shipped; the picker
  TUI modes route through the FDR 0016 surface; two weeks of `ph` as the
  primary attach front-door with no fallback to raw `posh attach`.
---

# `ph`: the session front-door

## Problem Statement

FDR 0011 unifies durable sessions behind `posh attach` and removes the
ephemeral roaming shell — a bare `posh box` no longer spawns a throwaway shell.
That is the right *model*, but `posh attach box:dev`, `posh attach box:`, and a
bare `posh attach` are verbose for what is the highest-frequency, muscle-memory
action a terminal roamer performs: start-or-attach a session. Three gaps remain
on top of FDR 0011:

- **No terse front-door.** The unified interface is correct but long. A roamer
  wants a two-character command with shell completion, not `posh attach` typed
  dozens of times a day.
- **No create-new shortcut.** FDR 0011's only create paths are the picker's
  "+ create new…" row or naming a fresh session explicitly. There is no one-shot
  "give me a fresh durable session on box" that does not require inventing a name
  up front.
- **The local-vs-host discriminator leans on a heuristic.** `posh`'s target
  grammar (RFC 0001) infers "remote host" from a `.`, `@`, or `:` in the token,
  which is fine for `posh` but surprising as the basis for a muscle-memory
  front-door (`ph web` — local session or the host `web`?).

`ph` is that front-door. It is deliberately **not** a new session model or a
second grammar to maintain: it is a thin router over FDR 0011's primitives, plus
the create sigil, plus shell completion, plus the shared picker surface (FDR
0016).

## Interface

**`ph` resolves a target against the unified reachable-session listing (FDR
0011's `posh list`) and dispatches to a primitive.** It owns no session logic —
it decides *start vs attach* and *local vs host*, then execs the underlying
`posh` verb. Two primitives sit under it:

- **`posh start [target]`** — create a durable session (strict): auto-id when
  unnamed or with the `+` sigil, named otherwise; errors if a named target
  already exists.
- **`posh attach <target>`** — attach an *existing* durable session (strict):
  errors if the target is absent.

The `posh` verbs are deliberately strict, single-purpose primitives — `posh`
never guesses. The **create-or-attach ergonomic lives in `ph`**: it resolves the
target against the listing and dispatches to `posh attach` when the session
exists, `posh start` when it does not (`:+` always forces `start`). This
supersedes FDR 0011's original attach-or-create `posh attach`, which is amended
to strict attach.

### Grammar

The **colon is the sole local-vs-host discriminator**. A bare word (no colon) is
*always* a local session; a host is *always* written with a colon. There is no
bare `ph <host>` form — this is a deliberate, clean divergence from `posh`'s
`.`/`@`/`:` heuristic, trading one convenience (typing a bare hostname) for zero
ambiguity in the highest-frequency command.

| invocation | meaning | routes to | stage |
|---|---|---|---|
| `ph` | picker across **all** hosts + local | start / attach | **FDR 0016** |
| `ph <host>:` | picker scoped to `<host>` | start / attach | **FDR 0016** |
| `ph <host>:+` | create a **new** auto-id session on `<host>` | `posh start <host>` | now |
| `ph <host>:<session>` | resolve: exists → attach, else create | `posh attach`/`posh start` | now |
| `ph <session>` | **local** session (resolve → attach or create) | `posh attach`/`posh start` | now |
| `ph :+` | create a new auto-id **local** session | `posh start` | now |

`<host>` accepts the same `[user@]host` forms `posh` does; only the leading
local-vs-host decision differs (colon required).

### Auto-id sessions are managed by their activity label, not their id

`:+` and the picker's create-new make a durable session with an **auto-generated
id the user never types**. This is only usable because FDR 0011 + RFC 0013 §5
surface each session by its **frontmost activity label** (the terminal title
plus the foreground-process command — RFC 0013 §5) in `posh list`, the
picker, and completion. The auto-id is the stable machine key; the activity
label ("`vim ~/notes` on box", "`cargo build` on web") is what the human
reads and selects. This is the direct enabler of FDR 0011's no-auto-reap
decision: because the user can *see* what each detached session is running, the
system never has to guess whether an idle-looking session holds valuable work.

### Durable remote sessions ride the mux

A `ph <host>:…` durable remote session rides the M2 mux connection
(`POSH_MUX_SESSIONS`, RFC 0011) like any other named `host:session` attach, and
inherits its wire-reconnect survival: a mux-wire death stalls and reattaches
rather than dropping the client to a shell (the durable endpoint the whole
front-door converges onto). `ph` adds no transport of its own.

### Shell completion (fish prioritized, bash to follow)

`ph <TAB>` completes against three surfaces:

- **Hostnames** — the union of ssh config `Host` aliases, `tailscale status`
  peers, and live mux endpoints (`posh mux ls`). This union is the initial
  source set and may grow.
- **Session names** — from the reachable-session listing. **Local** sessions are
  queried live; **remote** session names are served from a **cache** refreshed
  out of band (on endpoint changes), so interactive completion never stalls on a
  slow or unreachable endpoint.
- **Syntax** — the `:` host separator and the `:+` create sigil.

fish is the priority target (the eng default shell); bash completion follows the
same candidate sources.

## Examples

    $ ph web:dev          # attach dev on web (create if missing)
    $ ph web:+            # fresh auto-id durable session on web
    $ ph scratch          # local session "scratch" (attach or create)
    $ ph :+               # fresh auto-id local session
    $ ph web:<TAB>        # completes web's session names from the cache
    $ ph                  # picker over every reachable session (FDR 0016)

## Limitations

- **The TUI modes are blocked on FDR 0016.** Bare `ph` and `ph <host>:` open the
  cross-host / host-scoped picker, which is the same surface FDR 0016 grows out
  of the command palette. Until FDR 0016 lands, those two forms MAY error with
  the candidate list (the FDR 0011 non-TTY discipline) rather than launching a
  chooser. The direct-grammar forms (`:+`, `:<session>`, bare-local) and
  completion ship first and stand alone.
- **`posh ssh` is retired (decided).** Dropping bare `posh <host>` (FDR 0011)
  removes the ephemeral roaming shell, and the explicit `posh ssh <host>`
  subcommand that wrapped it is retired with it — durable sessions via `ph` cover
  remote access. The roaming *transport* (FDR 0003) is unaffected: durable
  sessions still roam.
- **Completion cache staleness.** A remote session created since the last cache
  refresh will not complete until the cache updates; the live `posh list box:`
  always sees it. This is the deliberate latency-vs-freshness trade for
  interactive completion.

## Tuning Levers

| Lever | Current | Rationale | Change signal |
|---|---|---|---|
| remote session-name completion source | out-of-band cache | interactive completion must never block on a slow endpoint | a fast local endpoint makes live query imperceptible and users hit staleness often |
| host completion source set | ssh config + tailscale + mux (union) | covers the reachable set without a config file | a host the user reaches by another means is routinely missing |
| bare `ph`/`ph host:` before FDR 0016 | error with candidates | non-TTY discipline, no half-built picker | the direct-grammar-only cut ships and the picker is wanted immediately |

## More Information

- **FDR 0011** (`0011-unified-durable-sessions.md`) — the durable-session model
  and the strict `posh start`/`posh attach` primitives `ph` fronts; its
  no-auto-reap decision the activity label enables, its removal of the bare-host
  ephemeral shell.
- **FDR 0016** (`0016-cross-host-session-switcher.md`) — the palette-as-picker /
  cross-host switcher that the TUI modes (bare `ph`, `ph host:`) open; `ph` and
  the switcher share one picker surface.
- **FDR 0009** (`0009-command-palette.md`) — the palette renderer FDR 0016 grows
  the picker out of.
- **RFC 0013 §5** (`docs/rfcs/0013-server-introspection-caps.md`) — the frontmost
  activity-label capability that makes auto-id sessions selectable.
- **RFC 0011** (`docs/rfcs/0011-multiplexed-datagram-channels.md`) — the M2 mux a
  `ph host:…` durable session rides; its reconnect survival.
- **FDR 0003** (`0003-mosh-parity-surface.md`) — the roaming transport that
  persists for durable sessions even as the ephemeral roaming *shell* is removed.
- **RFC 0001** (`docs/rfcs/0001-target-grammar-and-capability-table.md`) — the
  `posh` target grammar `ph` deliberately does *not* reuse for its local-vs-host
  decision.
