---
status: proposed
date: 2026-06-13
---

# posh-rec `.castx` Terminal Recording Format

## Abstract

This document specifies `.castx`, the terminal-recording file format produced
and consumed by posh-rec. `.castx` is a strict superset of the asciinema
`.cast` v2 format: line-delimited JSON with a header object followed by one
event array per line. It adds two backward-compatible extensions that stock
asciinema players ignore — a named step-marker event (`m`) and a `posh_rec`
header block carrying a format version and the emulator revision a recording
was produced against. The contract guarantees interoperability in both
directions: any conforming `.cast` v2 recording replays through posh-rec, and
any `.castx` plays in asciinema. Timing is recorded but never replayed as a
delay — a `.castx` replays to a screen that is a pure function of its bytes,
which is what makes it usable as a deterministic test oracle.

## Introduction

posh-rec records a terminal's output byte stream once and replays it through
the in-process `posh_term` emulator, so a recorded program or session can be
re-rendered and asserted on deterministically — with no live terminal and no
timing to race (the `tmux capture-pane` + `sleep` pattern that motivates the
tool). For that to be a *shared* capability — usable by posh, by the Go `crap`
tool, and by any other terminal project — the recording must be a documented
file-format contract rather than an implementation detail, because each
ecosystem owns its own *producer* while sharing one *replayer*. This RFC is
that contract.

The format deliberately builds on asciinema `.cast` v2, the de facto
interchange format for terminal recordings, so that `.castx` files remain
playable by the existing asciinema toolchain and so that the large body of
existing `.cast` recordings can be replayed and asserted without conversion.
The extensions this RFC adds are confined to mechanisms asciinema's own format
rules already require players to tolerate (unknown event codes and unknown
header keys), so the superset relationship holds without a version flag day.

This RFC specifies (1) the **document structure** — header and event lines;
(2) the **standard events** inherited from `.cast` v2; (3) the **posh-rec
extensions** — the `m` marker event and the `posh_rec` header block; (4) the
**interoperability rules** that make the superset relationship normative in
both directions; and (5) **replay and golden semantics** — how a consumer
turns a `.castx` into a screen, and the stability rules for golden frames
anchored on it. It does not specify the posh datagram protocol (RFC 0001), the
posh-term emulator's rendering behavior, or the asciinema `.cast` v2 format
itself, which it references normatively.

The reference implementation is the `crates/posh-rec` workspace crate: the
format reader/writer and streaming `Recorder` (`castx.rs`), the step-ratchet
replayer (`player.rs`), and the golden-frame renderer (`golden.rs`). The
`posh-rec` binary's `record`, `replay`, `step`, `bless`, and `assert`
subcommands, and `posh --record`, all produce or consume this format.

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Specification

### 1. Document structure

A `.castx` document is a sequence of lines separated by a single line feed
(`U+000A`). Each line is an independent, complete JSON value. The first
non-empty line is the **header** (a JSON object); every subsequent non-empty
line is an **event** (a JSON array).

- A conforming document MUST encode the header and each event as a single
  line of JSON with no interior unescaped line feed. Any line feed within a
  string value MUST be escaped (`\n`), so that splitting the document on raw
  `U+000A` yields exactly one logical record per line. (This is what lets a
  reader parse line-by-line without a streaming JSON parser.)
- The entire document MUST be UTF-8.
- A reader MUST ignore empty or whitespace-only lines, including a trailing
  newline at end of file.
- A reader MUST treat the first non-empty line as the header and MUST NOT
  interpret it as an event.

### 2. Header

The header is a JSON object. It carries the asciinema v2 fields and MAY carry
the `posh_rec` extension block (section 4.2).

```json
{"version":2,"width":80,"height":24,"env":{"TERM":"xterm-256color"},"posh_rec":{"v":1,"emu_rev":"0.1.0"}}
```

- `version` (number): the asciinema format version. A producer MUST emit `2`.
  A reader MUST reject a document whose `version` is present and not `2`. A
  reader MAY accept a header that omits `version`, treating it as `2`
  (defensive; the v2 spec requires the field).
- `width` (number) and `height` (number): the terminal's initial size in
  columns and rows. A producer MUST emit both. A reader that finds either
  absent MUST substitute a conventional default of 80 columns by 24 rows.
