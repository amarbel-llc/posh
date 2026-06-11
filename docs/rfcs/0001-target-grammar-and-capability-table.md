---
status: accepted
date: 2026-06-11
---

# posh Target Grammar and Datagram Capability Table

## Abstract

This document specifies two interfaces for the posh terminal multiplexer:
the grammar by which a bare command-line argument resolves to an attach
target (a local session, an ssh host, or a session on a remote host,
scp-style `[user@]host:[group/]session`), and a backward-compatible TLV
capability table appended to the encrypted UDP datagram headers, through
which client and server negotiate protocol extensions such as exit-status
propagation. Together they define the contract for the unified
host:session namespace.

## Introduction

posh combines local persistent sessions (zmx lineage) with a roaming
remote transport (mosh lineage). Before this specification, a bare
argument was either a local session name or — when it contained `@`, `.`,
or `:` — an ssh destination, and the UDP wire format had no mechanism for
evolving without breaking mixed-version peers.

This RFC specifies (1) the **target grammar**: one namespace covering
local sessions, remote shells, and remote sessions, with deterministic
fallbacks chosen so every pre-existing form keeps its meaning; (2) the
**remote command contract**: what a conforming client executes over ssh
to reach a remote session; and (3) the **capability table**: the sole
sanctioned mechanism for extending the datagram protocol, with its
initial registry. The feature-level rationale lives in FDR 0001
(`docs/features/`); the architecture trail in
`docs/plans/2026-06-11-host-session-namespace-design.md`.

## Requirements Language

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Specification

### 1. Target grammar

A *target* is the first non-flag command-line argument when it does not
match a built-in subcommand name. Parsing MUST be total: every input
resolves to exactly one of the four target kinds below; malformed
namespace forms fall back as specified rather than erroring.

The normative model is the parser's result type:

```rust
enum Target {
    /// Bare word: local session, attach-or-create (legacy form).
    LocalSession { name: String },
    /// `:session` / `:group/session`: explicit local.
    Local { group: Option<String>, session: String },
    /// An ssh destination: plain roaming shell (mosh form).
    Host { user: Option<String>, host: String },
    /// `[user@]host:[group/]session`: a session on a remote host.
    RemoteSession {
        user: Option<String>,
        host: String,
        group: Option<String>,
        session: String,
    },
}
```

Resolution MUST apply these rules in order; the first match wins:

1. **Explicit local.** An argument beginning with `:` resolves to
   `Local` with the remainder parsed per rule 5. The remainder MUST be a
   valid session part (non-empty, containing no `:`) — otherwise the
   argument falls through to rule 3 (IPv6 literals such as `::1` resolve
   there to `Host` via their empty head). A bare `:` is
   `LocalSession { name: ":" }`.
2. **Bracketed host.** An argument beginning with `[` up to a matching
   `]` is a host literal (IPv6-safe). A `:suffix` after the bracket, if
   non-empty, resolves to `RemoteSession` (suffix parsed per rule 5);
   otherwise the argument resolves to `Host`. An unterminated `[` falls
   through to rule 3.
3. **First-colon split.** For an argument containing `:`, split at the
   FIRST colon into a head and a session part. If the head and session
   part are non-empty and the session part contains no `:`, the target is
   `RemoteSession` (head parsed per rule 4, session part per rule 5).
   Otherwise the argument is `Host`: an empty session part is a bare
   trailing colon and is dropped (`box:` is the host `box`); a session
   part containing `:` belongs to an IPv6 literal, so the full string is
   the host. The host token is parsed per rule 4 either way.
4. **user@ split.** Within a host token, text before the first `@` is
   the optional user; the remainder is the host.
