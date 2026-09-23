---
status: proposed
date: 2026-09-23
---

# Push Command Request (`PUSH_CMD`, `PUSH_CMD_REQUEST`)

## Abstract

A viewport asks its session's daemon to run a command as a new anonymous
session in the session's working directory and move the viewport onto it —
the palette entry point of FDR 0020's *push-cmd*. This document specifies the
two capability entries that carry the exchange: `PUSH_CMD` (id 21), an offer
negotiated as a request/answer pair, and `PUSH_CMD_REQUEST` (id 22), a
request carrying a token that makes a repeated request harmless even after
the viewport has moved. The reply is the existing RFC 0008 §3.1 re-home;
nothing new is specified for it.

push-cmd's other entry point — `posh start -- <cmd>` run inside a session —
is a process on the session's host and uses the existing FDR 0012 switch
request; it is out of scope here.

## Introduction

FDR 0008's overlay was requested with `CLIENT_FLAG_ESCAPE`, a flag bit the
roaming client re-sends on every message until it sees `FLAG_OVERLAY`. That
made a lost datagram harmless, and repeats were harmless too: a daemon with an
overlay already up ignores another request.

A push cannot absorb repeats that way. Each served request creates a session,
and after the first one is served the viewport is re-homed onto the new
session, whose daemon has never seen the request. A retransmission already in
flight would reach the *new* daemon and push again. So the request carries a
token, the serving daemon remembers the tokens it has served, and it hands the
token to the session it creates, so that session ignores it too.

The offer exists because not every path can deliver a push. The relay cannot
report a re-home back to its viewport (ADR 0007), so a push over the relay
would lose track of where the viewport is. The offer is negotiated so that
only paths that carry the re-home ever see it.

## Requirements Language

The key words "MUST", "MUST NOT", "SHOULD", "SHOULD NOT" and "MAY" are to be
interpreted as described in BCP 14 (RFC 2119, RFC 8174) when, and only when,
they appear in all capitals.

## Specification

### 1. Roles

- **Viewport** — a posh client attached to a session: a local attach over the
  daemon socket, or a roaming client riding an M2 session channel.
- **Daemon** — the session daemon (`session/daemon.rs`) that owns the PTY.
- **Bridge** — the M2 session bridge (`remote/server.rs`) between a roaming
  viewport and the daemon.
- **Relay** — the per-invocation relay (`remote/relay.rs`, ADR 0007: bug fixes
  only).

Both entries live in the RFC 0001 capability table and follow its rules: an
entry a reader does not know is skipped.

### 2. `PUSH_CMD` (id 21): the offer

Payload: empty in both directions. A non-empty payload MUST be treated as
absent.

- A viewport that can perform a push-cmd (§5) MUST send `PUSH_CMD` in its
  initial capability table, and a roaming viewport MUST also send it on every
  message that carries its RFC 0013 §5.2 activity-label request.
- A daemon that implements this document, on receiving `PUSH_CMD` from a
  connection, MUST send `PUSH_CMD` to that connection **once**, on the first
  visible frame that carries an activity-label answer (the placement
  `SESSION_KIND`, id 20, already uses). It MUST NOT send it to a connection
  that did not ask.
- A viewport MUST NOT offer push-cmd to its user unless it has received
  `PUSH_CMD` during the current attach.
- The **bridge** MUST forward a viewport's `PUSH_CMD` to the daemon (as a
  `Tag::ClientCaps` table entry, like the RFC 0014 §3 entries).
- The **relay MUST NOT** forward it. Its viewport is then never offered
  push-cmd.

### 3. `PUSH_CMD_REQUEST` (id 22): the request

Payload: a big-endian unsigned 64-bit **token** (8 bytes). Token `0` is
reserved; a payload shorter than 8 bytes, or token `0`, MUST be ignored.
Bytes after the token are **reserved** for a future command field: a sender
MUST NOT send them, and a daemon that does not implement such a field MUST
ignore them (treating the request as one for its default command, §4).

- A viewport performs a push-cmd by choosing a token uniformly at random
  from the nonzero u64 range and sending a `PUSH_CMD_REQUEST` carrying it.