- `env`, `timestamp`, `title`, `idle_time_limit`, `theme` (asciinema v2
  optional fields): a producer MAY emit them; a posh-rec reader MUST tolerate
  and MAY ignore them. They carry no replay semantics in this specification.
- `posh_rec` (object): the posh-rec extension block, section 4.2.

### 3. Standard events

Each event is a JSON array of the form `[time, code, data]`:

- `time` (number): seconds, as a floating-point value, since the start of the
  recording. `time` is monotonically non-decreasing across the event stream;
  a producer SHOULD emit events in non-decreasing `time` order. `time` is
  recorded for playback tooling and frame bucketing but, per section 5.1, MUST
  NOT be replayed as a delay by a posh-rec consumer.
- `code` (string): the event type. This specification assigns `o`, `i`, `r`
  (this section) and `m` (section 4.1).
- `data` (string): the event payload, interpreted per `code`.

The standard `.cast` v2 event codes:

| `code` | Name | `data` |
|---|---|---|
| `o` | output | terminal output bytes (UTF-8; see section 5.2) |
| `i` | input | input bytes sent to the terminal (UTF-8) |
| `r` | resize | the new terminal size as `"COLSxROWS"`, e.g. `"80x24"` |

- An `o` event's `data`, decoded from JSON and interpreted as UTF-8 bytes,
  MUST be fed to the emulator in order. Example (ESC shown as its JSON
  escape): `[1.5,"o","hello \u001b[31mred\u001b[0m"]`.
- An `r` event's `data` MUST match `^[0-9]+x[0-9]+$`, the new column count and
  row count separated by a literal `x` (columns first — the asciinema
  convention). A consumer MUST resize the emulator to those dimensions at that
  point in the stream. Example: `[2.0,"r","100x40"]` sets 100 columns, 40
  rows.
- An `i` event records input. A pure replay consumer (one reconstructing the
  screen) MUST NOT feed `i` data to the emulator, because the resulting output
  is already present as `o` events; `i` is retained for fidelity and for
  asciinema playback.

### 4. posh-rec extensions

The extensions in this section are the only differences between `.castx` and
`.cast` v2. Both are designed to be invisible to a conforming asciinema player
(section 5.3).

#### 4.1 The `m` (marker) event

```json
[2.5,"m","prompt-ready"]
```

- An `m` event's `data` is a producer-assigned **marker name**: a stable,
  human-meaningful label for the byte position at which the marker appears in
  the output stream.
- A marker denotes a position *between* output bytes; it has no effect on the
  emulated screen. A replay consumer MUST NOT feed an `m` event's `data` to
  the emulator.
- Marker names need not be unique within a document. A consumer resolving a
  marker by name (e.g. "step to marker `K`") MUST resolve to the first marker
  with that name at or after the current position.
- Markers are the stable anchor for golden frames (section 5.4).

#### 4.2 The `posh_rec` header block

```json
"posh_rec":{"v":1,"emu_rev":"0.1.0"}
```

- `v` (number): the posh-rec extension version. This specification defines
  `v: 1`. A producer conforming to this specification MUST emit `"v":1`. A
  reader that does not understand a given `v` MUST still replay the standard
  events (the `posh_rec` block never changes how `o`/`i`/`r` are interpreted);
  it MAY ignore extension semantics it does not understand.
- `emu_rev` (string): the emulator revision the recording was produced
  against — `posh_term`'s `version+git-sha` (the version flowed from the
  repo's `version.env`, joined with the build's git rev; see eng-versioning(7)
  and `posh_term::emu_rev()`). Earlier producers emitted the bare version with
  no `+git-sha` suffix; readers MUST treat `emu_rev` as an opaque string (it is
  compared, never parsed — see section 5.4). Its semantics are advisory.
- A `.castx` produced by posh-rec MUST include the `posh_rec` block. A reader
  MUST treat its absence as "this is a plain `.cast`" and replay normally
  (section 5.3).

### 5. Replay and golden semantics

#### 5.1 Timing is never a delay

A posh-rec replay consumer MUST NOT sleep, block, or otherwise delay between
events on account of their `time` values. The emulated screen after consuming
a prefix of the event stream is a pure function of the `o`/`r` events in that
prefix. This determinism is the property the format exists to provide; a
consumer that honored `time` as a delay would reintroduce exactly the timing
nondeterminism the format removes.

