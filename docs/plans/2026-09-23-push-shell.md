# Push Shell Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use eng:subagent-driven-development to implement this plan task-by-task.

**Goal:** Replace FDR 0008's escape-to-shell overlay with *push shell* — an
ordinary FDR 0016 stack push of a new anonymous session, created by the parent
session's daemon in its cwd, running `$POSH_ESCAPE_CMD`.

**Architecture:** The client asks its session daemon for a push shell with a
deduplicated request (RFC 0016). The daemon creates a sibling anonymous session
in its own OSC-7 cwd and re-homes the *requesting* viewport onto it through the
existing FDR 0012 / RFC 0008 §3.1 `Tag::Switch`. The viewport's
`Event::Entered` pushes the parent; the sibling ending triggers the existing
default pop and its must-dismiss notice. The overlay remains, unchanged, as the
fallback wherever push shell is not offered, until the posh#213 cutover.

**Tech Stack:** Rust (`crates/posh`, `crates/posh-proto`), the RFC 0001
capability table, the session daemon's IPC.

**Rollback:** Push shell is gated on a daemon offer. Removing the client's
advertisement (`CAP_PUSH_SHELL` on Init / messages) makes every daemon silent,
so every palette falls back to *Shell out* — the overlay, unchanged. No wire
state persists.

**Records:** FDR 0020 (feature), RFC 0016 (wire contract), both `proposed`
until the code lands. FDR 0008 flips to `superseded` in the commit that lands
push shell. Design decisions: the 2026-09-23 grill (summarized in FDR 0020
§Decisions).

---

## Settled decisions (do not re-litigate)

1. The **parent daemon** creates the sibling, not the client.
2. The sibling is `SessionKind::Anonymous`, with **no push-shell marker**. No
   rule anywhere special-cases it.
3. **Every pop is announced**, a clean exit included.
4. The notice carries, for the ended session, its **cause and numeric status**
   and its last **activity label** (RFC 0013 §5 lexicon). Cascade entries carry
   `gone` plus the activity label recorded when they were pushed.
5. Push shell is offered only where the daemon offers it **and** the transport
   carries the re-home (local attach, M2 channel). The **overlay is the
   fallback** everywhere else: an old daemon, the relay, Architecture A.
6. The command is `$POSH_ESCAPE_CMD`, unchanged. Its name is revisited at the
   posh#213 cutover.
7. The palette shows **exactly one** entry: *Push shell* when offered, else
   *Shell out*.
8. Lifetime is not tied to the parent; nesting pushes again; the sibling is
   listed, pickable and titled like any session; it gets `POSH_SESSION`,
   `POSH_GROUP`, `TERM`; it is per-viewport.

## Facts the plan relies on (verified 2026-09-23)

- Unknown IPC tags are **skipped** by `FrameBuffer::next`
  (`session/ipc.rs`, pinned by `unknown_tag_skipped`); unknown capability
  entries are skipped by every table reader.
- Both bridges forward a client's capability entries to the daemon only
  through `relay::forwarded_client_caps` (`remote/relay.rs:230`), an explicit
  allow-list (ids 15–18). **The relay must never gain the new ids** — it cannot
  report a re-home to the viewport (ADR 0007). The M2 bridge gets its own
  addition in `bridge_client_message` (`remote/server.rs:1136`).
- The daemon attaches `CAP_SESSION_KIND` once, beside the first activity
  answer (`session/daemon.rs:389-401`). The offer copies that pattern.
- FDR 0012's router `switch_route_target` **excludes** the requester
  (`session/daemon.rs:901`) because there the requester is a separate
  `posh attach` process. A push shell's requester **is** the viewport, so the
  daemon queues `Tag::Switch` on the requester's own connection.
- A daemon is created by `daemon::ensure_session` (bind, `double_fork`,
  `daemon_main`) and inherits its creator's cwd and environment. Creators seed
  `SSH_AUTH_SOCK` into their own environment before forking
  (`remote/server.rs:210`, `remote/relay.rs:431-437`), so a sibling forked from
  the parent daemon is born on the same agent socket.
- **Trap:** a sibling forked *from a daemon* inherits the parent's client
  sockets, listener, and **PTY master**. It must `util::close_inherited_fds`
  (keeping only its own listener) or it keeps the parent's terminal alive.
