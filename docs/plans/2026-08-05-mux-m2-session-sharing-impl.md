# Mux M2 (Session Sharing) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use eng:subagent-driven-development to
> implement this plan task-by-task.

**Goal:** Named-session attaches ride the per-destination mux daemon's one
AEAD-UDP connection as RFC 0011 `session`-kind channels, behind the opt-in
`POSH_MUX_SESSIONS`, per the 2026-08-05 M2 revision of
`docs/plans/2026-07-28-connection-mux-endpoint-design.md`.

**Architecture:** The foreground client speaks new `MuxTag` session tags over
the mux IPC socket (whole assembled `ClientMessage`/`ServerFrame` bytes, no
terminal model in the daemon); the mux daemon maps each IPC session channel
1:1 to a wire `session` channel; the remote `posh-server mux` (generalized
`agent` verb) holds a channel table of DaemonLinks, applying the RFC 0008 §3
relay contract per channel. Fallback at every failure edge is today's
per-invocation relay connection, byte-identical.

**Tech Stack:** Rust workspace (`crates/posh`), existing `remote/mux.rs`,
`remote/relay.rs`, `remote/server.rs`, `remote/client.rs`, `remote/channel.rs`
machinery; Go (`posh-palette`) for the info table only.

**Rollback:** `POSH_MUX_SESSIONS` unset (opt-in gate) — the per-invocation
path is the default and the automatic fallback; nothing else changes
behavior. `POSH_MUX=0` still kills the whole endpoint.

**Verification lanes:** `just debug-cargo test -p posh <filter>` (dev loop),
`just debug-agent-e2e` (ignored E2Es), `just debug-mux-load` (transport
regression bar), `just lint-doc` (scd), merge gate = `just` via
`merge-this-session` (do not pre-run).

---

### Task 1: IPC session tags + codecs (`remote/mux.rs`)

**Promotion criteria:** N/A (additive tags in the mux IPC tag space).

**Files:**
- Modify: `crates/posh/src/remote/mux.rs` (MuxTag enum ~line 265, codec
  section, `mod tests`)

New tags + payloads (zmx framing, 1-byte tag + u32 LE len, as today):

```rust
// MuxTag additions (values continue the existing space):
SessionOpen = 6,   // client→mux: UTF-8 RFC 0001 target
SessionOpenAck = 7,// mux→client: u8 ok + u64 LE channel ordinal | u16 len + UTF-8 reason
SessionMsg = 8,    // client→mux: one encoded ClientMessage (opaque bytes)
SessionFrame = 9,  // mux→client: u32 LE srtt_ms + one encoded ServerFrame (opaque)
SessionClose = 10, // either: u8 origin (0 local detach / 1 remote terminal) + optional exit payload
```

`SessionFrame` carries the connection's live `srtt_ms` so the foreground
client's prediction engine keeps its SRTT trigger without owning a UDP
socket (RFC 0007 machinery reads it in Task 4).

**Steps (TDD, commit per green):**
1. Write failing round-trip/truncation tests in the style of
   `mux_hello_roundtrips_and_rejects_truncation`:
   `session_open_roundtrips_and_rejects_truncation`,
   `session_open_ack_roundtrips_ok_and_failure`,
   `session_frame_prefixes_srtt_and_keeps_body_opaque`,
   `session_close_roundtrips_both_origins`,
   `mux_frame_buffer_skips_unknown_session_tags_forward_compat`.
2. `just debug-cargo test -p posh remote::mux::tests::session_` — FAIL.
3. Implement the tag variants + encode/decode fns beside the Hello codecs.
4. Same filter — PASS. Commit (`remote/mux: M2 IPC session tags (1/N)`).

### Task 2: mux daemon session-channel routing (`remote/mux.rs`)

**Promotion criteria:** N/A (new path, gated by the client never sending the
tags unless opted in).

**Files:**
- Modify: `crates/posh/src/remote/mux.rs` (`IpcConn`, `mux_loop`)
- Modify: `crates/posh/src/remote/channel.rs` (client-initiated session
  ordinal allocation — `ChannelAllocator` already supports kinds; expose
  `next(KIND_SESSION)` for the mux)

