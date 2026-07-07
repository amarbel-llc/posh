---
status: proposed
date: 2026-07-07
---

# posh Terminal Query Passthrough and Kitty Keyboard Capability Negotiation

## Abstract

This document specifies how a posh session that serves screen output as rendered
frames (RFC 0008) answers terminal *query* sequences — device attributes, device
status reports, and the kitty keyboard protocol progressive-enhancement query
(`CSI ? u`) — emitted by the application inside the session. Under frame
transport the raw output byte stream never reaches the client's real terminal,
so an app that probes terminal capabilities receives no reply and concludes the
terminal supports nothing; in particular it never enables the kitty keyboard
enhancements that make modified keys such as Shift+Enter distinguishable from
their unmodified forms. This RFC defines a `CAP_KITTY_KEYBOARD` capability that a
client advertises for its real terminal, and the rule that the daemon answers
kitty and device-attribute queries on the application's behalf from the attached
clients' advertised capabilities, so no per-query round trip crosses the lossy
roaming link.

## Introduction

A posh session daemon owns the application PTY and a `posh_term::Terminal`
model. Since RFC 0008, a frame-capable client receives screen state as
`posh-proto` `ServerFrame` records (`Tag::Frame`), not the raw PTY output byte
stream (`Tag::Output`). The daemon's model already parses terminal query
sequences and produces the correct replies (`Terminal::take_responses`), but it
deliberately discards those replies when a client is attached, on the theory
that the client's real terminal will answer and "let the real terminal's
capabilities win" (the historical github #13 behavior).

That theory holds only under `Tag::Output`, where the raw query bytes are
broadcast to the client and reach its terminal. Under `Tag::Frame` the query
bytes are consumed by the daemon's model and never placed on the wire — a query
draws nothing on the screen, so it is absent from every frame. The net effect
under frame transport is: the app's query is consumed, the daemon's own answer
is dropped, the real terminal is never asked, and the app waits, then falls back
to "unsupported."