- `session::next_autoid(cfg)` (`session/mod.rs:975`, `pub(crate)`) names it
  `s-N`.
- `posh start :+` typed *inside* a session already creates and re-homes
  (`cmd_start_local` → `switch_in_place`, posh#183): the stack path push shell
  relies on is exercised today.
- Next free capability ids: **21, 22**.

---

### Task 1: The RFC 0016 capability entries

**Promotion criteria:** N/A (additive).

**Files:**
- Modify: `crates/posh-proto/src/caps.rs` (after `CAP_SESSION_KIND`, line 167;
  codecs beside `encode_session_kind`, line 453)
- Test: same file, `mod tests`

**Step 1: Write the failing tests**

```rust
#[test]
fn push_shell_ids_are_the_next_free_pair() {
    assert_eq!(CAP_PUSH_SHELL, 21);
    assert_eq!(CAP_PUSH_SHELL_REQUEST, 22);
}

#[test]
fn push_shell_request_token_roundtrips_and_rejects_bad_lengths() {
    let cap = encode_push_shell_request(0x0123_4567_89ab_cdef);
    assert_eq!(cap.id, CAP_PUSH_SHELL_REQUEST);
    assert_eq!(decode_push_shell_request(&cap.payload), Some(0x0123_4567_89ab_cdef));
    assert_eq!(decode_push_shell_request(&[0; 7]), None);
    assert_eq!(decode_push_shell_request(&[0; 9]), None);
    assert_eq!(decode_push_shell_request(&[0; 8]), None, "token 0 is reserved as 'none'");
}

#[test]
fn push_shell_offer_is_an_empty_entry() {
    let cap = encode_push_shell();
    assert_eq!(cap.id, CAP_PUSH_SHELL);
    assert!(cap.payload.is_empty());
}
```

**Step 2: Run to verify they fail**

Run: `just debug-cargo test -p posh-proto push_shell`
Expected: FAIL to compile (`CAP_PUSH_SHELL` not found).

**Step 3: Implement**

```rust
/// RFC 0016 §2: push shell, as a request/answer pair like RFC 0013 §5.2's
/// activity label. Client → server (empty): "I can push a shell". Server →
/// client (empty), once, beside the first activity answer: "this daemon
/// offers push shell". Only a local attach and the M2 bridge carry the client
/// half; the relay never does, so a relayed viewport is never offered one.
pub const CAP_PUSH_SHELL: u8 = 21;

/// RFC 0016 §3: client → server, a push-shell request carrying a nonzero u64
/// token (big-endian). The client repeats it until the re-home arrives; the
/// daemon serves each token once, and hands it to the sibling it creates so a
/// repeat that lands there after the re-home is ignored too.
pub const CAP_PUSH_SHELL_REQUEST: u8 = 22;

pub fn encode_push_shell() -> Cap {
    Cap { id: CAP_PUSH_SHELL, payload: Vec::new() }
}

pub fn encode_push_shell_request(token: u64) -> Cap {
    Cap { id: CAP_PUSH_SHELL_REQUEST, payload: token.to_be_bytes().to_vec() }
}

/// `None` for anything but exactly 8 bytes, and for token 0.
pub fn decode_push_shell_request(payload: &[u8]) -> Option<u64> {
    let token = u64::from_be_bytes(payload.try_into().ok()?);
    (token != 0).then_some(token)
}
```

**Step 4: Run to verify they pass** — same command; Expected: PASS.

**Step 5: Commit** — `posh-proto: RFC 0016 push-shell capability entries (ids 21, 22)`.
Update RFC 0001's registry table **in place** in this commit (rows 21 and 22,
citing RFC 0016).

---

### Task 2: The notice carries status and activity label

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/picker.rs` — `StackEntry` (line 145), `Event`
  (add `ActivityReported(String)`), `Current` (add `activity: String`),
  `current_entry_of`, `apply`, `AttachEnd::label` (line 625)
- Modify: `crates/posh-proto/src/caps.rs` — `SessionEnd::label` (line 186)
- Modify: `crates/posh/src/remote/palette_view.rs` — `notice_stack`
- Test: the `mod tests` of each

**Step 1: Write the failing tests**

```rust
// picker.rs
#[test]
fn a_pushed_entry_carries_the_activity_label_it_was_left_with() {
    let mut s = ViewportState::new();
    apply(&mut s, Event::Entered { target: ":a".into() });
    apply(&mut s, Event::ActivityReported("notes.md · nvim".into()));
    apply(&mut s, Event::Entered { target: ":b".into() });
    assert_eq!(s.stack[0].activity, "notes.md · nvim");
}