Client-side channel table: `IpcConn` gains `session: Option<ChannelId>`;
`mux_loop` gains `session_routes: HashMap<ChannelId, usize /*conn idx*/>`
(rebuilt on conn removal — indices shift; store by a stable conn id, mirror
the daemon.rs client-id pattern). On `SessionOpen`: allocate the wire
channel, send the OPEN-bearing instruction whose payload is the target
(retransmit until first frame acks it — reuse the send_due/RTO discipline by
carrying the open through a small per-channel outstate), reply
`SessionOpenAck`, count a session ref. Wire recv: `session`-kind
instructions route by channel id to the owning conn as `SessionFrame`
(prefixing `conn.srtt() as u32`); unknown session channel → drop (log once).
`SessionMsg` → seal_on(that channel) + fragment + send. `SessionClose` /
IPC-conn drop → wire CLOSE (`CLIENT_FLAG_SHUTDOWN` path) + unref.

**Steps:**
1. Failing tests (socketpair + loopback, `start_inprocess_daemon` pattern):
   `session_open_allocates_channel_acks_and_refs`,
   `session_msg_routes_to_the_wire_and_frames_route_back`,
   `two_ipc_conns_get_disjoint_channels_and_isolated_frames`,
   `ipc_conn_drop_closes_its_wire_channel_and_unrefs`,
   `open_retransmits_target_until_acked`.
2. FAIL → implement → PASS → commit (`remote/mux: M2 session routing (2/N)`).

### Task 3: the remote channel-table peer (`remote/server.rs`, `main.rs`)