- A local viewport (reliable IPC) sends it once, as a `Tag::ClientCaps`
  table.
- A roaming viewport MUST attach it to every message it sends until the
  re-home arrives (§4), then MUST stop.
- The **bridge** MUST forward it to the daemon; the **relay MUST NOT**.

### 4. Serving a request

A daemon that receives a `PUSH_CMD_REQUEST` MUST:

1. **Deduplicate.** If the token is in its *served set*, ignore the request.
   Otherwise add it. The served set MUST retain at least the 64 most recent
   tokens.
2. **Create** a new session in its own group:
   - kind `Anonymous`, named by the next free auto-id;
   - working directory: the result of ADR 0008's cascade, entered at its
     second step (the daemon has no caller cwd): the kernel's cwd of the
     session's child process where the platform provides it, else the
     session's last OSC-7 directory, else the daemon's start directory, else
     `$HOME`;
   - command: `$POSH_ESCAPE_CMD` from the daemon's environment,
     whitespace-split, or a login `$SHELL` when unset or blank;
   - the ordinary session environment (`POSH_SESSION`, `POSH_GROUP`, `TERM`);
   - with its served set **seeded with the token**;
   - holding no descriptor of the creating daemon's other than its own
     listener (above all, not the creating session's PTY master).
3. **Re-home the requester**: send `Tag::Switch` (RFC 0008 §3.1) naming the
   new session **on the connection the request arrived on**. It MUST NOT
   route the switch to any other connection. (This differs from FDR 0012's
   in-session `posh attach`, whose requester is a separate process and whose
   switch goes to the most-recent-input viewport.)

If creating the session fails, the daemon MUST NOT send a switch; the
viewport's request simply goes unanswered.

The seeded token is what makes a late repeat harmless: a roaming viewport's
retransmission that reaches the new session's daemon after the re-home is
already in that daemon's served set.

### 5. The viewport

- On receiving the re-home, a viewport records the transition as any
  re-home (FDR 0016 `Event::Entered`), which pushes the session it left.
- A viewport SHOULD bound how long it waits for the re-home and, on timeout,
  stop sending the request and tell its user.
- A viewport that was not offered push-cmd MUST fall back to FDR 0008's
  overlay request (`CLIENT_FLAG_ESCAPE` / `Tag::Shell`) for the same user
  action. It MUST NOT send both.

### 6. Compatibility

| viewport | daemon | path | result |
|---|---|---|---|
| new | new | local, M2 | push-cmd |
| new | old | any | no offer → overlay |
| new | new | relay | no offer (relay drops id 21) → overlay |
| new | — | Architecture A | no daemon, no offer → overlay |
| old | new | any | never asks → overlay |

No combination sends both mechanisms, and none needs a new IPC tag: both
entries ride tables every party already reads and skips when unknown.

## Security Considerations

A push-cmd runs `$POSH_ESCAPE_CMD` with the daemon's privileges, exactly as
FDR 0008's overlay does; the request adds no new authority. The daemon socket
is same-user IPC under the hardened socket directory, and an M2 request
arrives over the already authenticated channel. A token is not a secret: it
only deduplicates. The reserved command field (§3) would add authority — a
viewport choosing what the daemon runs — and MUST be specified with its own
security analysis before any sender uses it.

## Registry

This document requests capability ids **21** (`PUSH_CMD`) and **22**
(`PUSH_CMD_REQUEST`). RFC 0001's registry gains their rows, in place, in the
commit that implements them.

## References

- RFC 0001 — capability table and registry.
- RFC 0005 §3.6 — the `notice` view shown on the pop back.
- RFC 0008 §3.1 — the re-home.
- RFC 0013 §5 — the activity label, and the request/answer pattern §2 copies.
- RFC 0014 §3 — forwarding client entries through a bridge.
- ADR 0007 — why the relay is excluded. ADR 0008 — the directory cascade.
- FDR 0008 — the overlay this falls back to. FDR 0016 — the stack.
  FDR 0020 — the feature.