#[test]
fn the_notice_carries_the_ended_sessions_activity_label() {
    let mut s = st(&[(":below", SessionKind::Named)], Some(":top"));
    apply(&mut s, Event::ActivityReported("cargo build".into()));
    apply(&mut s, Event::AttachEnded(ended(0)));
    apply(&mut s, Event::AttachReturned);
    apply(&mut s, Event::Entered { target: ":below".into() });
    let n = s.notice.take().expect("every pop is announced, a clean exit too");
    assert_eq!(n.gone[0].activity, "cargo build");
}

// caps.rs — a killed or signaled end states its numeric status too
#[test]
fn every_end_label_carries_the_numeric_status() {
    assert_eq!(SessionEnd::Exited.label(0), "ended (exit 0)");
    assert_eq!(SessionEnd::Killed.label(143), "killed (posh kill, status 143)");
    assert_eq!(SessionEnd::Signaled(15).label(143), "ended (daemon got SIGTERM, status 143)");
    assert_eq!(SessionEnd::Failed.label(1), "ended (daemon failed, status 1)");
}

// palette_view.rs
#[test]
fn a_notice_entry_joins_its_story_and_activity_label() {
    let mut n = pop_notice(&["box:top", "box:mid"], "box:dev", &[]);
    n.gone[0].activity = "cargo build".into();
    n.gone[1].activity = "vim".into();
    let stack = notice_stack(&n);
    assert_eq!(stack[0]["detail"], "ended (exit 1) · cargo build");
    assert_eq!(stack[1]["detail"], "gone · vim");
}
```

**Step 2: Run to verify they fail** — `just debug-cargo test -p posh -- picker palette_view`
and `just debug-cargo test -p posh-proto end_label`. Expected: compile errors
(`activity` field, `ActivityReported`).

**Step 3: Implement**

- `StackEntry { target, kind, activity: String }` — empty when never reported.
- `Current` gains `activity: String`; `Event::ActivityReported(label)` sets it
  (a no-op without a current attach; an **empty** label never overwrites a
  known one, mirroring `KindReported`'s never-downgrade rule) and returns
  `[]` — the status socket does not show it, so no `RefreshStatus`.
- `current_entry_of` copies `activity` into the entry, so a push records it
  and `Cascade.gone[0]` carries it.
- `SessionEnd::label`: every non-`Exited` arm appends `, status {exit_code}`
  inside the parentheses. Update `attach_end_labels_say_what_happened`
  (picker.rs) to the new strings.
- `notice_stack`: `detail` = the story (`ended.label()` for `gone[0]`, `gone`
  for the rest) and, when the entry's activity label is non-empty,
  `" · {activity}"`.
- Fix every `StackEntry { .. }` literal the compiler flags (tests, `stacked`,
  `no_stack`, `palette_view` helpers) with `activity: String::new()`.

**Step 4: Run to verify they pass** — same commands; Expected: PASS, and the
existing notice and palette tests still pass.

**Step 5: Commit** — `posh: the pop notice carries status and activity label`.

---

### Task 3: Clients report the activity label to the reducer

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/picker.rs` — add `pub fn activity_reported(label: &str)`
  (one `dispatch(Event::ActivityReported(..))`, like `note_attach_end`)
- Modify: `crates/posh/src/remote/client.rs` — where a frame's
  `CAP_SESSION_ACTIVITY` sets `st.session_activity` (search
  `session_activity =`)
- Modify: `crates/posh/src/session/client.rs` — its activity handling beside
  the `CAP_SESSION_KIND` read (line 701)

**Steps:** TDD against the reducer is Task 2's; here, add one client-level
test per client that a frame carrying an activity entry leaves
`picker::stack_view()`'s next push holding that label (use
`picker::switch_test_guard()` and `picker::reset_for_test` as the existing
client tests do). Call `picker::activity_reported(&activity.label())` at the
point each client stores the label. Run
`just debug-cargo test -p posh -- session::client remote::client`. Commit:
`posh: clients report the activity label to the viewport reducer`.

