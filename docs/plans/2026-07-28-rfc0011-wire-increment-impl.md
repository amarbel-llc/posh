# RFC 0011 Wire Increment (envelope + agent kind + single session channel) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use eng:subagent-driven-development to
> implement this plan task-by-task.

**Goal:** Implement RFC 0011's first migration increment — the 9-byte channel
envelope, partitioned channel identifiers, the `agent` channel kind, and a
single `session` channel — behind an explicit opt-in selector, leaving baseline
connections byte-identical.

**Architecture:** The envelope inserts between fragment reassembly and message
decode (RFC 0011 §1); §4's concurrent reassembly already landed. A new
`remote/channel.rs` owns the envelope codec, the partitioned `u64` identifier,
and the agent-channel payload codec. `server_loop` and `drive_client` gain an
`enveloped: bool` (from the §6 CLI selector / `POSH_CHANNELS` opt-in): on send
they prepend the envelope before fragmenting; on receive they parse it after
reassembly and dispatch by kind. Agent traffic on enveloped connections moves
from `CAP_AGENT_*` capability entries to per-forwarded-connection `agent`
channels with per-channel cumulative offsets; the FDR 0004 symlink election
stays (RFC 0011 §7 conditional rule) until the mux endpoint exists.

**Tech Stack:** Rust, existing posh remote module (no new dependencies, no
async runtime; `poll`-driven loops as today).

**Rollback:** Purely additive behind the selector: no `--channels` on the
remote invocation and no `POSH_CHANNELS=1` locally ⇒ baseline wire,
byte-identical. Revert = drop the commits; no data or format migration.

**Constraints recap (read first):**

- RFC 0011 (docs/rfcs/0011-multiplexed-datagram-channels.md) — §2 envelope,
  §3 identifiers, §3.3 lifecycle, §3.4 limits, §4.1 sender interleaving
  SHOULDs, §5 agent payload + serviceability bound, §6 negotiation, §7
  conditional ownership (DO NOT remove the election), §8.
- Retired ids 6/7/8 MUST NOT be sent on enveloped connections; MUST still work
  on baseline ones (RFC 0001 registry untouched).
- Do NOT close posh#136 in this increment (needs the mux endpoint; see
  docs/plans/2026-07-28-connection-mux-endpoint-design.md).
- Run tests via `just debug-cargo test -p posh <filter>`.

---

### Task 1: Channel identifier + envelope codec

**Promotion criteria:** N/A (new module).

**Files:**
- Create: `crates/posh/src/remote/channel.rs`
- Modify: `crates/posh/src/remote/mod.rs` (add `pub(crate) mod channel;`)
- Test: in-module `#[cfg(test)] mod tests`

**Step 1: Write the failing tests** (in `channel.rs` with stub types so it
compiles; or start with the module containing only tests against the API below
and watch the compile fail, then add stubs and watch assertions fail):

```rust
// API under test:
// ChannelId(u64) — bit 0 initiator (0=client,1=server), bits 1..7 kind,
//   bits 8..63 ordinal. Constructors + accessors + KIND_SESSION=0, KIND_AGENT=1.
// ChannelAllocator::new(Role) — next(kind) yields ordinals 1,2,3… in own
//   initiator space; ChannelId::CONTROL (raw 0) is never yielded.
// Envelope { ver: u8, channel: ChannelId } —
//   encode_to(&mut Vec<u8>) prepends VER_1 (0x01) + u64 LE;
//   Envelope::parse(&[u8]) -> Result<(Envelope, &[u8])> returns payload rest;
//   parse rejects ver != 0x01 and inputs shorter than 9 bytes.

#[test] fn id_partition_roundtrips_initiator_kind_ordinal() { /* build from parts, read back all three; server bit set for server ids */ }
#[test] fn allocator_starts_at_one_and_is_monotonic_per_kind() { /* two kinds interleaved; ordinals independent; never 0 */ }
#[test] fn envelope_roundtrip_prefixes_nine_bytes() { /* encode over b"payload", parse, get same id + payload slice */ }
#[test] fn envelope_rejects_unknown_ver_and_truncation() { /* 0x02 → Err; 8-byte input → Err */ }
#[test] fn control_identifier_is_never_allocated_and_is_rejected_as_data() { /* raw 0: allocator never yields; a helper is_data_channel(id) false */ }
```

