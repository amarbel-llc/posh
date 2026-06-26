---
status: exploring
date: 2026-06-26
---

# Evolutionary Predictive-Echo Metric Vector and Species Contract

## Abstract

This document specifies the interface between posh's predictive local-echo
client and an evolved genetic-programming (GP) predictor supplied by the
mephisto framework. It defines the *metric vector* — the numeric terminal set
the evolved program consumes — its provenance and transport, the output
contracts of the two predictor species (a policy *controller* and a
*from-scratch* cell predictor), the inviolable echo-leak safety invariants, the
fitness/rank contract, the schema-versioned persisted-genome format, and the
online single-generation evolution loop. It is the contract both posh and
mephisto build against; it does not specify mephisto's internal evolution
mechanics.

## Introduction

posh inherits mosh's predictive local echo (FDR 0006): the client speculatively
echoes keystrokes to hide the round-trip, then reconciles against authoritative
server paints. The adaptive engine is governed by eight timing constants copied
verbatim from mosh's `terminaloverlay.h` and never re-tuned for posh's
transport. This pilot replaces the *policy* those constants encode with an
evolved GP program that consumes a rich, live metric vector (transport health,
render headroom, both-host environment) and continuously evolves *online*
against real prediction outcomes, persisting its genome across sessions so the
predictor improves over its lifetime.

Two predictor species are offered as user-selectable models:

- **Controller** — the GP program outputs *policy decisions* that drive posh's
  existing, deterministic echo machinery (overlay/cull/render). The safe arm.
- **From-scratch** — the GP program outputs *predicted cells* directly,
  replacing the overlay-decision logic. The research arm.

Scope: this RFC specifies (1) the metric vector, (2) how each metric reaches the
client-side predictor, (3) the two species' input/output type contracts, (4) the
leak-safety invariants, (5) the fitness/rank contract, (6) the persisted-genome
format, and (7) the online evolution loop integration. It does NOT specify
mephisto's selection/crossover/mutation internals, which are mephisto's concern;
it depends on them only through the public API named in §7.

Background: FDR 0006 (optimistic local echo, the gate mechanism and the
`Predictor` trait this plugs behind); RFC 0001 (target grammar and per-frame
capability table, the channel new server-forwarded metrics extend).

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Specification

### 1. Architecture

The predictor runs in the posh **client**. Each session, the client:

1. seeds (or loads, §6) a population of GP genomes;
2. on every keystroke, evaluates the *champion* genome to produce a prediction
   (per the species output contract, §4), subject to the runtime safety gate
   (§5);
3. on every authoritative server frame, scores **every** genome in the
   population against the just-arrived truth (§5.2 fitness), then advances the
   population one generation (§7);
4. at session end, persists the population (§6).

The evolved program is a pure function of the **metric vector** (§2) plus, for
the from-scratch species, the input bytes and current screen state. It MUST NOT
have side effects other than producing its declared output.

### 2. The Metric Vector

The metric vector is the GP terminal set: the inputs the evolved program may
read. Every terminal MUST be a scalar coercible to `f64`. Non-numeric signals
MUST be reduced to scalars before exposure:

- A **categorical** signal (e.g. frontmost-app identity) MUST be exposed as a
  stable numeric id obtained by hashing the source string into a bounded id
  space (`u32`, then widened to `f64`). The hash MUST be stable across sessions
  and posh versions so a persisted genome's equality/branch tests remain valid.
- A **structured** signal (e.g. a process tree) MUST be reduced to a fixed set
  of scalar features (counts, depths, a hashed foreground-process id); the raw
  structure MUST NOT be a terminal.

A terminal whose source is momentarily unavailable MUST be presented as a
sentinel `NaN`, and the program's evaluation harness MUST treat `NaN`
propagation as a non-fatal value (a genome that depends on an unavailable
terminal simply scores poorly), never as a panic.

The metric vector is versioned (§6). The terminal set for schema version 2
(v1 was the transport/host set below through `remote_fg_proc_id`; v2 appends the
screen-state block, read client-side from the displayed `Snapshot`):