---

### Task 4: The daemon creates a sibling in a given cwd

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/session/daemon.rs` — add `spawn_push_shell`;
  thread a `served: Vec<u64>` seed into `daemon_main` (initial served tokens)
- Test: `crates/posh/tests/session_integration.rs`

**Step 1: Write the failing test** (integration — it forks a real daemon)

```rust
#[test]
fn a_push_shell_sibling_starts_in_the_given_cwd_with_the_escape_command() {
    // Isolated POSH_DIR, like the other integration tests. POSH_ESCAPE_CMD
    // prints the cwd and the session name, then sleeps so the test can read.
    // Assert: a new `s-N` session exists, its kind is anonymous, its output
    // shows the requested cwd and POSH_SESSION=s-N, and the PARENT's pty is
    // unaffected — killing the parent's shell still ends the parent (proves
    // the sibling did not inherit the parent's PTY master).
}
```

Write it concretely against the helpers already in `session_integration.rs`
(read them first; do not invent a harness).

**Step 2: Run** — `just debug-cargo test -p posh --test session_integration push_shell`;
Expected: FAIL (no `spawn_push_shell`).

**Step 3: Implement**

```rust
/// RFC 0016 §4: create the push-shell sibling — an anonymous session in
/// `cwd` running `$POSH_ESCAPE_CMD`, seeded with `token` as already served —
/// and return its name. Called from INSIDE a daemon, so the grandchild must
/// shed every descriptor it inherited from this one (client sockets, the
/// listener, and above all this session's PTY master, which would otherwise
/// keep the parent's terminal alive past its own death).
pub(crate) fn spawn_push_shell(cfg: &Config, cwd: &str, token: u64) -> Result<String> {
    let name = session::next_autoid(cfg)?;
    let path = cfg.socket_path(&name)?;
    let listener = UnixListener::bind(&path)
        .map_err(|e| Error::Msg(format!("bind {}: {e}", path.display())))?;
    if util::double_fork()? {
        drop(listener);
        return Ok(name);
    }
    util::close_inherited_fds(&[listener.as_raw_fd()]);
    if std::env::set_current_dir(cwd).is_err() {
        let _ = std::env::set_current_dir(std::env::var("HOME").unwrap_or_else(|_| "/".into()));
    }
    daemon_main(cfg, &name, listener, crate::overlay::escape_command(), SessionKind::Anonymous, vec![token]);
}
```

`daemon_main` gains the `served: Vec<u64>` parameter; every existing caller
(`ensure_session`) passes `Vec::new()`. The cwd fallback order matches the
overlay's (OSC-7 pwd, else the daemon's own cwd — decided by the caller in
Task 5 — else `$HOME`).

**Step 4: Run** — same command; Expected: PASS. Also run
`just debug-cargo test -p posh --test session_integration` in full (it
exercises `ensure_session`).

**Step 5: Commit** — `posh: a daemon can create a sibling session in a given cwd`.

---

### Task 5: The daemon offers push shell and serves requests

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/session/daemon.rs` — `ClientConn` (add
  `wants_push_shell: bool`, `push_offered: bool`), `absorb_client_caps`
  (line 230), the frame-cap assembly (lines 389-401), the per-client loop's
  deferred actions (beside `open_shell`, line 1509), and a served-token set
  in `daemon_main`'s loop state

