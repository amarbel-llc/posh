---
status: proposed
date: 2026-08-25
---

# Client Introspection Capabilities (`CAP_CLIENT_IDENT`, `CAP_CLIENT_STATE`) and the Session Status Socket

## Abstract

This document specifies the reverse half of posh introspection: two capability
entries by which a posh client reports its own identity and its live local-echo
and transport state to the server it is attached to, a per-session status
socket through which any process — including a shell *inside* the session —
reads that state, and the propagation rule that makes an enclosing session's
client state visible from a session nested within it. It also fixes the
structural rule that every introspectable axis is declared once, in a shared
struct, from which every reporting surface renders, so that no future axis can
be visible from one end of a connection only.

## Introduction

RFC 0013 made the far end of a connection legible to a *client*: build,
transport state, and the session's activity label all flow server → client.
Nothing flows the other way. The client's echo prediction model, whether the
FDR 0006 slow-link escalation is governing or a pin is in effect, its measured
SRTT, its outcome counters, and its build all live only in the client process,
readable only from that process's own palette. A server, a session daemon, and
— the motivating case — a shell running inside the session have no way to
answer "which echo model is the terminal I am being viewed through actually in?"
The nested case is worse: a local `posh attach` inside a roaming session puts a
second daemon between the shell and the outer client, and the outer client's
state is two hops away with no path at all.

The inventory that motivated this record (2026-08-25) found that every entry in
RFC 0001's capability table is either "what I can decode" or "what I want from
you"; none is "here is my state". RFC 0013 §1–2 are request-gated because the
*client* is the consumer and can ask. Here the consumer is the server side,
which cannot ask a client that has not offered — so the client entries in this
document are sent unconditionally.

Scope: the two capability entries and their payloads, the retention and
exposure rules on the server and daemon, the session status socket and its line
format, upstream propagation across nested sessions, and the shared-struct
conformance rule. Out of scope: any change to frame bodies; the session-layer
collapse (FDR 0012), which is expected to subsume §5 by removing the nesting it
propagates through.

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

"Attached to" a message means present in that message's RFC 0001 capability
table. The "serving side" of a connection is whichever process decodes the
client's messages: a roaming `posh-server`, the RFC 0008 §3 relay, an RFC 0011
mux endpoint's per-channel bridge, or the session daemon on a local attach.

## Specification

### 1. `CAP_CLIENT_IDENT` (id 16)

#### 1.1 Payload

The client entry's payload is byte-identical in layout to `CAP_SERVER_IDENT`
(RFC 0013 §1.1): format version `0x01`, pid `u32` LE, process start time unix
ms `u64` LE, then length-prefixed version and git-sha strings (each ≤ 80
bytes). A decoder MUST reject an unknown format version, a truncated field, or
trailing bytes, and MUST keep its previously held identity (or none) on
rejection. The server entry is not defined; a server MUST NOT send id 16.

#### 1.2 Semantics

- A client MUST attach its identity to the first message of a connection
  (`Tag::Init` on a local attach; the first `ClientMessage` on a roaming
  connection) and to the first message after any resync or reconnect (the
  serving process behind an address may have been replaced).
- A client SHOULD additionally attach it at a slow cadence (RECOMMENDED every
  30 s) so a serving side that missed the first message on a lossy link still
  converges; it MUST NOT attach it to every message.
- The identity is per originating process. On a relayed or bridged connection
  the relay MUST forward the entry unchanged (§3); it MUST NOT substitute its
  own identity, which the daemon already learns from the relay's own
  `CLIENT_IDENT`.

### 2. `CAP_CLIENT_STATE` (id 17)

#### 2.1 Payload

| offset | size | field |
|---|---|---|
| 0 | 1 | format version, `0x01` |
| 1 | 1 | `echo_model` (§2.2) |
| 2 | 1 | `echo_control` bit set (§2.3) |
| 3 | 1 | `gates` bit set (§2.4) |
| 4 | 1 | `codec`: `0` unknown, `1` DumpDiff, `2` Morph |
| 5 | 4 | `srtt_ms`, `u32` LE; `0xFFFFFFFF` = no measured sample yet |
| 9 | 4 | `rto_ms`, `u32` LE |
| 13 | 2 | `escalate_srtt_ms`, `u16` LE |
| 15 | 2 | `escalate_hold_ms`, `u16` LE |
| 17 | 2 | `deescalate_srtt_ms`, `u16` LE |
| 19 | 2 | `deescalate_hold_ms`, `u16` LE |
| 21 | 4 | `predict_correct`, `u32` LE |
| 25 | 4 | `predict_nocredit`, `u32` LE |
| 29 | 4 | `predict_incorrect`, `u32` LE |
| 33 | 4 | `mispredict_resets`, `u32` LE |