| Terminal | Type | Unit | Provenance | Notes |
|---|---|---|---|---|
| `srtt_ms` | f64 | ms | client | smoothed RTT (`datagram.rs`) |
| `rto_ms` | f64 | ms | client | retransmit timeout |
| `send_interval_ms` | f64 | ms | client | server frame cadence |
| `retransmit_rate` | f64 | 1/s | server-forwarded (deferred) | server-side counter; not yet in `CAP_METRICS` |
| `outstanding` | f64 | frames | client | unacked frames in flight |
| `bw_up_bps` | f64 | bit/s | client | upstream bandwidth estimate |
| `fps` | f64 | 1/s | client | derived from loop-iter cadence |
| `loop_busy_frac` | f64 | ratio | client | busy/(busy+idle) headroom |
| `apply_us` | f64 | µs | client | recent frame-apply cost |
| `compose_us` | f64 | µs | client | recent compose cost |
| `dump_vt_us` | f64 | µs | server-forwarded (deferred) | server-side cost; not yet in `CAP_METRICS` |
| `pred_correct_rate` | f64 | ratio | client | predictor self-feedback, recent window |
| `pred_nocredit_rate` | f64 | ratio | client | " |
| `pred_incorrect_rate` | f64 | ratio | client | " |
| `epoch_lag` | f64 | frames | client | confirmed-vs-current epoch gap |
| `alt_screen` | f64 {0,1} | bool | client (reconstructed `server_term`) | full-screen app active |
| `echo_flag` | f64 {0,1} | bool | client (per-frame `FLAG_ECHO`) | remote PTY termios `ECHO` bit |
| `local_load1` | f64 | ratio | client host | 1-min load avg / ncpu |
| `remote_load1` | f64 | ratio | server-forwarded | " on the server host |
| `local_mem_avail_frac` | f64 | ratio | client host | available memory fraction |
| `remote_mem_avail_frac` | f64 | ratio | server-forwarded | " on the server host |
| `local_frontmost_app` | f64 | id | client host (OSC + OS) | hashed app id (categorical) |
| `remote_frontmost_app` | f64 | id | server-forwarded (OSC + proc tree) | hashed foreground-app id |
| `remote_proc_count` | f64 | count | server-forwarded | session process-group size |
| `remote_fg_proc_id` | f64 | id | server-forwarded | hashed foreground process name |
| `screen_rows` | f64 | rows | client (Snapshot) | v2; terminal height |
| `screen_cols` | f64 | cols | client (Snapshot) | v2; terminal width |
| `cursor_row` | f64 | row | client (Snapshot) | v2; cursor row |
| `cursor_col` | f64 | col | client (Snapshot) | v2; cursor column |
| `cursor_visible` | f64 {0,1} | bool | client (Snapshot) | v2 |
| `pen_fg` | f64 | color | client (Snapshot) | v2; SGR fg at cursor cell (`-1`=default, else packed RGB) |
| `pen_bg` | f64 | color | client (Snapshot) | v2; SGR bg at cursor cell (same encoding) |
| `pen_bold` | f64 {0,1} | bool | client (Snapshot) | v2 |
| `pen_dim` | f64 {0,1} | bool | client (Snapshot) | v2 |
| `pen_italic` | f64 {0,1} | bool | client (Snapshot) | v2 |
| `pen_underline` | f64 | ordinal | client (Snapshot) | v2; 0=none,1=single,2=double,3=curly,4=dotted,5=dashed |
| `pen_blink` | f64 {0,1} | bool | client (Snapshot) | v2 |
| `pen_inverse` | f64 {0,1} | bool | client (Snapshot) | v2 |
| `pen_invisible` | f64 {0,1} | bool | client (Snapshot) | v2 |
| `pen_strikethrough` | f64 {0,1} | bool | client (Snapshot) | v2 |
| `reverse_video` | f64 {0,1} | bool | client (Snapshot) | v2; DECSCNM |
| `bracketed_paste` | f64 {0,1} | bool | client (Snapshot) | v2; DECSET 2004 |
| `focus_reporting` | f64 {0,1} | bool | client (Snapshot) | v2; DECSET 1004 |
| `alternate_scroll` | f64 {0,1} | bool | client (Snapshot) | v2; DECSET 1007 |
| `app_cursor_keys` | f64 {0,1} | bool | client (Snapshot) | v2; DECCKM |
| `app_keypad` | f64 {0,1} | bool | client (Snapshot) | v2; DECKPAM |
| `mouse_mode` | f64 | DECSET# | client (Snapshot) | v2; 0=off, else 9/1000/1002/1003 |
| `mouse_encoding` | f64 | DECSET# | client (Snapshot) | v2; 0=default, else 1005/1006/1016 |

Note: the alternate *screen buffer* state (`alt_screen`) is NOT carried on the
client `Snapshot` (which has only `alternate_scroll`, DECSET 1007), but the
client already reconstructs the authoritative server terminal (`server_term`)
and reads `is_alt_screen()` from it directly — the same value the echo gate is
computed from. Likewise `echo_flag` already rides the per-frame `FLAG_ECHO`
runtime bit. So both are client-side and need no capability forwarding.

