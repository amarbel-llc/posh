# posh-server live introspection (wire + local) — design

- Status: approved 2026-08-19 (brainstormed + approved in-session)
- Motivating incident: posh#161 triage, 2026-08-19 — answering "what build is
  the far end running" required `/proc/<pid>/exe` spelunking plus grepping
  `/nix/store` binaries for embedded git hashes. Nothing in posh exposes a
  running server's identity or state to anyone who is not already shelled
  into the server host reading log files.

## Problem

The client-side command palette exposes live client data (mode, gates,
transport info), but the far end is opaque:

- No surface reports a running server's **build** (version + git hash), so
  version-skew questions during an incident are unanswerable from a client.
- The #6 `CAP_DIAG` server-state piggyback exists but is debug-posture-gated
  and consumed only by the client's SIGUSR2 dump — the palette never shows it.
- Roaming servers have no local socket by design (mosh-style), so on the
  server host the only introspection is SIGUSR2-to-log (FDR 0007) plus the
  posh#161 always-on mux logs — file archaeology, not a query.
- The agent-only mux peer — the long-lived shared endpoint whose health
  gates agent forwarding for a whole client host (posh#161/#162) — has no
  status surface at all on its own host.

## Decisions (three axes, chosen 2026-08-19)

1. **Surface: both** — wire-carried introspection displayed in the palette,
   plus a host-local query for the mux peer.
2. **Wire shape: identity always, state on demand.** A small static identity
   block is delivered reliably at connection setup (until-acked, zero
   steady-state cost); live state flows only while the client asks.
3. **Local scope: mux peer only.** Only `posh-server agent`/`mux` grows a
   local status socket. Roaming per-session servers keep the deliberate
   no-local-socket design; their introspection is the wire half + SIGUSR2.

## 1. Wire half

Two additions to the existing per-frame caps-table mechanism. Both are
optional capability ids; the ignore-unknown-caps contract makes every skew
combination degrade gracefully.

- **`CAP_SERVER_IDENT`** (server → client): version + build hash (the
  `posh version` string), pid, start time, and gate posture. The client
  advertises the request in its `Tag::Init` caps (re-advertised after
  RESYNC); the server attaches the ident payload to outgoing frames **until
  its acked-frame counter proves receipt** — the transport's existing
  until-acked idiom, so delivery is reliable with zero steady-state cost
  once acked.
- **Server state on demand**: generalize `CAP_DIAG` from "debug posture" to
  "requested": opening the palette's *About / transport info* makes the
  client advertise it on its messages; the server attaches its live state
  block (the existing ServerDiag v2 fields + the agent-endpoint block +
  uptime) to frames while advertised; closing the view stops the
  advertisement. The piggyback mechanism is unchanged — only the gate moves
  from a startup posture to a palette-driven toggle.
- **Palette**: the About view grows a "remote" section — build, uptime,
  transport + agent state — or `unknown (pre-introspection server)` when no
  ident arrives within a beat. The payload to `posh-palette` is additive
  JSON on the RFC 0005 control channel.
- **Mux connection**: same contract on the reserved heartbeat channel
  (ordinal 1, today one-directional client→remote). The mux daemon requests
  ident; the remote answers with one Empty frame carrying it; the daemon
  caches it and includes it in its IPC `StatusReply` — so `posh mux ls` /
  `posh ls` on the **client** host reports the remote endpoint's build.
  Deliberate side effect: this is the first **positive round-trip probe**
  on an idle M1 connection — the remote-liveness primitive the posh#162
  reconnect work needs. This design builds that primitive on purpose.

## 2. Local half

`posh-server agent`/`mux` binds a small status IPC socket beside its agent
socket (same `agent/` dir; pid-keyed liveness record; GC'd and unlinked
exactly like its sibling files; a bind failure is logged and never fatal).
It answers the mux-daemon-style Status verb with one line: ident + peer
address, heard-age, live/cumulative channel counts, and `agent/sock`
ownership. The server host's `posh ls` gains a "remote endpoints" section
reading these sockets — so the posh#161 question "is the twerk connection's
endpoint alive, and how stale is it" becomes one command on the server host.

## 3. Records

The new capability ids go into RFC 0001's registry table (maintained in
place), citing a short new RFC — or an RFC 0005/0008 amendment, decided at
writing time — that specifies the ident/state block encodings. The palette
About extension is additive fields on the RFC 0005 JSON.

## 4. Rollback

Purely additive; no dual-architecture period needed:

- Old server + new client: no ident/state arrives; About shows the
  pre-introspection marker. Old client + new server: caps never advertised,
  server sends nothing new. Pinned by a four-way skew test at the caps
  level (the frames-matrix pattern).
- Rollback = ship a client that stops advertising the two caps (server
  attachments are entirely client-solicited).
- The status socket is passive/read-only and removable independently.

## 5. Tuning levers

- **Palette state-refresh cadence** while the info view is open — start
  1 s. Change signal: state-block bytes visibly inflating frames on slow
  links, or battery complaints.
- **`posh ls` per-endpoint status-socket read timeout** — start ~200 ms.
  Change signal: real contention or slow endpoints producing false
  "stale" rows.
- **Ident re-arm points** — start: Init and RESYNC only. Change signal: a
  stale ident sighted after a server restart behind the same address.

## 6. Testing

- Encode/decode pins for the ident and state blocks (truncation-rejecting,
  like the mux hello tests).
- The four-way client×server skew matrix at the caps level.
- An in-process mux test extending the existing daemon harness: remote
  ident lands in the daemon's `StatusReply`.
- Endpoint status-socket lifecycle (bind, GC, Drop unlink) beside the
  existing `agent.rs` socket tests.

## Non-goals

- No local sockets for roaming per-session servers (mosh design holds).
- No reconnect behavior change — posh#162 stays separate; this only builds
  the round-trip probe primitive it can later use.
- No always-on state streaming; steady-state wire cost must remain zero
  when nobody is looking.