**Step 1: Write the failing tests** (unit, in `daemon.rs`'s `mod tests`)

```rust
#[test]
fn push_shell_is_offered_once_beside_the_first_activity_answer_to_a_client_that_asks() {
    // Mirror the CAP_SESSION_KIND test (daemon.rs:3368): a client whose table
    // carries CAP_PUSH_SHELL and CAP_SESSION_ACTIVITY gets CAP_PUSH_SHELL on
    // its first activity frame and never again; a client that did not ask
    // never gets it.
}

#[test]
fn a_push_request_is_served_once_per_token() {
    // Pure helper `fn take_push_request(table, served: &mut Vec<u64>) -> Option<u64>`:
    // the first table with token 7 yields Some(7) and records it; the same
    // token again yields None; token 8 yields Some(8); a malformed entry
    // yields None. A seed of [7] (a sibling's) makes 7 None from the start.
}
```

**Step 2: Run** — `just debug-cargo test -p posh -- session::daemon::tests::push`;
Expected: FAIL.

**Step 3: Implement**

- `absorb_client_caps`: `CAP_PUSH_SHELL` present latches `wants_push_shell`.
- Frame-cap assembly: beside the `kind_sent` block, if `wants_push_shell &&
  !push_offered`, set `push_offered = true` and push `caps::encode_push_shell()`.
- Request handling runs where the daemon reads a table — the Init table and
  `Tag::ClientCaps` — through `take_push_request(table, &mut served)`. A
  served token sets a deferred `push_shell_for: Option<u64>` on that client's
  pass. After the per-client borrow (beside the `open_shell` block,
  line 1803): compute the cwd exactly as the overlay does (`term.pwd()`, else
  the daemon's `cwd`), call `spawn_push_shell(cfg, &cwd, token)`, and queue
  `Tag::Switch` with `ipc::encode_switch_target(&cfg.group, &name)` on
  **`clients[i]` — the requester**, never `switch_route_target`. On error, log
  and send nothing: the request goes unanswered, and Task 7's client gives up
  after its bound and says so (RFC 0016 §4, §5). It does NOT fall back to the
  overlay — RFC 0016 §5 forbids sending both.
- The served set is bounded (keep the last 64 tokens); a token is 64 random
  bits, so collisions are not a concern.

**Step 4: Run** — same; Expected: PASS. Then the daemon's whole test module.

**Step 5: Commit** — `posh: the daemon offers push shell and serves each request once`.

---

### Task 6: The M2 bridge carries push shell; the relay does not

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/remote/server.rs` — `bridge_client_message`
  (line 1136)
- Test: `server.rs` `mod tests`, beside the posh#178 escape test (line 2961)

**Step 1: Write the failing tests**

```rust
#[test]
fn the_m2_bridge_forwards_push_shell_entries_to_the_daemon() {
    // Like the CLIENT_FLAG_ESCAPE -> Tag::Shell test at server.rs:2961: a
    // ClientMessage carrying CAP_PUSH_SHELL and CAP_PUSH_SHELL_REQUEST(9)
    // yields a Tag::ClientCaps whose table holds both.
}

#[test]
fn the_relay_never_forwards_push_shell() {
    // relay::forwarded_client_caps drops ids 21 and 22 — pinned so a future
    // "tidy" into the shared allow-list is caught. The relay cannot report a
    // re-home to its viewport (ADR 0007).
    let table = vec![caps::encode_push_shell(), caps::encode_push_shell_request(9)];
    assert!(crate::remote::relay::forwarded_client_caps(&table).is_empty());
}
```

**Step 2: Run** — `just debug-cargo test -p posh -- push_shell`; Expected:
the bridge test FAILS, the relay test PASSES (it pins today's behavior).

**Step 3: Implement** — in `bridge_client_message`, extend `forwarded` with
the message's `CAP_PUSH_SHELL` / `CAP_PUSH_SHELL_REQUEST` entries (a local
filter; do **not** touch `relay::forwarded_client_caps`). The re-home back is
already handled: the daemon's `Tag::Switch` reaches the bridge's daemon link,
which re-homes and sends `SESSION_WIRE_SWITCH` (`server.rs:783-797`).

**Step 4: Run** — same; Expected: both PASS.

**Step 5: Commit** — `posh: the M2 bridge carries push shell (the relay does not)`.

---

### Task 7: Clients offer exactly one entry and send the request

**Promotion criteria:** the overlay path is removed at the posh#213 cutover,
not here.

**Files:**
- Modify: `crates/posh/src/remote/client.rs` — the Init / per-message caps
  (add `CAP_PUSH_SHELL`), frame-cap reading (record `push_shell_offered`),
  `palette_commands` (line 305), `dispatch_palette_action` (`shell.open`,
  line 1013; add `shell.push`), the re-home handling that clears the pending
  token
- Modify: `crates/posh/src/session/client.rs` — the same set, local shape:
  Init caps, frame-cap reading, `palette_commands` (line ~1028),
  `dispatch_local_action` (line 1114; add `shell.push` → a `Tag::ClientCaps`
  frame carrying the request)

**Step 1: Write the failing tests**

```rust
// both clients
#[test]
fn the_palette_offers_push_shell_when_the_daemon_does_and_shell_out_otherwise() {
    // palette_commands(.., push_offered = true) contains "Push shell"
    // (shell.push) and NOT "Shell out"; with false, the reverse.
}

// remote client
#[test]
fn a_push_request_rides_every_message_until_the_rehome_lands() {
    // dispatch "shell.push" sets a pending nonzero token; outgoing message
    // caps carry CAP_PUSH_SHELL_REQUEST(token) on each send; a Switched event
    // clears it, and the next message no longer carries it.
}

// local client
#[test]
fn dispatch_shell_push_queues_one_client_caps_request() {
    // like dispatch_shell_open_appends_a_shell_frame (session/client.rs:2854)
}
```

**Step 2: Run** — `just debug-cargo test -p posh -- palette_offers push_request shell_push`;
Expected: FAIL.

**Step 3: Implement**

- Advertise `CAP_PUSH_SHELL` wherever each client already sends
  `CAP_SESSION_ACTIVITY` (Init, and per message for the roaming client).
- A frame carrying `CAP_PUSH_SHELL` sets `push_shell_offered` (sticky for the
  attach).
- `palette_commands` takes `push_offered` and emits exactly one of
  `{"name": "Push shell", "action": {"method": "shell.push"}}` /
  the existing *Shell out* row.
- `shell.push`: mint a random nonzero u64. Local: append one `Tag::ClientCaps`
  frame with `encode_push_shell_request(token)` (IPC is reliable — one send).
  Roaming: hold it in `ClientState::pending_push`, attach it to every message
  until the re-home (`MuxSessionEvent::Switched`), then clear. Show the
  existing notice line `opening shell…`, cleared by the re-home.
- Bound the wait (RFC 0016 §5): if no re-home arrives within 10 s, drop the
  pending token and replace the notice with `push shell: no answer from the
  session`. Test it by passing `now` explicitly, as `dispatch_palette_action`
  already takes it.
- The re-home itself needs nothing new: the local client already re-dials on
  `Tag::Switch` and calls `picker::entered`; the roaming client records
  `Switched` (defect B). `Entered` pushes the parent.

**Step 4: Run** — same; Expected: PASS. Then
`just debug-cargo clippy -p posh --all-targets -- -D warnings`.

**Step 5: Commit** — `posh: push shell in the palette, with the overlay as the fallback`.

---

### Task 8: A live verification recipe

**Files:** Modify `justfile` (debug group, beside `debug-verify-escape`).

Add `debug-verify-push-shell`: over a local loopback pair (copy
`debug-verify-escape`'s shape), attach, trigger *Push shell*, run `pwd; echo
$POSH_SESSION` in the pushed shell, `exit`, and assert the parent's screen is
back with the pop notice up (`ended (exit 0)`). Its comment block documents
what it proves and the signature that means failure. Run it; paste its output
in the commit message. Commit: `justfile: debug-verify-push-shell`.

---

### Task 9: Records move with the code

**Files:**
- `docs/features/0020-push-shell.md` → `status: experimental`
- `docs/features/0008-escape-to-shell.md` → `status: superseded`, naming
  FDR 0020 in a header note; body states the overlay remains as the fallback
  until posh#213
- `docs/rfcs/0016-push-shell-request.md` → `status: experimental`
- `docs/rfcs/0001-target-grammar-and-capability-table.md` — rows 21, 22 (if
  not already landed in Task 1)
- `doc/posh-client.1.scd` — the palette entry (search `Shell out`)
- `docs/features/0016-cross-host-session-switcher.md` — the notice now carries
  status and activity label for every pop

Run `just lint-doc`. Commit in the SAME commit as the last code task, per
`docs/README.md` ("status moves in the commit that moves the code") — so fold
this into Task 7's commit, or land Tasks 7 and 9 together.

---

## Out of scope

- Deleting the overlay (`overlay.rs`, `Tag::Shell`, `CLIENT_FLAG_ESCAPE`,
  `FLAG_OVERLAY`, the bridge shims): the posh#213 cutover.
- Renaming `$POSH_ESCAPE_CMD`: the posh#213 cutover.
- Stack management and killing (FDR 0016 v2).