Terminals marked client are already available to posh today (transport via
`datagram.rs`/`stats.rs`; screen state and the two session-gate signals from the
reconstructed `server_term` / `FLAG_ECHO`). The host/frontmost/proc-tree
terminals and the server-side counters (`retransmit_rate`, `dump_vt_us`) are NEW
signals the server MUST gather and forward (§3). Implementations MAY add
terminals in a later schema version (§6); they MUST NOT remove or renumber
existing terminals within a schema version.

### 3. Metric Provenance and Transport

- **Client-local** terminals (transport, render headroom, predictor feedback,
  `local_*`, screen state) MUST be read directly by the client. The two
  session-gate terminals (`alt_screen`, `echo_flag`) are also client-side:
  `alt_screen` from the reconstructed `server_term`'s `is_alt_screen()`, and
  `echo_flag` from the per-frame `FLAG_ECHO` runtime bit — neither needs a
  capability.
- **Server-forwarded** terminals (`remote_*` host/app/proc) MUST be transported
  over the per-frame capability channel defined by RFC 0001 — the same mechanism
  that carries the `ECHO` flag, scrollback, and exit-status capabilities today
  (FDR 0006) — as a single `CAP_METRICS` capability, negotiated (the client
  advertises it only when a GP species is active) and sampled server-side at a
  throttled interval. The server-side counters `retransmit_rate` and
  `dump_vt_us` are deferred (they need server-side `Stats` read-back getters);
  they ride a later `CAP_METRICS` payload version and remain `NaN` until then.
- **OSC-sourced** identity (`*_frontmost_app`): the client SHOULD derive
  `local_frontmost_app` from the host windowing system and/or OSC title
  sequences (OSC 0/1/2) it receives; the server SHOULD derive
  `remote_frontmost_app` from OSC sequences emitted by the session program
  and/or the foreground process of the session process group, and forward the
  hashed id. Where neither source is available the terminal MUST be `NaN`
  (§2), not a fabricated value.

The frequency of metric-vector refresh is not normative, but the client MUST
present a metric vector that is current as of the most recent server frame when
it evaluates a genome on a server-frame tick.

### 4. Species Output Contracts

Both species consume the metric vector. They differ only in leaf availability
and output type.

#### 4.1 Controller

Additional leaves: none beyond §2. Output: a `PolicyKnobs` value the existing
echo machinery consumes:

| Field | Type | Semantics |
|---|---|---|
| `show` | bool | display the prediction this tick at all |
| `flag` | bool | render the prediction flagged (underline/dim) |
| `confirm_gate_ms` | f64, clamped [0, 5000] | effective confirmation hold before a prediction is shown |
| `suppress_on_ambiguity` | bool | drop the prediction when the local frame already matches (autosuggestion case) |

The controller's `PolicyKnobs` MUST be applied *within* posh's existing
overlay/cull/render pipeline; the controller MUST NOT itself emit cells. Out-of-
range scalar outputs MUST be clamped to the stated range, never rejected.

#### 4.2 From-scratch

Additional leaves: the input byte(s) of the keystroke and a read-only view of
the current displayed `Snapshot`. Output: an ordered list of overlay operations
`{row: u16, col: u16, glyph: char}` plus an optional cursor position. The list
length MUST be bounded (implementation-defined cap, RECOMMENDED ≤ the screen
cell count); a program emitting more operations MUST have its output truncated
at the cap, not rejected.

Both species' outputs are subject to the safety gate (§5.1) before anything
reaches the display.

#### 4.3 Genome representation (multi-output)

A single mephisto GP program has one output root, but both species' outputs are
multi-valued. The representation is mephisto-internal, but the contract this RFC
fixes is:

- **Controller** — the genome MUST realize the four `PolicyKnobs` fields as a
  fixed tuple of single-root programs (one root per field) over the shared
  metric-vector leaf set, so the existing single-root crossover/mutation
  operators apply component-wise without a new operator family. Boolean fields
  are derived by thresholding their root value at 0; `confirm_gate_ms` is its
  root value clamped to [0, 5000] (§4.1).
- **From-scratch** — its variable-length cell-list output (§4.2) does NOT fit a
  fixed-arity tuple; its genome representation is DEFERRED. From-scratch is the
  research arm and is specified here for the output contract only; the
  controller pilot does not depend on it, and persistence (§8) MUST NOT block on
  it.

### 5. Safety Invariants

