# Mux Endpoint M1 (agent-only connection) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use eng:subagent-driven-development to
> implement this plan task-by-task.

**Goal:** Build the per-destination local mux endpoint in its approved M1
shape — one agent-only enveloped connection per destination owning agent
forwarding, sessions keeping their own connections with forwarding off — so
`agent/sock` ownership becomes structural from a single client host and
posh#136 closes.

**Architecture:** A double-forked local daemon (session-daemon pattern) per
destination key under `<base>/mux/`, speaking zmx-style IPC to per-invocation
posh processes (hello/version-stamp, session ref-counting, status) and owning
one `--channels` connection bootstrapped against a new `posh-server agent`
remote subcommand (agent channels + election only, no PTY). Agent
serviceability is client-gated on the session refcount (the FDR 0014
M1 policy); the connection lingers `POSH_MUX_PERSIST` (60 s, decided) after
the last unref with agent service off. Remote side binds
`agent/mux-<client-id>.sock` and claims `agent/sock` through the existing
election — reusing the posh#152 `.active` marker machinery — which from one
client host makes it the sole agent-capable endpoint by construction.

**Tech Stack:** Rust, existing remote module (channel.rs envelope,
AgentChannelMux, sshwrap bootstrap, session/daemon.rs + session/ipc.rs
patterns). No new dependencies, no async runtime.

**Rollback:** M1 is gated behind `POSH_MUX=1` (opt-in, default off) until
promotion; without it, invocations forward per-connection exactly as today.
The gate is the single switch: off ⇒ no mux spawn, sessions keep `-A`.

**Decided inputs (do not relitigate):** M1-first (2026-07-28);
`posh-server agent` subcommand; `POSH_MUX_PERSIST` default 60 s; FDR 0014 §8
election ratified (most-recently-active, per-client-host sockets,
event-driven); client id = sanitized hostname with `POSH_CLIENT_ID` override.
Design: `docs/plans/2026-07-28-connection-mux-endpoint-design.md`.

---

### Task 1: Destination key + mux paths

**Files:** Create `crates/posh/src/remote/mux.rs` (module decl in
`remote/mod.rs`); tests in-module.

TDD the pure helpers first: `dest_key(user, host, family, port_range) ->
String` (canonicalized, filesystem-safe slug; case-fold host, default user
marker, family/port-range suffixes only when non-default);
`mux_dir()`/`mux_socket_path(key)` under the existing base-dir resolution
with the agent-dir hardening (0700, self-owned, symlink-rejecting — reuse the
existing hardening helper, do not duplicate it); `client_id()` = sanitized
hostname (`[A-Za-z0-9._-]`, else `-`), `POSH_CLIENT_ID` override via a
string-predicate function tested without env mutation. Tests: key stability
and distinctness (user@host vs host, `-4` vs auto, port ranges), slug safety
for hostile hostnames, id sanitization. Commit.

### Task 2: `posh-server agent` — the agent-only remote

**Files:** Modify `crates/posh/src/main.rs` (subcommand parse),
`crates/posh/src/remote/server.rs` (a `run_agent_only` entry reusing
`bootstrap_transport` + the enveloped receive/send seams; no PTY, no
producer); Modify `crates/posh/src/remote/agent.rs` only if the endpoint
needs a mux-socket-name variant (`agent/mux-<client-id>.sock` instead of
`srv-<pid>.sock`, plus a matching `.active` marker name — extend the #152
marker naming rather than duplicating it).

Behavior: `posh-server agent --client-id <id> [--channels implied]` prints
the same `POSH IP`/`POSH CONNECT` handshake, then serves ONLY agent channels
over the enveloped connection: binds `agent/mux-<id>.sock`, participates in
the election (claim/release/repoint via the existing machinery, marker name
`mux-<id>.active`), forwards accepted connections as `agent` channels.
Election interplay: from one client host with sessions invoked `-a`, this is
the only agent-capable endpoint — sole ownership by construction. Peer
activity gates release exactly as today. TDD with the loopback harness:
`agent_only_server_serves_channels_without_a_session`,
`agent_only_server_claims_and_repoints_like_a_sibling`. Commit per green
step.

### Task 3: The mux daemon + IPC

**Files:** Extend `crates/posh/src/remote/mux.rs`: daemonize (double-fork,
process-group; mirror `session/daemon.rs`), bind `mux/<key>.sock` (losing
race ⇒ connect to winner), IPC tags in the socket's own tag space using
`session/ipc.rs` framing: `MuxHello{version-stamp, pid}` /
`MuxHelloAck{stamp, state, key}` (stamp mismatch ⇒ client starts
`<key>.<ver>.sock` variant, old drains); `MuxSessionRef`/auto-unref on IPC
disconnect; `MuxStatus`. The daemon owns the ssh bootstrap for
`posh-server agent` (via sshwrap with `channels: true` and the agent-only
tail) and the client half of the agent channels (dial local `$SSH_AUTH_SOCK`
per OPEN — reuse `AgentClient` + `AgentChannelMux`). Refcount 0 ⇒ FAIL new
agent opens, close open channels; linger 60 s then exit. Unit-test the
refcount/linger state machine with virtual time; loopback-test hello/ref
lifecycle. Commit per green step.

### Task 4: Client integration behind `POSH_MUX`

**Files:** Modify `crates/posh/src/main.rs` + `remote/sshwrap.rs`: when
`POSH_MUX` is on AND the target is remote AND forwarding would be on: ensure
the mux endpoint for the dest key (spawn if absent), hold a `MuxSessionRef`
for the invocation's lifetime, and invoke the session's own bootstrap with
forwarding OFF (`-a` semantics) so no per-session `srv-<pid>` endpoint
exists. `POSH_MUX` off ⇒ byte-identical today's behavior (pin with a test on
the bootstrap command construction). Failure posture: mux spawn/hello
failure warns and falls back to per-connection forwarding (never strand the
user without an agent). Commit.

### Task 5: E2E + conformance + docs

- Extend `just debug-agent-e2e` with an M1 lane: real ssh loopback, mux
  endpoint up, TWO concurrent sessions, `ssh-add -l` through `agent/sock`
  succeeds from both; kill/idle one client and verify zero-window handoff
  (the FDR 0014 promotion-criteria E2E: reproduce-then-fixed).
- RFC 0011 §7/§8 conformance: single-client-host sole ownership (bound
  election trivially stable), and the §8 two-host election unit-tested via
  two mux-named endpoints with markers.
- Docs: posh(1) `POSH_MUX`/`POSH_CLIENT_ID`/`POSH_MUX_PERSIST` ENVIRONMENT
  entries; posh-server(1) `agent` subcommand; FDR 0014 status advance
  (promotion criteria met ⇒ `Closes #136` in the landing commit); FDR 0004
  "Forwarded once" updated to the mux path; mux design doc marked
  implemented (M1); AGENTS.md key-fact bullet updated.
- Final: pre-merge skill lane + attestation + `merge-this-session`; the
  landing commit carries `Closes #136`.
