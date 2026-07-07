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
capability id (11). Its payload is exactly one byte, the low 5 bits of the kitty
keyboard progressive-enhancement flags (disambiguate=1, report-events=2,
report-alternate=4, report-all=8, report-text=16). The capability functions as a
GATE: its PRESENCE means "the client's real terminal speaks the kitty keyboard
protocol"; its *absence* means "unknown / not advertised." The daemon does not
report the payload value back to the app (§3 — the query reply carries the
model's own current flags, not a capability); advertising `0x1f` (full) is the
conventional value.

- A client whose real terminal supports the kitty keyboard protocol (§2.1) MUST
  advertise `CAP_KITTY_KEYBOARD`.
- A client whose real terminal does not support it MUST NOT advertise it.
- On the socket transport (RFC 0008 §1.1), `CAP_KITTY_KEYBOARD` is an Init-only,
  per-connection capability, stable for the connection's lifetime.
- On the roaming transport (RFC 0001 / RFC 0008), it is advertised in the
  client's per-message cap table, exactly as `CAP_SCROLLBACK` and `CAP_MORPH`
  are, so its loss on any single datagram is self-correcting by repetition.

#### 2.1 Client-side capability determination

A client MUST determine its real terminal's kitty keyboard support by a means
local to the client and its real terminal; the determination MUST NOT be proxied
through the session (which is precisely what this specification exists to work
around), MUST NOT steal application input, and MUST NOT emit bytes that violate
the client's output-ordering contract (the alternate-screen takeover MUST remain
the first bytes written).

posh is kitty-focused (see the repository description), so the determination
keys on `$TERM`:

- `$TERM == "xterm-kitty"` ⇒ the outer terminal is kitty, which fully supports
  the protocol. The client MUST advertise `CAP_KITTY_KEYBOARD`. This is a plain
  environment-variable read — no tty I/O, hence no interference — and it is the
  case posh targets.
- Any other `$TERM` ⇒ the client SHOULD read that terminal's local terminfo and
  represent whatever capability it indicates; until that is implemented the
  client MUST NOT advertise the capability, and the daemon answers legacy-only (a
  safe default). Reading other terminals' terminfo is a documented future
  extension.

A naive live `CSI ? u` probe on the shared session tty at loop entry is known to
steal application input and pollute the takeover-output ordering, and MUST NOT be
used. (A live probe placed where it provably does neither remains a permitted
implementation choice for the non-kitty terminfo fallback, but is not required.)

> **Implementation status:** the daemon-side answering (§3, §4), the
> `CAP_KITTY_KEYBOARD` gate (§2), and the local client's `$TERM == "xterm-kitty"`
> advertisement (§2.1) are implemented. The non-kitty terminfo fallback (§2.1)
> and the roaming/remote client's advertisement (§2, roaming bullet) are future
> extensions; until a client advertises, the daemon answers legacy-only,
> unchanged from before this specification.

### 3. Daemon answering of capability-backed queries

Kitty-protocol *detection* is by reply PRESENCE, not value: an application
enables the protocol iff a `CSI ? <flags> u` reply comes back at all (before the
DA reply), then pushes the flags it wants. The reply's `<flags>` value is the
terminal's *current* enabled flags (initially `0`), not a capability. Therefore
`CAP_KITTY_KEYBOARD` is a GATE — "does the real terminal speak kitty?" — and the
daemon MUST NOT substitute a value into the reply. When it answers, it MUST
answer with its own model's current flags (the value `Terminal::take_responses`
already produces, which reflects what the app has pushed and posh-term records).

When the application's output produces query replies in the model
(`Terminal::take_responses` non-empty), the daemon MUST apply the policy from
§3.1:

- Kitty keyboard query `CSI ? u`: answered (reply passed through verbatim) only
  when the gate is open (§3.1). When the gate is closed, the daemon MUST NOT emit
  the kitty reply, so the application concludes the protocol is unsupported and
  does not enable encodings the real terminal cannot deliver.
- Device-attributes (DA1/DA2) and device-status (DSR-CPR) queries: the daemon
  MUST answer from its own terminal model's response whenever it is answering at
  all, independent of the kitty gate. These describe the emulated session
  terminal (DA) or the model's authoritative cursor position (DSR), so the model
  is authoritative.

The reply MUST be written to the application PTY as ordinary PTY input (the
existing `Terminal::take_responses` → PTY-write path), so the application
receives it identically to a real terminal's reply.

#### 3.1 Policy across attached clients

The daemon drives a single application PTY but MAY have multiple clients
attached. It MUST pick one of three policies:

- **Silent** — if ANY attached client is a legacy `Tag::Output` client (RFC 0008
  — did NOT negotiate frame transport), the daemon MUST answer nothing. That
  client forwards the raw query to its real terminal, which answers via
  `Tag::Input`; a daemon answer too would double-reply. (Frame clients attached
  alongside it then do not get the enhancement — acceptable degradation for the
  uncommon mixed case.)