#### 5.2 Output encoding and UTF-8 reassembly

- `o` and `i` `data` are UTF-8 text with control and non-printable bytes
  JSON-escaped (e.g. ESC as `\u001b`, line feed as `\n`). A reader recovers
  the original bytes by JSON-decoding the string and taking its UTF-8 bytes.
- A producer MUST emit each `o` event's `data` as valid UTF-8. Because a
  terminal output stream is read in fixed-size chunks, a multi-byte UTF-8
  scalar can straddle a chunk boundary; a producer MUST reassemble such a
  split scalar across chunk reads so that no `o` event ends mid-scalar (the
  reassemble-across-reads convention). A byte sequence that is not valid UTF-8
  cannot be represented losslessly and is out of scope; a producer MAY
  substitute `U+FFFD` for an unrepresentable byte.
- A reader MUST accept any valid UTF-8 `o` data, including multi-byte scalars
  encoded directly or via `\\uXXXX` escapes (including surrogate pairs).

#### 5.3 Interoperability (the superset contract)

- A `.castx` document MUST be a valid asciinema `.cast` v2 document under
  asciinema's own tolerance rules: its header carries the required v2 fields,
  its events use numeric `time` and string `code`/`data`, and its extensions
  (`m` events, the `posh_rec` header key) are an extra event code and an extra
  header key respectively — both of which a conforming asciinema player
  ignores. Therefore any `.castx` MUST play in asciinema.
- A posh-rec reader MUST accept any conforming `.cast` v2 document. It MUST
  ignore events whose `code` it does not recognize (forward/foreign
  extensions) rather than erroring, and MUST treat a missing `posh_rec` block
  as a plain recording. Therefore any `.cast` v2 MUST replay through posh-rec.
- There is no version flag day: the extension version `v` (section 4.2)
  distinguishes posh-rec revisions, and unknown event codes degrade to
  "ignored" on both sides.

#### 5.4 Golden-frame stability and `emu_rev`

A golden frame is a snapshot of the emulated screen at a chosen position,
written by `posh-rec bless` and checked by `posh-rec assert`.