**Step 2:** Run `just debug-cargo test -p posh remote::channel` — expect
compile failure, then assertion failures against `todo!()` stubs.

**Step 3:** Implement (LE per §2; keep it dependency-free):

```rust
pub const VER_1: u8 = 0x01;
pub const ENVELOPE_LEN: usize = 9;
pub const KIND_SESSION: u8 = 0;
pub const KIND_AGENT: u8 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChannelId(pub u64);
impl ChannelId {
    pub const CONTROL: ChannelId = ChannelId(0);
    pub fn new(server_initiated: bool, kind: u8, ordinal: u64) -> ChannelId {
        ChannelId((server_initiated as u64) | ((kind as u64 & 0x7f) << 1) | (ordinal << 8))
    }
    pub fn server_initiated(self) -> bool { self.0 & 1 == 1 }
    pub fn kind(self) -> u8 { ((self.0 >> 1) & 0x7f) as u8 }
    pub fn ordinal(self) -> u64 { self.0 >> 8 }
    pub fn is_data(self) -> bool { self.ordinal() != 0 }
}
// Allocator: per-kind next-ordinal table (kind-indexed [u64; 128] is fine),
// role fixed at construction; next(kind) returns ChannelId and bumps.
// Envelope::parse: check len >= 9, check ver, split_at(9), u64 LE from [1..9].
```

**Step 4:** `just debug-cargo test -p posh remote::channel` — all pass.

**Step 5:** Commit (`remote/channel: RFC 0011 §2/§3 envelope + identifier
codec`, sign off as Clown with the build id).

---

### Task 2: Agent-channel payload codec (§5)

**Files:**
- Modify: `crates/posh/src/remote/channel.rs`

**Step 1: Failing tests:**

```rust
// AgentPayload { flags: u8, send_base: u64, recv_ack: u64, data: Vec<u8> }
// FLAG_OPEN=0x01, FLAG_CLOSE=0x02, FLAG_FAIL=0x04.
// encode() → flags,u64 LE,u64 LE,data; decode(&[u8]) -> Result<AgentPayload>;
// decode REJECTS len < 17; unknown flag bits are surfaced (decode succeeds,
// caller checks `has_unknown_bits()` and discards the instruction — §5 says
// ignore-not-guess).
#[test] fn agent_payload_roundtrips_including_empty_data() {}
#[test] fn agent_payload_rejects_truncated_header() {}
#[test] fn agent_payload_flags_unknown_bits_detected() {}
#[test] fn agent_payload_larger_than_retired_247_budget_roundtrips() { /* 4096-byte data */ }
```

**Steps 2–4:** fail → implement → pass (same recipe filter).

**Step 5:** Commit.

---

### Task 3: The §6 selector

**Files:**
- Modify: `crates/posh/src/remote/sshwrap.rs` (remote_command builder,
  ~lines 90–129: append `--channels` when the local side selected the
  envelope)
- Modify: `crates/posh/src/main.rs` (server arg parse ~261–309: accept
  `--channels`, thread a `channels: bool` into `server::run`)
- Modify: `crates/posh/src/remote/server.rs`, `crates/posh/src/remote/client.rs`
  (plumb the flag down to the loops; unused until Task 4)
- Local selection: `POSH_CHANNELS=1` env opt-in read where the client decides
  its bootstrap flags (same place `-A` is decided). Default OFF.

**Step 1:** Failing test: existing sshwrap tests cover remote_command
composition — add
`remote_command_carries_channels_flag_only_when_selected` asserting the flag's
presence/absence, and a main.rs arg-parse test if the parser has a test
surface; otherwise assert via `server::run`'s config struct.

**Steps 2–5:** fail → implement → pass → commit. NOTE (§6): a server invoked
WITHOUT the flag must not change behavior in any way — keep the flag purely
additive plumbing in this task.

