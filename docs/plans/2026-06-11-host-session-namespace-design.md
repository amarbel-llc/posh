# Unified host:session namespace — approved design

github #37 part 2. Approved 2026-06-11 after design review; the normative
artifacts are the RFC (grammar, wire) and FDR (feature record) this design
produced — read those first. This document records the decision trail.

## What

One namespace for everything posh can attach to, scp-style:

```
posh dev                  # local session (bare word, unchanged)
posh :dev                 # explicit local; :grp/dev addresses a group
posh box:dev              # session dev on ssh host box, roaming UDP
posh user@box:grp/dev     # full form
posh [fe80::1]:dev        # bracketed IPv6 host
posh list box:            # remote listing, host-prefixed output
posh box:<Tab>            # completes remote session names
```

## Decisions (with rationale)

1. **Attach-or-create symmetry**: `box:dev` creates the remote session if
   absent, exactly like local `posh dev`.
2. **scp grammar**: first-colon split, `user@` prefix, brackets for IPv6;
   a malformed session part (empty, or containing `:`) falls back to
   pure-host, so bare `fe80::1` / `::1` / `box:` keep today's meaning.
   First `/` in the session part splits group from session.
3. **Typed parser**: the grammar is a total function
   `Target::parse(&str) -> Target` returning an enum (`LocalSession`,
   `Local`, `Host`, `RemoteSession`); every fallback is a typed outcome.
   Dispatch is one `match`; edge cases are a table-driven test.
4. **Architecture A — transport composition** (v1): `box:dev` runs
   `posh-server new -- posh attach [-g grp] dev` over ssh; the session
   daemon is the only persistence, each roaming client is an ephemeral
   transport pair. B (server proxies session IPC natively) and C (daemon
   speaks UDP itself) are recorded in the FDR as natural progressions.
5. **Both detach paths stay**: Ctrl-\ unwinds via the inner attach;
   Ctrl-^ . quits the transport and the inner attach detaches on hangup.
   Session survives either way.
6. **Wire evolution = capability table, not version bumps or feature
   bits**: one reserved flags bit (0x02, both directions) means "a TLV
   capability table follows the fixed header"; unknown ids skip by
   length; the table rides in every datagram (connectionless, idempotent
   — no handshake state). Registry: id 0 PROTOCOL_VERSION (meta escape
   hatch, currently 1), id 1 EXIT_STATUS (client: empty = "I understand";
   server: payload = shell-style code on shutdown frames). Reserved:
   TERM_FEATURES (outer-terminal facts — the termcap direction).
   POSH CONNECT is untouched. Mixed versions degrade, never corrupt.
7. **Exit status end-to-end**: inner attach exits with the session code
   (#18); the server carries it via EXIT_STATUS; `posh box:dev; echo $?`
   reports the session shell's status.
8. **Completion**: live `ssh -o BatchMode=yes -o ConnectTimeout=2 box
   posh list --short`, cached per host (~30s TTL) under
   `$XDG_CACHE_HOME/posh/`. `posh list box:` is the same query, uncached,
   host-prefixed output. Group-qualified completion deferred.

## Tuning levers

- Completion ssh ConnectTimeout: 2s (signal: Tab latency complaints).
- Completion cache TTL: 30s (signal: staleness complaints).
- Capability table in every datagram: revisit to send-until-first-ack if
  payloads grow (signal: datagram overhead from TERM_FEATURES-scale
  entries).

## Rollback

Additive syntax; `posh ssh` / `posh attach` remain the permanent dual
architecture. The wire change is negotiated (capability table), so mixed
versions degrade to v0 behavior. Promotion criterion for relying on
exit-status: both fleet hosts on a capability-table build. Rollback is a
single revert; no migration state.

## Testing

Table-driven `Target::parse` matrix; capability-table roundtrip +
unknown-id skip + 4-way skew matrix; pty e2e (sandbox-safe, no sshd):
full composition over loopback UDP (attach, type, Ctrl-\, survive,
re-attach), exit status across the transport, `:dev` local form.
Cross-host flows go to docs/manual-testing.md.
