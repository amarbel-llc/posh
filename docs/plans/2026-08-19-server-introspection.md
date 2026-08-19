# Server Introspection Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use eng:subagent-driven-development to
> implement this plan task-by-task.

**Goal:** A client can always see what build it is connected to, can ask the
far end for its live state from the palette, and a server host can query its
mux peer locally — per `docs/plans/2026-08-19-server-introspection-design.md`.

**Architecture:** Two new RELEASED capability ids on the existing per-frame
caps mechanism: `CAP_SERVER_IDENT` (13; client requests until held, server
attaches while requested — zero steady state once delivered) and
`CAP_SERVER_STATE` (14; the released twin of experimental `CAP_DIAG` 224,
advertised by the client for a bounded window after the palette About command).
The mux daemon requests ident on its heartbeat channel and reports it in
`mux ls`; the agent-only remote answers with one Empty frame. The mux peer
additionally binds a read-only status socket (`agent/mux-<id>.status.sock`)
answering one status line per connection; the server host's `posh ls` reads it.

**Tech Stack:** existing posh caps/frame machinery (posh-proto `caps.rs`),
`remote/{client,server,relay,mux,agent}.rs`, plain unix sockets. No new deps.

**Rollback:** purely additive caps (unknown ids are ignored by contract);
rollback = a client build that stops advertising ids 13/14. The status socket
is independent and best-effort (bind failure logged, never fatal).

**Design refinements vs. the design doc (recorded, still conforming):**
- Ident delivery is client-driven request-until-held (client advertises 13 on
  every message while it holds no ident; server statelessly attaches while
  requested) instead of server-side until-acked bookkeeping. Same reliability
  and zero steady-state cost, less server state. Client re-requests after a
  RESYNC, which covers server restarts behind the same address.
- The palette About view is a one-shot dialog (`session.aboutinfo` composes a
  string), not a persistently open view — so "state while the view is open" becomes:
  the About action arms a request window (`STATE_REQUEST_WINDOW_MS`, tuning
  lever, start 10 s); reports arriving in the window refresh the cached state;
  About renders the cached state with its age. Reopening About refreshes.
- Allocating released id 14 for the state request resolves the shipping half
  of posh#150 for CAP_DIAG: default builds advertise 14, never 224; the server
  answers either id with the same payload (224 stays for old debug-posture
  clients). Note this on #150 when the work merges.

**Conventions for every task:** worktree only; TDD (failing test first);
cheap verification is `just debug-cargo test -p <crate> <filter>`; do NOT run
full `just` (the merge hook is the CI lane). Commit after each task with the
`:clown:` sign-off trailer used by this session's earlier commits.

---

### Task 1: `ServerIdent` block + the two cap ids (posh-proto)

**Files:**
- Modify: `crates/posh-proto/src/caps.rs` (id block ~line 110; payload
  helpers near `ServerDiag`/`encode_server_diag`)

**Step 1: failing tests** — in `caps.rs`'s test module:

```rust
#[test]
fn server_ident_roundtrips_and_rejects_truncation() {
    let ident = ServerIdent {
        version: "0.3.2".into(),
        git_sha: "e76fb8f".into(),
        pid: 4242,
        start_unix_ms: 1_755_000_000_000,
    };
    let bytes = encode_server_ident(&ident);
    assert_eq!(decode_server_ident(&bytes).unwrap(), ident);
    for cut in 0..bytes.len() {
        assert!(decode_server_ident(&bytes[..cut]).is_err(), "cut={cut}");
    }
}

#[test]
fn introspection_cap_ids_are_released_band() {
    assert_eq!(CAP_SERVER_IDENT, 13);
    assert_eq!(CAP_SERVER_STATE, 14);
    assert!(CAP_SERVER_IDENT < 224 && CAP_SERVER_STATE < 224);
}
```

**Step 2:** `just debug-cargo test -p posh-proto server_ident` → FAIL
(unresolved names).

**Step 3: implement.** Ids beside `CAP_COALESCE` (12):

```rust
/// Server identity (RFC 0013): version, git sha, pid, start time. Client
/// entry (empty payload): "send me your identity" — advertised on every
/// message while the client holds no ident, so delivery is reliable with
/// zero steady-state cost once held. Server entry: the encoded
/// [`ServerIdent`], attached to frames while requested.
pub const CAP_SERVER_IDENT: u8 = 13;
/// On-demand server state (RFC 0013): the RELEASED request id for the
/// [`ServerDiag`] payload that experimental [`CAP_DIAG`] carries in a debug
/// posture (posh#150). Client entry (empty payload): "attach your state to
/// frames"; server entry: the `ServerDiag` v2 payload, identical to
/// CAP_DIAG's. Servers answer either request id.
pub const CAP_SERVER_STATE: u8 = 14;
```