Empirically (posht `rawkeys`, posh#126): running inside a frame-mode session the
kitty query `CSI ? u` returns no reply, whereas the identical probe under
`POSH_SESSION_FRAMES=0` (raw `Tag::Output`) returns `CSI ? 0 u`. The observable
user symptom is that Shift+Enter is byte-identical to Enter (`0x0d`) — correct
for the kitty *legacy* encoding, but never upgraded to the distinct `CSI 13;2u`
form because the enhancement is never negotiated. The key bytes are not mangled
by posh; the *negotiation* is broken by frame transport.

This specification closes that gap. It reuses the RFC 0001 §3 capability table
and the RFC 0008 socket / roaming cap-negotiation machinery rather than
introducing a new reliable channel: a terminal query is request/response with no
self-healing from screen state, so proxying every query over the lossy roaming
link would demand a durable server→client substream; advertising the capability
once (durably, as a cap) and answering locally at the daemon avoids that
entirely. FDR 0013 already specifies the complementary *outbound* direction
(mirroring the app's pushed kitty flags onto the outer terminal); this RFC
supplies the *inbound* query answer that FDR 0013 assumed.

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Specification

### 1. Scope of "query" sequences

A *terminal query* is an escape sequence the application emits that requires a
reply from the terminal. This specification covers exactly the following query
classes; a conformant daemon MUST recognize these and MUST NOT treat any other
sequence as a query:

- Kitty keyboard progressive-enhancement query: `CSI ? u` (bytes `1b 5b 3f
  75`). The reply form is `CSI ? <flags> u`.
- Primary Device Attributes (DA1): `CSI c` / `CSI 0 c` (`1b 5b 63`). The reply
  is `CSI ? <params> c`.
- Secondary Device Attributes (DA2): `CSI > c` / `CSI > 0 c`. The reply is `CSI
  > <params> c`.
- Device Status Report — cursor position (DSR-CPR): `CSI 6 n`. The reply is `CSI
  <row> ; <col> R`.

DSR-CPR (`CSI 6 n`) is inherently positional and MUST be answered from the
daemon's own terminal model (the model holds the authoritative cursor position),
NOT from client capabilities; it is included here only to state that the daemon
MUST continue to answer it even when a client is attached (§4). The capability
mechanism (§2, §3) applies to the kitty query and the device-attribute queries.

### 2. The `CAP_KITTY_KEYBOARD` capability

A new RFC 0001 §3 capability, `CAP_KITTY_KEYBOARD`, is assigned the next unused
capability id. Its payload is exactly one byte: the kitty keyboard
progressive-enhancement flag set the advertising client's real terminal
supports, as the low 5 bits (`0b00000` … `0b11111`) defined by the kitty
keyboard protocol (disambiguate=1, report-events=2, report-alternate=4,
report-all=8, report-text=16). A payload value of `0` means "the terminal
implements the kitty keyboard protocol but reports no enhancement flags"; the
*absence* of the capability means "unknown / not advertised."

- A client that has determined its real terminal's kitty keyboard support
  (§2.1) MUST advertise `CAP_KITTY_KEYBOARD` with the supported flag byte.
- A client whose real terminal does not implement the kitty keyboard protocol
  MUST NOT advertise `CAP_KITTY_KEYBOARD`.
- On the socket transport (RFC 0008 §1.1), `CAP_KITTY_KEYBOARD` is an Init-only,
  per-connection capability: it is carried in the client's `Tag::Init` cap
  table and is stable for the connection's lifetime.
- On the roaming transport (RFC 0001 / RFC 0008), `CAP_KITTY_KEYBOARD` is
  advertised in the client's per-message cap table, exactly as `CAP_SCROLLBACK`
  and `CAP_MORPH` are, so its loss on any single datagram is self-correcting by
  repetition.

#### 2.1 Client-side capability determination

A client MUST determine its real terminal's kitty keyboard support by querying
the terminal directly, before it hands the terminal to the session render loop:
it writes `CSI ? u` followed by a Primary DA request (`CSI c`) and reads the
reply. If a `CSI ? <flags> u` reply arrives before the DA reply, the terminal
implements the protocol and `<flags>` is the supported flag set; if only the DA
reply arrives, the terminal does not implement it. This probe is local to the
client and its real terminal; it MUST NOT be proxied through the session.

A client MAY cache this determination for the process lifetime. A client that
cannot perform the probe (e.g., no controlling terminal) MUST NOT advertise
`CAP_KITTY_KEYBOARD`.

### 3. Daemon answering of capability-backed queries

When the application emits a kitty keyboard query (`CSI ? u`) or a device-
attributes query (DA1/DA2), and at least one client is attached, the daemon MUST
answer from the *effective* advertised capability (§3.1) rather than discarding
its model's response:

- Kitty keyboard query `CSI ? u`: the daemon MUST reply to the application PTY
  with `CSI ? <effective-flags> u`, where `<effective-flags>` is the effective
  kitty flag set per §3.1. When no attached client advertises
  `CAP_KITTY_KEYBOARD`, the daemon MUST reply with the legacy-only indication
  its own terminal model produces (equivalently, it MUST NOT claim kitty support
  the clients did not advertise).
- Device-attributes queries: the daemon MUST answer from its own terminal
  model's response (the model emulates a fixed, well-known terminal type), as it
  does today when no client is attached. Device attributes describe the emulated
  session terminal, not the outer terminal, so the model is authoritative.

The reply MUST be written to the application PTY as ordinary PTY input (the
existing `Terminal::take_responses` → PTY-write path), so the application
receives it identically to a real terminal's reply.

#### 3.1 Effective capability across multiple clients

A session MAY have multiple clients attached simultaneously, each having
advertised a different `CAP_KITTY_KEYBOARD` value (or none). Because the daemon
drives a single application PTY, it MUST compute a single *effective* kitty flag
set:

- If any attached client has NOT advertised `CAP_KITTY_KEYBOARD`, the effective
  set MUST be empty (the daemon MUST answer as legacy-only). Enabling
  enhancements the app would then encode as `CSI` sequences to a client whose
  terminal cannot decode them would corrupt that client's input.
- If every attached client has advertised `CAP_KITTY_KEYBOARD`, the effective
  flag set MUST be the bitwise AND (intersection) of all advertised flag bytes,
  so the app enables only enhancements every attached terminal supports.
- With no clients attached, the daemon MUST answer from its own model (the
  pre-existing no-client behavior), unchanged.

This conservative-intersection rule guarantees that no attached client receives
key encodings its terminal cannot represent.

#### 3.2 Re-negotiation on attach and detach

The effective set is a function of the currently attached clients. When a client
attaches or detaches, the daemon MUST recompute the effective set for subsequent
queries. The daemon MUST NOT retroactively re-answer a query already answered;
an application that must observe a capability change re-queries. (In practice the
kitty query is issued at application startup and on alternate-screen entry, so a
mid-session attach by a less-capable client affects only queries issued after
that attach.)

### 4. Interaction with the existing discard rule

The historical rule "when clients are attached, the model stays silent and the
real terminal answers" (github #13) is AMENDED by this specification:

- For the capability-backed queries in §1 (kitty, DA1, DA2), the daemon MUST
  answer from the effective capability / its model per §3, whether or not
  clients are attached.
- For DSR-CPR (`CSI 6 n`), the daemon MUST answer from its own model whether or
  not clients are attached (the model holds the authoritative cursor position).
- For any other model-generated response not enumerated in §1, the daemon MAY
  retain the existing behavior (answer only when no client is attached).

A daemon MUST NOT both answer a query itself (§3) and forward the raw query
bytes to a client for the client's terminal to also answer; doing so would
deliver two replies to the application.

### 5. Examples

Client probe of a kitty-capable outer terminal (client → real terminal, then
reply): the client writes `1b 5b 3f 75  1b 5b 63` (`CSI ? u` `CSI c`) and reads
`1b 5b 3f 31 75  1b 5b 3f 36 32 3b 63` (`CSI ? 1 u` then the DA reply),
determining flags = `1`. The client advertises `CAP_KITTY_KEYBOARD` with payload
byte `0x01`.

Application query answered by the daemon (app → daemon → app): the app writes
`CSI ? u`. A single client is attached advertising flags `0x01`. The daemon
writes `CSI ? 1 u` (`1b 5b 3f 31 75`) to the application PTY.

Two clients, flags `0x1f` and `0x01`: the effective set is `0x1f AND 0x01 =
0x01`; the daemon answers `CSI ? 1 u`.

Two clients, one advertising `0x1f` and one not advertising the capability: the
effective set is empty; the daemon answers legacy-only (`CSI ? 0 u`).

### 6. Relationship to FDR 0013

FDR 0013 specifies mirroring the *application's* pushed kitty flags (`CSI >
flags u` / `CSI = flags ; mode u`) outward onto each client's real terminal, so
the terminal encodes keys as the app requested. That outbound mirror is only
useful once the app has *enabled* the enhancements, which requires the app to
first believe the terminal supports them — the inbound query answer this RFC
specifies. The two are complementary: this RFC lets the app learn support and
enable enhancements; FDR 0013 carries the enabled state to the real terminal.

## Security Considerations

Terminal query replies are low-sensitivity: device attributes and kitty flags
describe terminal capabilities, not user data. The daemon answers only the
enumerated query classes (§1) and MUST NOT echo arbitrary application output
back to the PTY as a "reply," which could otherwise be a reflection primitive.

The `CAP_KITTY_KEYBOARD` payload is a single byte constrained to the low 5 bits;
a daemon MUST mask the received payload to those bits and MUST treat a malformed
or oversized payload as "capability absent" rather than trusting an
out-of-range value. A hostile client cannot induce the daemon to emit key
encodings to *other* clients' terminals that they cannot decode, because the
effective set is the intersection (§3.1): a client can only ever *reduce* the
effective capability, never raise it above what another client advertised.

The client-side probe (§2.1) writes to and reads from the client's own real
terminal only; it introduces no new trust boundary and no cross-session
information flow.

## Conformance Testing

Conformance tests for this specification live in
`crates/posh/zz-tests_bats/` (the session-daemon bats suite), with the
capability-determination and effective-set logic additionally unit-tested in
`crates/posh/src/session/daemon.rs` and the posh-term query recognition in
`crates/posh-term`.

Tests use binary injection via `bats-emo`:

    require_bin POSH posh

### Covered Requirements

| Requirement | Test File | Description |
|-------------|-----------|-------------|
| §3 kitty query answered from cap | `daemon.rs` unit | A frame client advertising `CAP_KITTY_KEYBOARD` ⇒ the daemon writes `CSI ? <flags> u` to the PTY on `CSI ? u` |
| §3 no cap ⇒ legacy answer | `daemon.rs` unit | No advertised cap ⇒ the daemon answers legacy-only, never claiming kitty support |
| §3.1 conservative intersection | `daemon.rs` unit | Two clients, flags AND; any non-advertising client ⇒ empty effective set |
| §4 DSR-CPR still answered | `daemon.rs` unit | `CSI 6 n` answered from the model with a client attached |
| §1 query recognition | `posh-term` unit | Only the enumerated sequences are recognized as queries |

## Compatibility

This specification is additive and degrades cleanly:

- A client that does not advertise `CAP_KITTY_KEYBOARD` gets today's behavior:
  the daemon answers legacy-only, and Shift+Enter (and other modified keys) stay
  in their legacy encoding. No regression relative to the current frame-mode
  behavior.
- A baseline (pre-this-RFC) daemon ignores an unknown `CAP_KITTY_KEYBOARD`
  capability in the Init table (RFC 0001 §3 requires unknown caps to be ignored)
  and continues to drop query replies under frame transport. A client MUST NOT
  assume the capability took effect.
- `POSH_SESSION_FRAMES=0` (RFC 0008 §6) remains a full escape hatch: under raw
  `Tag::Output` the query reaches the real terminal directly and this
  specification's daemon-answer path is not exercised.
- The nested case (a roaming client attached to a session that is itself a
  posh session on the remote host) requires the capability to propagate across
  both frame layers; the effective-set rule composes (each layer intersects),
  but end-to-end nested propagation is out of scope for this revision and
  tracked separately.

## References

### Normative

- [RFC 2119] Key words for use in RFCs to Indicate Requirement Levels.
- [RFC 0001] posh Target Grammar and Capability Table — the §3 capability table
  format `CAP_KITTY_KEYBOARD` uses.
- [RFC 0008] posh Unified Session Frame Transport — the `Tag::Frame` transport
  and socket / roaming capability negotiation this RFC extends.

### Informative

- [FDR 0013] Kitty keyboard protocol pass-through — the complementary outbound
  flag mirror.
- [kitty keyboard protocol] The progressive-enhancement flag semantics and query
  form (`CSI ? u` / `CSI = flags ; mode u`).
- posh#126 — the Shift+Enter investigation and the `rawkeys` empirical capture
  that motivated this specification.