---

### Task 4: Enveloped session channel (single session)

**Promotion criteria:** baseline path retired only per RFC 0008 §6's promotion
criterion — not in this increment.

**Files:**
- Modify: `crates/posh/src/remote/server.rs` (send: prepend envelope before
  `fragmenter.make_fragments`; recv: `Envelope::parse` after `assembly.add`,
  dispatch on kind)
- Modify: `crates/posh/src/remote/client.rs` (mirror)
- Test: `crates/posh/src/remote/sync.rs` or a new integration test module —
  drive a loopback enveloped pair.

Contracts:
- The session channel is client-initiated kind 0 ordinal 1; its OPEN-bearing
  instruction is the ordinary first `ClientMessage` (the bootstrap invocation
  is the binding; RFC 0011 §3.3's target-in-OPEN applies when channels open
  against an already-shared connection, which this increment never does).
- Receiver rules: unknown/RESERVED kind ⇒ discard instruction (and CLOSE per
  §3.2 once agent CLOSE machinery exists — for session-only, discard+log);
  `ver != 0x01` ⇒ discard, count it, do NOT tear down; identifier 0 ⇒ discard.
- Baseline mode must remain byte-identical: every envelope call site is gated
  on the Task 3 flag.

**Step 1:** Failing tests: a loopback pair in enveloped mode exchanges
Init→Frame→ack unchanged (assert the decoded ServerFrame equals baseline
run's); a receiver in enveloped mode discards a ver-0x02 instruction and
stays alive; a baseline-mode byte capture contains no 0x01-prefixed envelope
(guard against accidental always-on).

**Steps 2–5:** fail → implement → pass → commit.

---

### Task 5: Agent channels over the envelope (§5)

**Promotion criteria:** CAP_AGENT_* sending retired for enveloped connections
only; baseline connections keep it indefinitely (RFC 0001 registry note).

**Files:**
- Modify: `crates/posh/src/remote/agent.rs` (AgentEndpoint: emit/accept
  per-channel events keyed by ChannelId instead of the u32 record channel —
  keep the u32 path for baseline; a thin adapter maps between them)
- Modify: `crates/posh/src/remote/server.rs` + `client.rs`: on enveloped
  connections, replace `AgentStream`/CAP_AGENT_DATA/ACK carriage with one
  outbox per agent channel: send `AgentPayload` instructions on
  server-allocated kind-1 ids; retransmit unacked tail on the existing RTO
  tick; deliver in offset order; OPEN on first instruction; CLOSE/FAIL
  terminal; FAIL surfaces as closed socket. Enforce MAX_AGENT_CHANNELS as the
  §3.4 per-kind bound. Respect §4.1: cap one agent instruction's data at
  32 KB and send pending session instructions first.
- Test: agent lifecycle tests mirroring
  `remote::agent::tests::channel_open_data_close_lifecycle` but over the
  envelope; a simulated-loss retransmission test; a >247-byte single
  instruction test; an assertion that enveloped connections never emit cap
  ids 6/7/8 (scan the encoded caps table).

**Steps:** per behavior, strictly test-first; commit after each green
(lifecycle, loss/retransmit, FAIL, budget, no-retired-ids). This is the
largest task — expect several commits.

---

### Task 6: Conformance sweep + docs

**Files:**
- Verify every RFC 0011 Conformance Testing bullet that applies to this
  increment has a named test; add any missing.
- Modify: docs/rfcs/0011-multiplexed-datagram-channels.md — mark implemented
  conformance items IMPLEMENTED with test names (follow the §4 precedent).
- Modify: CLAUDE.md "Key design facts" — add one bullet: frames/agent over
  the envelope exist behind `POSH_CHANNELS`/`--channels`, baseline default
  unchanged.
- Update FDR 0014: the wire increment landed; posh#136 remains open pending
  the mux endpoint (M1 of the mux design).

**Final step:** `spinclass merge-this-session` (the hook runs the full gate;
do not run `just` first). Attestation via nothing-but-the-truth as required.