`ServerIdent` `{ version: String, git_sha: String, pid: u32, start_unix_ms:
u64 }` with `PartialEq/Debug/Clone`. Encoding (mirror the `MuxHelloAck`
style): `u8` fmt version (=1), `u32` pid LE, `u64` start LE, `u8` version
len + bytes, `u8` sha len + bytes. `decode_server_ident` errors on short
input, bad lengths, or fmt version != 1 (forward compat: reject, requester
just keeps the "unknown" display).

**Step 4:** test passes. **Step 5:** commit
(`proto: ServerIdent block + released introspection cap ids (13/14)`).

---

### Task 2: RFC 0013 + RFC 0001 registry rows

**Files:**
- Create: `docs/rfcs/0013-server-introspection-caps.md` (RFC skeleton per
  the `eng:rfc` shape: normative encoding of the ident payload, the
  request/attach semantics of ids 13/14, the mux heartbeat-channel exchange,
  the 224-compat rule)
- Modify: `docs/rfcs/0001-target-grammar-and-capability-table.md` — add rows
  for ids 13 and 14 citing RFC 0013 (the registry is maintained in place);
  note on the 224 row that its shipping consumers migrate to 14 (posh#150).

No test lane; `just lint-doc` only checks `doc/*.scd`. Commit
(`rfc: RFC 0013 server introspection caps; register ids 13/14`).

---

### Task 3: server answers ident + released state id (roaming server_loop)

**Files:**
- Modify: `crates/posh/src/remote/server.rs` — Init/message cap parsing
  (~line 1491-1497, the `peer_wants_*` block) and the frame-extras site
  that attaches the CAP_DIAG payload (search `CAP_DIAG` near line 1895).

**Step 1: failing test** — beside the existing caps-negotiation tests in
`server.rs` (find the four-way frames matrix for the pattern). Pin: a
`ClientMessage` carrying `CAP_SERVER_IDENT` yields a frame whose caps
contain a decodable `ServerIdent` with `env!("POSH_VERSION")` /
`env!("POSH_GIT_SHA")`; a message carrying `CAP_SERVER_STATE` yields a
frame carrying a `CAP_DIAG`-shaped `ServerDiag` payload under id
`CAP_SERVER_STATE`; a message with neither yields frames with neither
(the skew/steady-state bound).

**Step 2:** run → FAIL. **Step 3: implement.**
- `let mut peer_wants_ident = ...` parsed per-message like `peer_wants_diag`;
  `peer_wants_diag |= find(CAP_SERVER_STATE)` for the state half, but track
  WHICH id was requested so the reply is attached under the id the client
  asked with (an old debug-posture client keeps receiving 224).
- Build the ident payload once at loop start (`ServerIdent { version:
  env!("POSH_VERSION").into(), git_sha: env!("POSH_GIT_SHA").into(), pid:
  std::process::id(), start_unix_ms: <wall clock captured at loop entry> }`)
  and attach while `peer_wants_ident`.

**Step 4:** pass. **Step 5:** commit
(`server: answer CAP_SERVER_IDENT / released CAP_SERVER_STATE requests`).

---

### Task 4: relay answers the same requests

**Files:**
- Modify: `crates/posh/src/remote/relay.rs` — the seam where the relay
  answers `CAP_DIAG`/`CAP_METRICS` from its own transport state (module doc
  ~line 104; search `CAP_DIAG` in relay.rs). This covers `host:session`
  relays AND the M2 mux channel table (which applies the relay contract).

Same TDD shape as Task 3 against the relay's unit harness (`relay.rs` has
`periodic_send`/rewrap tests to model on): ident attached while requested;
state under the requested id. Commit
(`relay: answer introspection caps from the relay's own state`).

---

### Task 5: client requests ident + palette-armed state window