#### 5.1 Runtime gate (live backstop)

Independent of any species output, the client MUST suppress *all* local echo
when either gate condition holds:

- the server's alternate screen is active (`alt_screen` = 1), or
- the remote PTY's `ECHO` termios flag is off (`echo_flag` = 0).

This gate is a hard runtime wrapper around both species and MUST NOT be
overridable by genome output. It exists because the evaluator (§5.2) scores a
genome only *after* the outcome is observed; without this gate a never-before-
seen mutant could echo one secret keystroke (e.g. a password) before its fitness
is assessed.

#### 5.2 Lethal fitness (evolutionary backstop)

The evaluator MUST assign a *lethal* rank — a sentinel that selection treats as
strictly worst (e.g. `f64::MAX`/`+∞`) — to any genome whose prediction, on the
scored tick, would have echoed input while `echo_flag` = 0. A lethal genome
MUST never be selected as a parent or champion; it MAY exist transiently in the
population. Leak avoidance MUST be modeled as a disqualifying constraint, NOT a
weighted penalty that other objectives can outweigh.

### 6. Fitness and Rank Contract

The species' `Fitness` type MUST retain the raw outcome counters so that
`rank()` can scalarize them: `correct`, `nocredit`, `incorrect`, `resets`,
`epoch_lag`, and a `latency_hidden` measure (server-confirmed predictions ×
their pre-confirmation display time). `rank()` MUST return a single `f64` where
**lower is better**, MUST return the lethal sentinel (§5.2) for a leaking
genome, and SHOULD include a parsimony term over program size/op-cost so that
added metric terminals must earn their place in the tree rather than bloating
it. Objective weights SHOULD be exposed as tunable parameters.

### 7. Online Evolution Loop

The client drives mephisto's public single-generation API. The contract is:

- `mephisto::domain::initial_population(dom, cfg, rng) -> Vec<Genome>` — seed
  once at session start when no persisted population is loaded.
- `mephisto::domain::evaluate_population(dom, &pop) -> Vec<Scored>` — score and
  rank best-first; called per server-frame tick. This call MUST NOT consume
  randomness (it is reproducible).
- `mephisto::domain::step(dom, cfg, rng, &scored) -> Vec<Genome>` — advance one
  generation.
- `Scored { genome, fitness, rank }` — `scored[0]` is the champion the client
  evaluates for display.

The client loop is: seed-or-load once, then per tick `{ evaluate_population;
champion = scored[0]; choose display (§7.1); step }`.

#### 7.1 Adaptive shadow baseline

Whenever a GP species is selected, the client MUST run the `adaptive` predictor
(FDR 0006) concurrently as a shadow baseline and score it each tick with the
same `Fitness`/`rank()` as the population (§6). The predictor whose output is
*displayed* MUST be the better-ranked (lower `rank`) of {adaptive shadow, GP
champion `scored[0]`}. The handover MUST apply hysteresis — the GP champion MUST
beat the adaptive shadow by a sustained margin before it is displayed, and
display MUST revert to adaptive after a sustained regression — so the displayed
predictor does not flap tick-to-tick.

This makes adaptive a permanent floor, not merely a cold-start crutch: the user
never sees the evolved predictor perform worse than today's default. Cold start
is the special case where the adaptive shadow wins because the population has not
yet matured; no separate maturity heuristic is required. The client MUST NOT
display GP output while the adaptive shadow is better-ranked.

### 8. Persisted Genome Format

The population is persisted across sessions. The persisted artifact MUST be a
self-describing blob carrying:

- a `metric_schema_version` integer naming the terminal set (§2) the genomes
  were evolved against;