- **Answer** — no clients attached (the model is authoritative), OR every
  attached client is a frame client whose real terminal supports the kitty
  keyboard protocol (each advertised `CAP_KITTY_KEYBOARD`). The daemon writes the
  model's responses verbatim (kitty reply + DA/DSR).
- **Suppress-kitty** — every attached client is a frame client but at least one's
  real terminal does NOT support kitty (did not advertise the cap). The daemon
  answers DA/DSR from the model but strips the kitty reply, so the app concludes
  unsupported and does not encode keys the terminal cannot send.

The gate is presence-based (advertised or not); the advertised flag *value* is
not intersected or reported, because §3 establishes that the reply value is the
model's own flags, not a capability.

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

- For the kitty query (`CSI ? u`), the daemon MUST apply the §3.1 policy
  (Answer / Silent / Suppress-kitty), whether or not clients are attached.
- For DA1/DA2 and DSR-CPR (`CSI 6 n`), the daemon MUST answer from its own model
  whenever it is not in the Silent policy — including under Suppress-kitty, which
  strips only the kitty reply — since these describe the emulated terminal / the
  model's authoritative cursor position.
- For any other model-generated response not enumerated in §1, the daemon MAY
  retain the existing behavior (answer only when no client is attached).

A daemon MUST NOT both answer a query itself (§3) and forward the raw query
bytes to a client for the client's terminal to also answer; doing so would
deliver two replies to the application. (The Silent policy exists precisely to
avoid this when a legacy client is attached.)

### 5. Examples

Client determination on a kitty terminal: `$TERM == "xterm-kitty"`, so the
client advertises `CAP_KITTY_KEYBOARD` (payload `0x1f`) in its Init cap table —
no tty probe.

Application query answered by the daemon (app → daemon → app), one kitty frame
client attached: the app writes `CSI ? u`. The gate is open, so the daemon writes
its model's reply verbatim — `CSI ? 0 u` (`1b 5b 3f 30 75`) initially, since no
flags are enabled yet. The reply's *presence* tells the app the protocol is
supported; the app then pushes the flags it wants (`CSI > 8 u` etc.), posh-term
records them, and a later `CSI ? u` is answered `CSI ? 8 u` from the model.

One frame client whose terminal is NOT kitty (no cap advertised): the app writes
`CSI ? u` `CSI c`. The daemon is in Suppress-kitty, so it writes only the DA
reply (`CSI ? 62 ; 22 c`) — no `CSI ? … u` — and the app concludes the kitty
protocol is unsupported.

A legacy `Tag::Output` client attached: the daemon is Silent; the raw query
reached that client's real terminal, which replies via `Tag::Input`.

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
encodings to *other* clients' terminals that they cannot decode: the gate opens
only when EVERY attached frame client advertises support (§3.1), so one client
can only ever *close* the gate (Suppress-kitty), never open it on another's
behalf.

The client-side determination (§2.1) reads the client's own `$TERM` (or, in the
future, its own terminfo); it does no session-proxied I/O and introduces no new
trust boundary or cross-session information flow.

## Conformance Testing

Conformance tests for this specification are unit tests in
`crates/posh/src/session/daemon.rs` (the query policy and the kitty-reply strip)
and `crates/posh-proto/src/caps.rs` (the `CAP_KITTY_KEYBOARD` payload decode).

### Covered Requirements

| Requirement | Test | Description |
|-------------|------|-------------|
| §3.1 Answer (kitty frame client) | `query_policy_kitty_frame_client_answers` | Every frame client's terminal kitty-capable ⇒ Answer (model reply verbatim) |
| §3.1 Suppress-kitty (non-kitty frame client) | `query_policy_non_kitty_frame_client_suppresses_kitty` | A frame client without the cap ⇒ Suppress-kitty |
| §3.1 gate needs ALL frame clients | `query_policy_all_frame_clients_must_support_kitty` | One non-advertising frame client ⇒ Suppress-kitty; all advertising ⇒ Answer |
| §3.1 Silent (legacy client) | `query_policy_legacy_client_is_silent`, `..._mixed_frame_and_legacy_is_silent` | Any legacy `Tag::Output` client ⇒ Silent |
| §3.1 no clients | `query_policy_no_clients_answers` | No clients ⇒ Answer from the model |
| §3 strip kitty reply, keep DA/DSR | `strip_kitty_reply_removes_only_the_kitty_reply` | Suppress-kitty drops `CSI ? … u` but keeps `…c` / `…R` |
| §2 payload decode + mask | `kitty_keyboard_payload_decodes_and_masks` (posh-proto) | 1-byte payload masked to low 5 bits; malformed ⇒ absent |

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