**Files:**
- Modify: `crates/posh/src/remote/client.rs`:
  - `ClientState`: add `server_ident: Option<caps::ServerIdent>` and
    `state_request_until: u64` (0 = unarmed).
  - The per-message extras block (~line 2565, where CAP_DIAG is pushed):
    push `CAP_SERVER_IDENT` while `st.server_ident.is_none()`; push
    `CAP_SERVER_STATE` while `now < st.state_request_until`.
  - Frame-receive path (~line 1969, the CAP_DIAG record site): also decode
    `CAP_SERVER_IDENT` → `st.server_ident`; treat an arriving
    `CAP_SERVER_STATE` payload exactly like CAP_DIAG (update
    `last_server_diag` + a `last_server_diag_at: u64` timestamp for age
    display). Clear `server_ident` on the RESYNC path so it re-requests.
  - `about_summary` (~line 271): add a `remote:` section — `remote: posh
    <ver> (<sha>) pid=<pid> up=<dur>` from `server_ident`, else
    `remote: unknown (pre-introspection server, or not yet delivered)`;
    plus the cached state line with age when present.
  - `dispatch_palette_action` `session.aboutinfo` arm: set
    `st.state_request_until = now + STATE_REQUEST_WINDOW_MS` (const 10_000,
    doc-comment it as the design's tuning lever) BEFORE composing the text.
    NOTE: aboutinfo currently returns `send=false` (client-local); arming
    the window must flip it to request a wire send so the cap goes out
    promptly — mirror how input-bearing actions return send=true.
- Test: extend `about_summary_reports_gates_mode_and_version` + a new test
  that (a) a fresh state advertises 13 in extras, (b) after `server_ident`
  is set it does not, (c) `session.aboutinfo` arms the window and 14 is
  advertised until it expires.

Commit (`client: hold server ident, palette-armed state window, About remote
section`).

---

### Task 6: mux daemon learns the remote's ident

**Files:**
- Modify: `crates/posh/src/remote/mux.rs`:
  - `heartbeat_message()` → parameterize: carry
    `caps: vec![Cap { id: CAP_SERVER_IDENT, payload: vec![] }]` while the
    daemon holds no remote ident (plumb an `Option<&ServerIdent>` or a bool
    from `mux_loop`).
  - `mux_loop`: on an inbound SESSION_CHANNEL (ordinal 1) instruction —
    today ignored — try `ServerFrame::decode` and lift a
    `CAP_SERVER_IDENT` payload into `remote_ident: Option<ServerIdent>`.
  - `MuxStatusCtx`/`status_line`: add `remote=` (version+sha or `unknown`).
- Modify: `crates/posh/src/remote/server.rs` `mux_peer_loop`: the
  session-kind discard arm (~line 420) — before discarding, decode the
  `ClientMessage`; if it carries `CAP_SERVER_IDENT`, reply once per request
  sighting with an Empty `ServerFrame` on `SESSION_CHANNEL` whose caps carry
  the ident payload (reuse `empty_ack_frame(0, 0)` + push the cap).
- Test: extend the in-process daemon harness
  (`start_inprocess_daemon` / the `m1`/`m2` tests in mux.rs): drive a real
  `agent_only_loop` peer, wait for `ipc_status` to contain `remote=` with
  `env!("POSH_GIT_SHA")`.

Commit (`mux: heartbeat ident exchange — mux ls shows the remote build`).
This is the positive round-trip probe posh#162 builds on; say so in the
commit body with `Refs #162`.

---

### Task 7: mux peer status socket

**Files:**
- Modify: `crates/posh/src/remote/agent.rs`: `AgentEndpoint::new_mux` binds
  `agent/mux-<id>.status.sock` (+ writes `mux-<id>.status.pid` BEFORE the
  bind, mirroring the write-before-bind ordering so `gc_dead_sockets`
  reaps a crashed peer's leftovers via the existing stem/pid rules —
  verify `sock_stem("mux-x.status.sock") == "mux-x.status"` parses and add
  a test); Drop unlinks both. Expose `status_listener(&self)`.
  Bind failure: `log_write("warn", ...)`, endpoint construction still Ok.
- Modify: `crates/posh/src/remote/server.rs` `mux_peer_loop`: poll the
  status listener; on accept, write ONE line (ident + peer + heard-age +
  live/cumulative agent channels + `agent/sock` ownership + session-channel
  count — the SIGUSR2 line plus ident) and close. No read, no framing:
  connect-read-EOF protocol.
- Tests: agent.rs lifecycle test (bind, GC of a dead peer's status files,
  Drop unlink) + a mux.rs harness assertion connecting to the status socket
  and reading a line containing `env!("POSH_GIT_SHA")`.

Commit (`agent: mux-peer status socket (connect → one line → EOF)`).

---

### Task 8: `posh ls` remote-endpoints section (server host)

**Files:**
- Modify: `crates/posh/src/remote/mux.rs` (or a sibling helper): a
  `status_ls()` scanning `<base>/agent/*.status.sock`, connect with a
  ~200 ms timeout (tuning lever — doc-comment it), read the line; label
  no-answer sockets `stale`.
- Modify: the unified `posh list` renderer (main.rs — find the seam via
  `MUX_LS_EMPTY`, the #158 mux-endpoints section): append a
  `remote endpoints:` block when any status sockets exist. Interactive
  view only — `--short`/`--json`/piped output stay session-only by the
  #158 contract (pin that in the test).
- Test: model on `mux_ls_reports_live_stale_and_empty`.

Commit (`list: remote-endpoints section from mux-peer status sockets`).

---

### Task 9: docs

**Files:**
- Modify: `doc/posh.1.scd` (`mux ls` gains `remote=`; `list` gains the
  remote-endpoints note), `doc/posh-server.1.scd` (`agent` verb: status
  socket + ident answering), FDR 0009's palette command list is untouched
  (About exists) but FDR 0007 gains one line (ident in the About/remote
  surface). Run `just lint-doc`.
- AGENTS.md: one bullet in the debugging section (`posh ls` on the server
  host now answers the twerk-connection-health question locally).

Commit (`docs: server introspection surfaces`).

---

### Task 10: merge

Attest via `nothing-but-the-truth` (all five skills, honestly), then
`merge-this-session-async`. The pre-merge hook runs the full gate — do not
run `just` beforehand. On the wake, note on posh#150 that shipping state
requests now ride released id 14, and on posh#162 that the round-trip probe
primitive landed.