- the serialized genomes (via mephisto's `serialize`/`deserialize`).

On load, if `metric_schema_version` does not match the running client's schema,
the client MUST NOT feed the genomes to the evolved program directly; it MUST
either migrate them (a documented per-version transform) or discard them and
cold-start (§7). Feeding a genome evolved against a different terminal set is a
conformance violation, because the program's leaf indices would reference the
wrong metrics.

The persisted blob stores program *structure*, not metric *values*; it
therefore does not embed host environment data (see Security Considerations).

## Security Considerations

**Echo of secret input.** The central risk is echoing secret keystrokes (e.g.
passwords) locally. This is addressed in depth by the two-layer model of §5: a
non-overridable runtime gate (§5.1) and lethal fitness (§5.2). An implementation
that weakens either layer — making the gate optional, or modeling leak avoidance
as a soft penalty — is non-conformant.

**Metric-vector information exposure.** The metric vector carries sensitive host
information: frontmost-app identity, process-tree features, and load on both
hosts. The persisted genome (§8) stores program structure only and therefore
does NOT contain these values. However, any *diagnostic logging* of the live
metric vector (e.g. under a debug-log facility) WOULD contain frontmost-app and
process identifiers. Implementations MUST NOT write raw metric-vector snapshots
to durable or world-readable logs without redacting the categorical identity and
process terminals. App and process identities are exposed to the GP only as
opaque hashed ids (§2), which limits but does not eliminate this exposure.

**Server-forwarded metrics are trusted.** Server-forwarded terminals (§3) arrive
over the existing authenticated, encrypted capability channel (RFC 0001); they
inherit posh's existing trust model in which the server is trusted. A
compromised server could supply misleading metrics, but a compromised server can
already drive the client's display arbitrarily, so this introduces no new trust
boundary.

## Conformance Testing

The safety invariants (§5) and the species output contracts (§4) are testable
against the posh client and its deterministic `PredictHarness`
(`crates/posh/src/remote/predict/test_support.rs`), which drives a predictor
through the real predict → server-echo → `dump_vt` round-trip → cull → render
cycle.

Unit-level conformance (the harness, `cargo test --workspace`):

| Requirement | Vehicle | Description |
|---|---|---|
| §5.1 runtime gate | `PredictHarness` | with `echo_flag`=0 or `alt_screen`=1, no overlay cells are shown regardless of genome output |
| §5.2 lethal fitness | species `rank()` test | a genome echoing under `echo_flag`=0 ranks strictly worst and is never `scored[0]` |
| §4.1 controller clamping | controller test | out-of-range `confirm_gate_ms` is clamped to [0, 5000] |
| §4.2 output cap | from-scratch test | an over-long overlay-op list is truncated, not rejected |
| §7.1 shadow baseline | best-of test | GP output is not displayed while the adaptive shadow ranks better; handover applies hysteresis |
| §8 schema mismatch | persistence test | loading a mismatched `metric_schema_version` cold-starts or migrates, never feeds stale leaves |

End-to-end conformance, when wired, SHOULD live in a `zz-tests_bats/` lane using
`bats-emo` binary injection (`require_bin POSH posh`) to assert no local echo
appears at a real `ECHO`-off prompt.

## Compatibility

- **Model selection is additive.** Two new `POSH_PREDICTION_MODEL` values are
  introduced — `controller` and `scratch` (the from-scratch species); `adaptive`
  remains the default and the existing `adaptive`/`always`/`never`/
  `experimental`/`optimistic` values are unchanged (FDR 0006).
- **mephisto dependency.** posh depends on the mephisto library crate
  (`mephisto::domain`, `mephisto::rng`) at or above the commit that exposes the
  single-generation API of §7. The dependency MUST pin a specific revision; a
  mephisto change to the §7 API signatures is a breaking change requiring a
  coordinated bump.
- **Persisted-genome versioning** is handled by `metric_schema_version` (§8);
  adding metric terminals is a schema-version increment with a migration path or
  a cold-start fallback.
- **Offline training is out of scope.** This RFC specifies the online lane only;
  there is no recorded-corpus training lane and the metric vector is therefore
  never serialized to a recording.
- **Host-shared predictor instance (future).** This RFC specifies a per-client
  predictor. A future revision MAY hoist the population + evolution loop into a
  single host-local instance shared by all posh client sessions on that machine,
  so evolution is amortized once per host and benefits from the diversity of
  every session's outcome stream (and the both-host/app metric terminals let one
  shared population specialize per context via the GP). The seams MUST NOT
  preclude this: the metric source (§3) and the predictor are addressed through
  interfaces that admit a remote/shared implementation, and the per-client
  `adaptive` shadow floor (§7.1) remains in-process so a shared-instance outage
  degrades gracefully to today's default rather than losing local echo. Such an
  instance would persist one population per host (§8), not one per session.

## References

Normative:

- [RFC 2119] Key words for use in RFCs to Indicate Requirement Levels.
- [RFC 0001] Target Grammar and Capability Table (`docs/rfcs/0001-…`) — the
  per-frame capability channel server-forwarded metrics extend.
- [mephisto domain API] `mephisto::domain::{initial_population,
  evaluate_population, step, Scored}` and `mephisto::rng::Rng`.

Informative:

- [FDR 0006] Optimistic local echo (`docs/features/0006-…`) — the gate
  mechanism, the `Predictor` trait, and the eight mosh-inherited constants this
  pilot replaces.