5. **group/ split.** Within a session part, text before the FIRST `/`
   is the group, the remainder the session, both MUST be non-empty for
   the split to apply; otherwise the whole part is the session and the
   group is absent (the peer's default group applies).
6. **No colon.** An argument containing `@` or `.` resolves to `Host`;
   anything else resolves to `LocalSession`.

Examples (normative):

| Input | Target |
|---|---|
| `dev` | `LocalSession { name: "dev" }` |
| `:dev` | `Local { group: None, session: "dev" }` |
| `:grp/dev` | `Local { group: Some("grp"), session: "dev" }` |
| `box.example` | `Host { host: "box.example" }` |
| `user@box` | `Host { user: Some("user"), host: "box" }` |
| `box:dev` | `RemoteSession { host: "box", session: "dev" }` |
| `user@box:grp/dev` | `RemoteSession { user, host, group, session }` |
| `[fe80::1]:dev` | `RemoteSession { host: "fe80::1", session: "dev" }` |
| `fe80::1` | `Host { host: "fe80::1" }` (session part `:1` is malformed) |
| `::1` | `Host { host: "::1" }` (empty head, malformed session part) |
| `box:` | `Host { host: "box" }` (empty session part) |
| `[fe80::1]` | `Host { host: "fe80::1" }` |

A session name containing a literal `/` is not addressable through a
group-qualified namespace form (rule 5 takes precedence); implementations
MUST leave such names reachable via the explicit `attach` subcommand.

`list` position: an implementation supporting remote listing MUST accept
a `Host`-resolving argument with a trailing `:` (e.g. `posh list box:`)
and print each remote session name prefixed with `host:` (and `group/`
where not the default), such that every printed name is itself a valid
`RemoteSession` target.

### 2. Remote command contract

To attach to `RemoteSession { user, host, group, session }`, a client
MUST execute, over ssh to `[user@]host`:

```
[locale-env-prefixes] posh-server new [-4|-6] [-p RANGE] -- posh [-g GROUP] attach SESSION [command...]
```

(`-g` is a global posh option and precedes the subcommand; trailing
arguments become the create-command, mirroring local
`posh attach <name> [command...]`.)

- The session, group, and any forwarded environment values MUST be
  shell-quoted such that arbitrary names survive the ssh hop losslessly.
- The server host MUST provide `posh-server` and `posh` on the
  non-interactive ssh PATH.
- The client then connects exactly as for a `Host` target (parsing
  `POSH CONNECT`, key in the environment). The `POSH CONNECT` line
  format is unchanged by this specification.
- Session persistence is provided solely by the session daemon on the
  remote host; the `posh-server` + inner `posh attach` pair is transport
  and MUST be treated as disposable. Concurrent attaches from multiple
  clients each create their own pair.

### 3. Datagram capability table

The encrypted payload of a posh datagram begins with a fixed header
(2-byte timestamp, 2-byte timestamp echo) followed by the direction's
message (client message or server frame), each beginning with a 1-byte
`flags` field.

Bit `0x02` of `flags` in either direction is reserved as the EXTENSION
bit:

- When clear, the message body follows the flags byte exactly as in the
  baseline (pre-capability) format.
- When set, a capability table immediately follows the flags byte, and
  the message body follows the table.

The capability table is:

```
count: u8
entries: count × ( id: u8, len: u8, payload: len bytes )
```

- A receiver MUST skip unknown ids using `len`.
- A sender MUST NOT set the EXTENSION bit when the table is empty.
- Senders SHOULD include their table in every message (the protocol is
  connectionless; no handshake state exists). Receivers MUST NOT assume
  a capability persists across messages beyond the most recently
  received table from the peer.
- A receiver that has never seen the EXTENSION bit from its peer MUST
  treat the peer as baseline and MUST NOT send capability-dependent
  payloads it could misparse. (Capability *entries* themselves are
  always safe to send once the peer has sent any table, and are safe in
  the sender's own table regardless, since unknown ids are skipped.)

#### Capability registry

| id | Name | Direction | Payload | Meaning |
|---|---|---|---|---|
| 0 | `PROTOCOL_VERSION` | both | 1 byte | Meta escape hatch: the version of the post-table format. Currently 1. A future value > 1 MAY redefine everything after the table; receivers seeing a higher version than they implement MUST fall back to baseline interpretation of the body. |
| 1 | `EXIT_STATUS` | both | client: 0 bytes; server: 1 byte | Client entry (empty payload) advertises understanding. A server MUST NOT send the entry unless the client advertised it; when sent, it MUST appear on shutdown-flagged frames and its payload is the session command's shell-style exit code (`WEXITSTATUS`, or 128+signal). A client receiving it MUST exit with that code after the shutdown handshake. |
| 2–223 | — | — | — | Unassigned; allocate sequentially via this registry. |
| 224–255 | — | — | — | Reserved for experiments; MUST NOT appear in released builds. |

`TERM_FEATURES` (outer-terminal facts such as color depth and kitty
protocol support, advertised client → server so the server-side terminal
model can answer queries honestly) is anticipated as the next allocation
but is NOT specified by this document.

## Security Considerations

- The capability table is inside the AEAD-sealed payload; it is
  authenticated and confidential to the same degree as all session data.
  A forged table cannot be injected without the session key.
- Table parsing MUST be bounds-checked: `count` and each `len` are
  attacker-controlled (by an authenticated peer) and a malformed table
  MUST cause the message to be discarded, not a panic or over-read.
  Total table size is bounded by the datagram size.
- The remote command contract interpolates names into a shell command
  line; conforming clients MUST apply the shell-quoting requirement of
  section 2 to every interpolated value. The quoting function is shared
  with the existing locale forwarding (see `remote/sshwrap.rs`).
- Rule-based target fallbacks mean a typo in a session part can resolve
  to a `Host` and open an ssh connection (e.g. `box:` instead of
  `box:dev`); ssh's own host authentication is the boundary there.

## Conformance Testing

Wire-level and grammar requirements are covered by the cargo test suite
(`crates/posh`): a table-driven test over the normative examples above
MUST exist for `Target::parse`, and capability-table tests MUST cover
the encode/decode roundtrip, unknown-id skip, malformed-table rejection,
and the four-way version-skew matrix (baseline/extended × client/server).
End-to-end behavior (exit status across the transport, the composed
remote attach) is covered by the pty integration tests in
`crates/posh/tests/`. A `zz-tests_bats/` conformance suite with binary
injection via `bats-emo` (`require_bin POSH posh`) is the intended home
for cross-implementation CLI conformance once one exists; until then the
cargo suite is normative.

## Compatibility

- Every pre-existing argument form resolves identically under the new
  grammar (rules 3 and 6 reproduce the prior heuristic for all inputs
  that were previously meaningful).
- Baseline peers (no EXTENSION bit) interoperate with extended peers in
  all four combinations; extended behavior degrades to baseline, never
  corrupts. There is no flag day.
- `POSH CONNECT`, the session IPC (`Tag` frames), and the ssh bootstrap
  are unchanged.
- Future datagram-protocol changes MUST use the capability table (new
  registry entries) or, for incompatible redesigns, a `PROTOCOL_VERSION`
  bump — ad-hoc format changes are non-conformant.

## References

- FDR 0001: Unified host:session namespace (`docs/features/`).
- ADR 0001: macOS/Linux libc portability (`docs/decisions/`) — platform
  constraints on the transport implementation.
- Design trail: `docs/plans/2026-06-11-host-session-namespace-design.md`.
- [RFC 2119] Key words for use in RFCs to Indicate Requirement Levels.
- mosh: Winstein & Balakrishnan, "Mosh: An Interactive Remote Shell for
  Mobile Clients" (USENIX ATC 2012) — the transport lineage.