**Promotion criteria:** `posh-server agent` alias removable only after no
supported client spawns it (a later, dated decision — record, don't do).

**Files:**
- Modify: `crates/posh/src/main.rs` (server verb routing ~306: `mux` verb,
  `agent` → same entry)
- Modify: `crates/posh/src/remote/server.rs` (`agent_only_loop` →
  `mux_peer_loop` generalization)
- Reuse: `crates/posh/src/remote/relay.rs` `DaemonLink`, `HeldFrame`, the
  Init/caps forwarding and input-bridge fns — lift them `pub(crate)` rather
  than copying (they are the §3 contract; divergence here is a bug).

`mux_peer_loop`: agent handling exactly as `agent_only_loop`, plus a
`Vec<SessionChannel>` where `SessionChannel { id: ChannelId, link: DaemonLink,
held: Option<HeldFrame>, inbox: InputInbox }`. Wire OPEN (first instruction on
an unseen session ordinal, payload = target): bound check
(`MAX_SESSION_CHANNELS = 16`, refuse with the terminal per §3.4), then
connect-or-create the named daemon (`session::connect_or_create`),
`Tag::Init` with forwarded caps + `CAP_LOSSY`. Daemon `Tag::Frame` →
per-channel held-frame + send on that channel (§4.1: session sends before
agent bulk — extend `iteration_sends`' caller to drain session channels
first). Wire `ClientMessage` → `Tag::Input`/`Resize`/`FrameAck` bridge
(relay_loop's conversion, per channel). DaemonLink EOF → channel terminal +
exit-status payload; other channels untouched.

**Steps:**
1. Failing tests: `mux_peer_opens_daemonlink_per_session_channel`,
   `mux_peer_bounds_session_channels_and_refuses_past_16`,
   `daemonlink_eof_closes_only_its_channel`,
   `session_frames_precede_agent_bulk_in_the_drain`,
   `agent_only_alias_still_serves_zero_session_case`.
   (In-process: fake daemon sockets per the relay tests' pattern.)
2. FAIL → implement → PASS → commit (`remote/server: M2 channel-table peer (3/N)`).

### Task 4: client integration behind `POSH_MUX_SESSIONS` (`main.rs`,
`remote/client.rs`, `remote/mux.rs` client half)

**Promotion criteria:** per-invocation relay connections remain the default
until the design doc's promotion criteria are met (E2E + loadprobe bar +
soak); this task changes nothing with the gate unset — pin by test.

**Files:**
- Modify: `crates/posh/src/remote/mux.rs` (`mux_sessions_selected()` —
  OPT-IN truthy shape via `sshwrap::env_value_on`, like `POSH_CHANNELS`,
  NOT the default-on parser; `MuxHandle::open_session(target)` client fn)
- Modify: `crates/posh/src/main.rs` (foreground attach ~529: try the mux
  session path before `foreground_server_tail` when gate on + mux handle
  live; any failure → warn once → existing path)
- Modify: `crates/posh/src/remote/client.rs` (the transport seam: a
  `MessageTransport` enum { Udp(existing), MuxIpc(UnixStream+MuxFrameBuffer) }
  at the assembled-message boundary — `drive_client` reads/writes whole
  messages + srtt hint; prediction/palette/scrollview untouched)

**Steps:**
1. Failing tests: `mux_sessions_gate_is_opt_in` (unset/0 ⇒ off; truthy ⇒ on),
   `gate_off_keeps_the_attach_path_byte_identical` (argv construction pinned,
   the M1 `mux_gate_off` pattern), `open_failure_falls_back_to_relay_attach`,
   plus a socketpair `drive_client`-over-MuxIpc smoke
   (`client_drives_a_session_over_mux_ipc`).
2. FAIL → implement → PASS → commit (`remote/client: M2 attach via mux (4/N)`).

### Task 5: E2E + regression lanes

**Files:**
- Modify: `crates/posh/src/remote/mux.rs` tests (the `#[ignore]` E2E lane)
- Modify: `justfile` only if a new filter is needed (prefer the existing
  `debug-agent-e2e` `agent_forward` filter — name the new E2E to match, e.g.
  `agent_forward_mux_m2_two_sessions_share_one_connection`).

The E2E (design-doc promotion criterion): two sessions + agent bulk on ONE
connection; `ssh-add -l` succeeds mid-stream; kill one client — the other
session unaffected; reattach; zero cross-channel talk (frame targets
asserted). Also re-run `just debug-mux-load` (bar: unchanged — M2 code
doesn't alter the transport, only its callers) and note in the loadprobe
module doc that the N-session model now has a production counterpart.

**Steps:** write E2E → run via `just debug-agent-e2e` → green → commit.

### Task 6: palette info surface (About / transport info)

**Files:**
- Modify: `crates/posh/src/remote/palette.rs` (new command + data assembly)
- Modify: `crates/posh/src/remote/client.rs` (gate values resolved at
  attach; congestion summary via `MuxStatus` when on the mux path)
- Modify: `posh-palette/` (Go: render the two-column table)

Rows: posh version (build-time env), destination key, connection mode
(mux-sessions / mux-agent-only / per-invocation; enveloped / baseline),
resolved `POSH_MUX`, `POSH_MUX_SESSIONS`, `POSH_CONGESTION`,
`POSH_CHANNELS`, `POSH_SESSION_FRAMES`, `POSH_RELAY` (value + whether env or
default), and cwnd/cuts/streak_hwm from `MuxStatus` when available.

**Steps:** failing Rust-side assembly test (`about_table_reports_resolved_
gates_and_mode`) → implement → Go renderer row (existing table widget) →
`nix build .#posh-palette` (or `just build-go` lane) → commit.

### Task 7: docs + record updates (same merge as the code)

**Files:**
- Modify: `doc/posh.1.scd` (`POSH_MUX_SESSIONS` ENVIRONMENT entry, opt-in;
  cross-ref from `POSH_MUX`), `doc/posh-server.1.scd` (`mux` verb, `agent`
  alias) — `just lint-doc`.
- Modify: `AGENTS.md` (the mux bullet: session sharing exists behind
  `POSH_MUX_SESSIONS`, opt-in; palette About surface).
- Modify: `docs/rfcs/0011-multiplexed-datagram-channels.md` Compatibility
  (additional session channels now SHIP behind the opt-in; the §9.2
  mechanism precondition is met — cite).
- Modify: `docs/features/0014-...md` More Information (mux peer verb) and
  `docs/plans/2026-07-28-mux-endpoint-m1-impl.md` status one-liner.
- FDR for the user-facing feature: defer until promotion (the M2 revision
  in the design doc is the record until then) — note in the final commit.

**Steps:** edit → `just lint-doc` → commit (final commit message carries
`Refs #54` and the design-doc pointer; promotion issues get filed at merge).

---

**Merge:** one `merge-this-session` cycle at the end (pre-merge skill
attestation as usual); cheap per-crate `cargo build`/test filters during the
loop, no full `just` before the merge.