- A golden MUST be anchored on a **named marker** (section 4.1) or a byte
  offset — a producer-controlled, stream-stable position. A golden MUST NOT be
  keyed on an absolute visible-change counter (`posh_term`'s `generation()`)
  or any other emulator-internal count, because such counts MAY shift if a
  future emulator coalesces changes differently while rendering the identical
  final screen.
- `emu_rev` is advisory. A consumer comparing a recording against a golden MAY
  compare the recording's `emu_rev` to the revision under which the golden was
  blessed; on a difference it SHOULD warn that the golden may need
  regeneration, but it MUST NOT fail solely on an `emu_rev` difference. This
  makes regeneration deliberate and auditable rather than silent.

### 6. Complete example

A minimal `.castx` recording 80x24, clearing the screen, printing bold red
"hi", marking the position, then resizing (ESC bytes shown as their JSON
`\u001b` escape):

```
{"version":2,"width":80,"height":24,"posh_rec":{"v":1,"emu_rev":"0.1.0"}}
[0.000000,"o","\u001b[H\u001b[J\u001b[1;31mhi\u001b[0m"]
[0.010000,"m","greeting-shown"]
[0.500000,"r","100x40"]
```

## Security Considerations

- A `.castx` is passive data: replaying it feeds recorded bytes to an
  in-process terminal emulator and inspects the resulting screen. It conveys
  no capability to the consumer beyond what those bytes render. A posh-rec
  replay consumer MUST NOT execute, fetch, or otherwise act on `.castx`
  content other than feeding `o`/`r` events to the emulator and resolving
  markers.
- A reader MUST bounds-check untrusted input: a malformed header, a
  non-numeric `time`, a malformed `r` payload, or a truncated line MUST cause
  that record to be rejected (or the document to fail to parse) rather than an
  over-read, panic, or unbounded allocation. `r` dimensions are
  attacker-influenced and MUST be clamped to the emulator's supported size
  range.
- The emulator that consumes `o` data is itself the trust boundary for escape
  sequence handling; this format does not expand it. A `.castx` cannot cause
  the emulator to do anything a live byte stream could not, because `o` data
  is exactly such a stream.
- `.castx` files may capture sensitive terminal output (the recorded session's
  contents). Treat a recording with the same confidentiality as the session it
  captured; this specification does not encrypt or redact recordings.

## Conformance Testing

Conformance tests for this specification live in the `crates/posh-rec` cargo
suite (the normative home until a cross-implementation `zz-tests_bats/` CLI
suite exists, consistent with RFC 0001's Conformance Testing). The unit tests
in `castx.rs`, `player.rs`, and `golden.rs`, and the integration tests under
`crates/posh-rec/tests/`, exercise the requirements below; they drive the
`posh-rec` binary via Cargo's `CARGO_BIN_EXE_posh-rec` for CLI-level
requirements.

A future cross-implementation suite SHOULD use `bats-emo` binary injection
(`require_bin POSH_REC posh-rec`) so a non-Rust producer/consumer can run the
same tests.

### Covered Requirements

| Requirement | Test | Description |
|---|---|---|
| §1, line-based parsing; escaped interior newline | `castx::tests` (write/read round-trip) | An `o` payload containing a newline stays one line and round-trips. |
| §2, header fields + width/height default | `castx::tests` (header read) | Headers with and without `posh_rec`; defaults applied when size absent. |
| §3, `o`/`r` interpretation; `r` is COLSxROWS | `player::tests`, `cli::tests` | Output renders; a `"40x10"` resize sets 40 columns and 10 rows. |
| §3, `i` not fed on replay | `tests/replay.rs` | A plain `.cast` with an `i` event replays without the input reaching the screen. |
| §4.1, marker resolves to the next at/after position | `player::tests`, `tests/step.rs` | `step_to_marker` lands exactly at the named marker. |
| §5.2, UTF-8 reassembly across writes | `castx::tests` (Recorder) | A scalar split across two writes yields one valid `o` event. |
| §5.3, plain `.cast` replays; unknown codes ignored | `tests/replay.rs` | A plain `.cast` and a recording with an unknown event code replay. |
| §5.4, golden round-trip; regression fails | `tests/golden_cli.rs`, `tests/emulation.rs` | `bless`→`assert` passes; a mutated golden fails with a diff. |

## Compatibility

- **No flag day.** `.castx` is gated by no version negotiation. A reader that
  predates an extension ignores it (unknown event code, unknown header key);
  the `posh_rec.v` field distinguishes extension revisions when semantics
  matter. All four skew combinations (asciinema/posh-rec × producer/consumer)
  degrade to standard `.cast` v2 behavior, never to corruption.
- **Extension versioning.** Future posh-rec extensions MUST either be a new
  ignorable event code / header key (no `v` bump) or, when they change the
  meaning of existing fields, a `posh_rec.v` increment with this RFC
  superseded or amended. New event codes MUST remain single characters that a
  conforming asciinema player ignores, preserving the superset contract.
- **asciinema upstream.** Should asciinema define a `.cast` v3 or assign a new
  event code that collides with `m`, this RFC MUST be revised to preserve the
  superset relationship (e.g. by re-mapping the marker code); the `posh_rec.v`
  field is the signal for such a revision.

## References

### Normative

- [RFC 2119] Key words for use in RFCs to Indicate Requirement Levels.
- [asciicast v2] asciinema `asciicast` file format, version 2 — the base
  format `.castx` extends (line-delimited JSON header + `[time, code, data]`
  events; codes `o`/`i`/`r`).

### Informative

- RFC 0001: posh Target Grammar and Datagram Capability Table
  (`docs/rfcs/0001-target-grammar-and-capability-table.md`) — the Conformance
  Testing convention (cargo suite as normative home until a `zz-tests_bats`
  CLI suite exists) followed here.
- posh-rec epic (github #56) and phase issues #57–#61 — the recorder/replayer
  this format serves; #61 (adoption) records a real mosh emulation byte stream
  as a `.castx` and asserts it deterministically.
- `crates/posh-rec` — reference implementation: `castx.rs` (reader, writer,
  streaming `Recorder`), `player.rs` (step-ratchet replayer + markers),
  `golden.rs` (grid/vt golden render), and the `posh-rec` binary subcommands
  `record`/`replay`/`step`/`bless`/`assert` plus `posh --record`.