Total length 37. A decoder MUST reject an unknown format version or a payload
shorter than 37 bytes and MUST keep its previously held state on rejection. A
decoder MUST accept and ignore trailing bytes: a later format version MAY
append fields while keeping this prefix, and a peer that knows only `0x01`
reads the prefix. (This is the `CAP_METRICS` v1→v2 convention, RFC 0007 §3,
chosen over RFC 0013's strict-length rule precisely because this payload is
expected to grow with every new axis, §6.) A format version whose prefix is
*not* this layout MUST use a new version byte.

Counters are cumulative for the life of the client's current predictor and
reset to zero when the predictor is rebuilt (an echo-model switch, FDR 0006).
Thresholds report the values the sender's escalation machine is using, so a
reader never has to guess which build's constants apply.

#### 2.2 `echo_model`

| value | model |
|---|---|
| 0 | unknown / no predictor (a local-attach client today) |
| 1 | `adaptive` |
| 2 | `always` |
| 3 | `never` |
| 4 | `experimental` |
| 5 | `optimistic` |
| 6 | `controller` (RFC 0007) |
| 7 | `scratch` (RFC 0007) |

This is the model **in effect**, after any escalation switch: an escalated
session reports `5` with `escalated` set in §2.3, not `1`. New models take the
next free value; values MUST NOT be reused.

#### 2.3 `echo_control`

| bit | meaning |
|---|---|
| 0 | `governing`: the escalation machine decides the model (default model, gate on) |
| 1 | `escalated`: the auto switch to `optimistic` is currently applied |
| 2 | `gate_off`: `POSH_ECHO_ESCALATE` is off |
| 3 | `pinned_env`: the model was pinned by `POSH_PREDICTION_MODEL`/`POSH_PREDICTION` |
| 4 | `pinned_palette`: the model was pinned by a palette `Echo:` command |
| 5–7 | reserved, MUST be zero |

`governing` and any `pinned_*` bit are mutually exclusive; a decoder MUST treat
a payload with both set as malformed. `escalated` MUST NOT be set unless
`governing` is set.

#### 2.4 `gates`

| bit | meaning |
|---|---|
| 0 | `echo_on`: the remote PTY's `ECHO` bit as the client last read it from `FLAG_ECHO` |
| 1 | `alt_screen`: the client's reconstructed server terminal is on the alternate screen |
| 2 | `predict_active`: the predictor currently has showable predictions |
| 3–7 | reserved, MUST be zero |

These are the client's *view* of the FDR 0006 gates. A serving side that knows
the true PTY state MAY compare them to detect a gate that never reached the
client (the 2026-08-23 `flags: 0` incident, FDR 0006).

#### 2.5 Semantics

- A client MUST attach `CLIENT_STATE` **unconditionally** — it MUST NOT wait
  for a server request — on: the first message of a connection and after any
  resync/reconnect (with `CLIENT_IDENT`); the next message after any change to
  bytes 1–20 (model, control, gates, codec, thresholds); and otherwise at a
  heartbeat cadence of RECOMMENDED 5 s.
- A change to the counters alone (bytes 21–36) MUST NOT by itself trigger a
  send; counters ride the next change- or heartbeat-driven entry. A client MUST
  NOT attach `CLIENT_STATE` more than once per second.
- A client with no predictor (today's local-attach client) MUST still send the
  entry, with `echo_model` = 0, `srtt_ms` = `0xFFFFFFFF`, and counters zero.
  Absence means "old client", not "no echo"; a reader MUST distinguish the two.
- A server MUST NOT send id 17.

### 3. Retention and forwarding on the serving side

- The serving side MUST retain the most recently decoded `CLIENT_IDENT` and
  `CLIENT_STATE` **per attached client** for as long as that client is attached,
  and MUST discard them when the client detaches, times out, or is replaced on
  resync.
- A relay (RFC 0008 §3) and a mux per-channel bridge (RFC 0011) MUST forward
  both entries from the roaming client to the daemon in their own
  `Tag::Init`/subsequent client messages, unchanged and under the same ids, so
  the daemon retains the *originating* client's state. The relay is itself a
  daemon client and additionally reports its own `CLIENT_IDENT`; the daemon
  MUST keep both, keyed by originating pid, and present the relay as the
  attachment and the roaming client as its origin.
- A serving side MUST NOT use any `CLIENT_STATE` field for behavior. It is
  display and diagnostic data; in particular a server MUST NOT gate `FLAG_ECHO`
  emission, frame pacing, or codec choice on it.

### 4. The session status socket and `posh status`

#### 4.1 Socket

A session daemon MUST bind `<base>/<group>/<session>.status.sock` beside the
session socket, following RFC 0013 §4's contract exactly: a liveness record
written before the bind so existing GC reaps crash leftovers, one UTF-8
response per accepted connection then close, never read, non-fatal bind
failure. Readers treat connect-timeout or empty output as `stale`.

A roaming `posh-server` that owns its PTY directly (Architecture A) has no
session dir; it MUST bind the same socket under
`<base>/remote/<pid>.status.sock` with the same liveness record, response, and
GC rules, and MUST additionally emit the response in its `SIGUSR2` diag dump
(FDR 0007). `posh status` and `posh ls` MUST read the `remote/` sockets
alongside the session dirs. The two roles differ only in the path; a reader
MUST NOT be able to tell from the response which role answered.

#### 4.2 Response format

The response is one or more lines. Line 1 is the session line; each further
line describes one attached client. Fields are space-separated `key=value`
pairs; values containing spaces or `=` MUST be double-quoted with `\"` and `\\`
escapes. Unknown keys MUST be ignored. Keys MUST appear in the order below; a
writer MAY omit a key whose value is unknown.

Session line:

    session=<name> group=<group> daemon=<version>(<sha>) pid=<u32> \
      frames=<on|off> echo_flag=<0|1> alt_screen=<0|1> clients=<n> \
      activity="<label>"

Client line (one per retained client, `via=` present only for a relayed origin):

    client pid=<u32> build=<version>(<sha>) [via=relay pid=<u32>] \
      echo=<model> control=<auto|auto-escalated|pinned-env|pinned-palette|gate-off> \
      srtt=<ms|none> rto=<ms> codec=<dumpdiff|morph|unknown> \
      gates=echo:<0|1>,alt:<0|1>,active:<0|1> \
      thresholds=<esc_srtt>/<esc_hold>/<deesc_srtt>/<deesc_hold> \
      predict=<correct>/<nocredit>/<incorrect> resets=<n> age=<ms> \
      [upstream=<n>]

`age` is milliseconds since the `CLIENT_STATE` entry was decoded. `echo` renders
the §2.2 model name, with value 0 rendered as `none` (a client that reported it
has no predictor). A client
that has sent `CLIENT_IDENT` but no `CLIENT_STATE` renders `echo=unknown`; a
client that has sent neither renders `build=unknown echo=unknown` — the
old-client verdict. The rendered `control` value is derived from the §2.3 bits:
`gate_off` → `gate-off`; else `pinned_env`/`pinned_palette` → the matching
name; else `escalated` → `auto-escalated`; else `auto`.

#### 4.3 `posh status`

`posh status [host:][session]` reads and prints the response. With no target it
MUST resolve `$POSH_SESSION`/`$POSH_GROUP` and read the enclosing session's
socket — this is the in-session read that motivates this document — and MUST
fail with a one-line error naming the missing variable when not inside a
session. `posh ls` SHOULD append, per session, the `echo`, `control`, `srtt`
and `build` fields of each attached client. The palette's *About / transport
info* and *Show echo prediction stats* MUST render the same fields from the same
source struct (§6); the client's own `SIGUSR2` dump MUST include `echo_model`,
`echo_control`, the thresholds, and its build — the fields the FDR 0007 dump
omitted.

### 5. Upstream propagation across nested sessions

When a local `posh attach` (or `posh start`) runs with `$POSH_SESSION` set —
i.e. inside an enclosing session — the attaching client MUST read the enclosing
session's status socket (§4.1) and attach its **client lines** to its own
`Tag::Init` as one `CAP_CLIENT_UPSTREAM` entry (id 18), so the inner daemon can
show what the enclosing session is viewed through.

#### 5.1 `CAP_CLIENT_UPSTREAM` (id 18) payload

| offset | size | field |
|---|---|---|
| 0 | 1 | format version, `0x01` |
| 1 | 1 | depth `d` (1 = the immediately enclosing session) |
| 2 | 2 | text length `tl`, `u16` LE |
| 4 | tl | the enclosing session's §4.2 client lines, UTF-8, `\n`-separated |

The text MUST be the verbatim §4.2 client lines of the enclosing session,
including any `upstream=` lines they already carry (so depth-`n` nesting
propagates transitively without the daemon understanding nesting). A client
MUST cap `tl` at 4096 bytes, truncating whole lines from the end and appending
`upstream=truncated`. A daemon MUST retain the entry with the client that sent
it and render it after that client's line, indented by two spaces, in the §4.2
response; `upstream=<n>` on the client line counts the retained lines.

If the enclosing socket is stale or absent the client MUST send the entry with
`tl` = 0 and the daemon renders `upstream=stale` — distinguishing "the outer
daemon predates this document" from "not nested".

#### 5.2 Relationship to FDR 0012

This section exists because nesting exists. FDR 0012 (session layer collapse)
retargets the outer transport at the inner daemon instead of nesting, at which
point the inner daemon holds the roaming client's `CLIENT_STATE` directly via §3
and no `UPSTREAM` entry is needed. When FDR 0012 reaches `experimental`, this
section SHOULD be revisited; id 18 MUST remain valid for the intentional-nesting
escape hatch FDR 0012 preserves.

### 6. The double-end visibility rule

Every introspectable axis MUST be declared exactly once, as a field on one of
two structs in `posh-proto`: `ClientIntrospection` (the decoded form of
§1–§2, plus the client's own view of it) and `ServerIntrospection` (RFC 0013
§1–§2 and §5). All of the following MUST render from those structs and MUST
NOT hand-assemble their own field lists:

- the `CLIENT_IDENT`/`CLIENT_STATE` encoder and decoder;
- the `SERVER_IDENT`/`SERVER_STATE` encoder and decoder;
- the client, server, and mux-daemon `SIGUSR2` dumps (FDR 0007);
- the palette's *About / transport info* and *Show echo prediction stats*;
- the §4.2 status response, `posh status`, `posh ls`, and `posh mux ls`.

`posh-proto` MUST carry a conformance test that, for each struct, (a) encodes a
value with every field non-default and asserts the decode round-trips it, and
(b) renders every surface above and asserts each field's registered key name
appears in the output. Adding a field without extending the payload (§2.1
append rule) or a renderer fails the test; that failure is the enforcement.

Axes that are not struct fields — an environment gate, a CLI flag that changes
observable behavior — MUST surface as a field before the change lands (the
§2.3 `gate_off`/`pinned_*` bits are the pattern). An FDR introducing such an
axis SHOULD state, in its Interface section, which field carries it.

### Appendix A (informative): the local/remote introspection divergences this document consolidates

The 2026-08-25 sweep of local (session daemon + local-attach client) versus
remote (roaming server/client, relay, mux) behavior found these introspection
gaps; §4 and §6 are their consolidation. Feature-parity gaps outside
introspection (echo prediction, resync, scrollback v2, agent forwarding on
local-origin sessions, sizing arbitration — the #87/#53/#137 class) are tracked
separately and are out of this document's scope; the remaining untracked
local/remote divergences the same sweep found are posh#171.

| Axis | Local today | Remote today | Consolidated by |
|---|---|---|---|
| `SIGUSR2` transport dump | none under `session/` | `remote/diag.rs` client, server, mux daemon (FDR 0007) | §4.1 (daemon answers on the socket), §6 (all dumps render one struct) |
| Status socket | daemon answers only the `posh list` IPC reply | mux `agent/mux-<id>.status.sock` (RFC 0013 §4); Arch-A server none | §4.1 (`<session>.status.sock` and `remote/<pid>.status.sock`, one response) |
| Client-state visibility | client has no predictor, sends nothing | client holds everything, sends nothing | §1–§3 (`echo_model` = 0 for a predictor-less client is still a report) |
| Periodic `[stats]` log records | local client honors `POSH_DEBUG_LOG` but emits no transport records | full periodic records + `#wedge` breadcrumbs | not consolidated here — posh#171 |

## Security Considerations

`CLIENT_IDENT` and `CLIENT_STATE` reveal the client's build, pid, start time,
link quality, and echo behavior to the authenticated serving side only; they
ride the AEAD-sealed connection (RFC 0001) or the owner-only session socket.
`CLIENT_UPSTREAM` carries the same data one hop further, to a daemon on the same
host under the same uid. The status socket inherits the session directory's
owner-only permissions; it exposes nothing a `SIGUSR2` dump does not already
write to a per-pid log, but it does so without a signal, so it MUST NOT be
bound world-readable.

No field is trusted for behavior (§3): a malformed or adversarial payload from
an authenticated peer affects only what a status reader sees. Decoders are
bounds-checked per RFC 0001's table rules; the `UPSTREAM` text is display-only
and MUST be rendered as opaque text, never parsed for `key=value` pairs by the
daemon (a hostile outer session could otherwise inject fields into the inner
session's status).

The `gates.echo_on` bit reports whether the remote PTY has `ECHO` on — a
password prompt reads as `0`. This reveals *that* a secret is being typed, not
its content, and only to a peer that already sees the session frames.

## Conformance Testing

Per the repo's convention (RFC 0001, RFC 0002), the cargo suite under
`crates/posh/src/**/tests` and `crates/posh-proto` is the normative home for
conformance until a `zz-tests_bats/` lane exists — no bats lane exists in the
repo today. The bats files named below are the implementation plan's targets
for that lane and land with the sections they cover; until then the cargo
tests for the same rows are normative. The bats lane uses binary injection via
`bats-emo`:

    require_bin POSH posh

### Covered Requirements

| Requirement | Test File | Description |
|---|---|---|
| §2.5, client MUST send `CLIENT_STATE` unconditionally | `introspect-client-state.bats` | a loopback server with no request sees id 17 on the first message and within one heartbeat of an `Echo:` palette switch |
| §2.5, local-attach client MUST send `echo_model` = 0 | `introspect-client-state.bats` | `posh attach` to a daemon; `posh status` renders `echo=none` (a sent value 0), not `echo=unknown` (nothing sent) |
| §3, relay MUST forward unchanged | `introspect-relay.bats` | a roaming client through the relay; the daemon's status shows the roaming pid as origin with `via=relay` |
| §4.1, socket contract | `introspect-status-sock.bats` | connect → lines → EOF; a dropped-listener socket reads `stale`; bind failure is non-fatal |
| §4.3, `posh status` inside a session | `introspect-status-sock.bats` | run with `POSH_SESSION` set, no target; run without, expect the one-line error |
| §5, upstream propagation | `introspect-upstream.bats` | attach inside an attach; the inner status shows the outer client's line indented; a stale outer socket renders `upstream=stale` |
| §6, coverage test | `cargo test -p posh-proto` | every struct field round-trips and appears in every renderer |

## Compatibility

- **Additive.** Ids 16, 17, 18 are allocated from RFC 0001's unassigned band;
  a pre-RFC-0014 serving side skips them by `len`. A pre-RFC-0014 client sends
  none of them and renders as `build=unknown echo=unknown` — the intended
  old-client verdict, never a default.
- **`CLIENT_STATE` grows by appending** (§2.1). A reader keeps the `0x01`
  prefix it knows. A relayout requires a new version byte and a new decoder arm;
  the old arm MUST remain for one release.
- **No behavior depends on any field** (§3), so a mixed-version fleet differs
  only in what `posh status` can show.
- **FDR 0012** is expected to make §5 unnecessary on the collapse path; §5.2
  governs its retirement.

## References

Normative:

- [RFC 2119] Key words for use in RFCs to Indicate Requirement Levels.
- [RFC 0001] Target Grammar and Capability Table — the registry ids 16–18
  extend, and the table-parsing rules every decoder here inherits.
- [RFC 0013] Server Introspection Capabilities — §1.1 is the `CLIENT_IDENT`
  payload layout; §4 is the status-socket contract §4.1 adopts.

Informative:

- [FDR 0006] Optimistic local echo — the echo models, escalation machine, and
  gates §2 reports.
- [FDR 0007] Transport state dump — the `SIGUSR2` surface §4.3 and §6 bring
  under the shared struct.
- [FDR 0012] Session layer collapse — the feature that subsumes §5.
- [RFC 0007] §3 — the append-only payload-versioning convention §2.1 adopts.
