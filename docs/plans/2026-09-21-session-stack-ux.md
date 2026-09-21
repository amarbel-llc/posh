# Session Stack UX Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use eng:subagent-driven-development to implement this plan task-by-task.

**Goal:** Give every posh session a daemon-owned kind (anonymous / named / system), make the viewport the registry for its own session stack, show the predecessor in the palette heading, and ask on detach what to do with anonymous sessions the viewport created.

**Architecture:** Section 1 adds a `SessionKind` to `posh_proto`, appends it skew-tolerantly to the daemon's `SessionInfo` record, and threads it from `posh start` / `posh attach --create` into `ensure_session`; a remote auto-id create becomes a remote-atomic `POSH_HANDSHAKE=1 posh start --detach --kind anonymous` over ssh that answers with a `POSH START 1 <name> <kind>` handshake line (the go-plugin / `POSH CONNECT` shape: one structured stdout line before anything else, version field first; the env prefix is the magic cookie and keeps the pinned prose unchanged for every other caller), followed by an ordinary attach, so no roaming wire changes. The space-separated `POSH …` family is kept for now; a pipe-delimited cutover is tracked as posh#202. Section 2 types the front door's stack with the kind and binds a per-viewport status socket. Section 3 splits a `StackView` schema from a `palette_view` presentation module. Section 4 adds the `POSH_LEAVE_ANONYMOUS` prompt in the front door's re-attach loop.

**Tech Stack:** Rust (Cargo workspace: `crates/posh`, `crates/posh-proto`), hand-rolled IPC framing (`session/ipc.rs`), RFC 0005 JSON palette control, scdoc man pages, nix build (`just build-rust` / `just debug-cargo test -p posh`).

**Rollback:** Every section is additive. Section 1: an old client ignores the trailing kind byte, an old daemon reads `unknown`, `--kind` is optional and defaults to `named`, and the remote auto-create falls back to the pre-existing probe-then-attach path when the remote rejects `--kind`. Section 2: `POSH_VIEWPORT_STATUS=0` skips the socket bind. Section 3: presentation only. Section 4: `POSH_LEAVE_ANONYMOUS=keep` disables the prompt.

**Design:** `docs/plans/2026-09-21-session-stack-ux-design.md` (approved 2026-09-21).

**Sequencing note:** Sections 2 through 4 depend on Section 1's types and on the exact shape Section 2's socket takes. Their tasks below are scoped (files, tests, behavior) but NOT yet broken into 2-to-5-minute steps. Expand each into steps of the Section 1 shape when its predecessor has merged; do not start a later section from this outline alone.

**Dev loop:** `just debug-cargo test -p posh <test_name>` for a single test, `just debug-cargo test -p posh-proto` for the proto crate. `merge-this-session` runs the full `just` gate; do not run `just` by hand before merging. Commit after every green step; the pre-commit hook formats (conformist).

---

## Section 1: daemon session kind

### Task 1: `SessionKind` in posh-proto

**Promotion criteria:** N/A (new type).

**Files:**
- Modify: `crates/posh-proto/src/caps.rs` (append after `SessionActivity`, around line 302)
- Test: same file, `mod tests`

**Step 1: Write the failing test**

Append to the `tests` module at the bottom of `crates/posh-proto/src/caps.rs`:

```rust
#[test]
fn session_kind_byte_roundtrip_and_unknown() {
    for k in [SessionKind::Anonymous, SessionKind::Named, SessionKind::System] {
        assert_eq!(SessionKind::from_byte(k.to_byte()), k);
    }
    assert_eq!(SessionKind::from_byte(0), SessionKind::Unknown);
    assert_eq!(SessionKind::from_byte(200), SessionKind::Unknown);
    assert_eq!(SessionKind::Unknown.to_byte(), 0);
}

#[test]
fn session_kind_names_are_the_cli_and_json_spellings() {
    assert_eq!(SessionKind::Anonymous.as_str(), "anonymous");
    assert_eq!(SessionKind::Named.as_str(), "named");
    assert_eq!(SessionKind::System.as_str(), "system");
    assert_eq!(SessionKind::Unknown.as_str(), "unknown");
    assert_eq!(SessionKind::parse("anonymous"), Some(SessionKind::Anonymous));
    assert_eq!(SessionKind::parse("named"), Some(SessionKind::Named));
    assert_eq!(SessionKind::parse("system"), None, "system is reserved, not creatable");
    assert_eq!(SessionKind::parse("bogus"), None);
}
```

**Step 2: Run test to verify it fails**

Run: `just debug-cargo test -p posh-proto session_kind`
Expected: FAIL, `cannot find type SessionKind`.

**Step 3: Write minimal implementation**

Add after the `SessionActivity` struct in `caps.rs`:

```rust
/// What a session IS, as its creator stated at create time and the daemon
/// stores for the session's life (design: docs/plans/2026-09-21-session-
/// stack-ux-design.md §1). `Anonymous` is a `:+` / picker create-new
/// auto-id session; `Named` is one the user named (the default for every
/// caller that says nothing); `System` is reserved for a daemon-owned
/// session hosting a tool instead of a shell (nothing creates one yet).
/// `Unknown` is the reading of a daemon that predates the field. A
/// session never changes kind; naming is never consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionKind {
    #[default]
    Unknown,
    Anonymous,
    Named,
    System,
}

impl SessionKind {
    /// The one-byte wire form (`0` unknown, `1` anonymous, `2` named,
    /// `3` system): the IPC `SessionInfo` tail and the on-frame activity
    /// entry both carry it.
    pub fn to_byte(self) -> u8 {
        match self {
            SessionKind::Unknown => 0,
            SessionKind::Anonymous => 1,
            SessionKind::Named => 2,
            SessionKind::System => 3,
        }
    }

    /// Any byte outside the known set reads `Unknown` (a newer origin).
    pub fn from_byte(b: u8) -> SessionKind {
        match b {
            1 => SessionKind::Anonymous,
            2 => SessionKind::Named,
            3 => SessionKind::System,
            _ => SessionKind::Unknown,
        }
    }

    /// The CLI (`--kind`) and JSON (`"kind"`) spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Unknown => "unknown",
            SessionKind::Anonymous => "anonymous",
            SessionKind::Named => "named",
            SessionKind::System => "system",
        }
    }

    /// Parse a `--kind` value. Only the CREATABLE kinds parse: `system` is
    /// reserved and `unknown` is never stated.
    pub fn parse(s: &str) -> Option<SessionKind> {
        match s {
            "anonymous" => Some(SessionKind::Anonymous),
            "named" => Some(SessionKind::Named),
            _ => None,
        }
    }
}
```

**Step 4: Run test to verify it passes**

Run: `just debug-cargo test -p posh-proto session_kind`
Expected: 2 passed.

**Step 5: Commit**

```bash
git add crates/posh-proto/src/caps.rs
git commit -m "posh-proto: SessionKind (anonymous/named/system) with byte and string forms"
```

---

### Task 2: `SessionInfo` carries the kind

**Promotion criteria:** N/A (additive wire tail).

**Files:**
- Modify: `crates/posh/src/session/ipc.rs:305-395` (struct, `encode`, `decode`)
- Test: `crates/posh/src/session/ipc.rs` `mod tests` (the five existing `info_*` tests construct `SessionInfo` literally and must gain `kind: SessionKind::Unknown`, or use `..Default::default()` after Step 3 derives `Default`)

**Step 1: Write the failing tests**

Add to the `tests` module:

```rust
#[test]
fn info_kind_roundtrip() {
    let info = SessionInfo {
        clients: 0,
        pid: 1,
        cmd: "bash".to_string(),
        cwd: String::new(),
        activity: String::new(),
        kind: SessionKind::Anonymous,
    };
    let bytes = info.encode();
    // core + (u16 len + 0 activity bytes) + 1 kind byte
    assert_eq!(bytes.len(), INFO_LEN + 2 + 1);
    assert_eq!(SessionInfo::decode(&bytes).unwrap().kind, SessionKind::Anonymous);
}

#[test]
fn info_decodes_pre_kind_record_as_unknown() {
    // A daemon with the activity tail but no kind byte (2026-09 builds).
    let info = SessionInfo {
        clients: 0,
        pid: 1,
        cmd: "bash".to_string(),
        cwd: String::new(),
        activity: "vim".to_string(),
        kind: SessionKind::Named,
    };
    let mut bytes = info.encode();
    bytes.truncate(INFO_LEN + 2 + "vim".len());
    let decoded = SessionInfo::decode(&bytes).unwrap();
    assert_eq!(decoded.activity, "vim");
    assert_eq!(decoded.kind, SessionKind::Unknown);
}
```

Also update the existing `info_roundtrip` length assertion from `INFO_LEN + 2 + "htop".len()` to `INFO_LEN + 2 + "htop".len() + 1`, and add `kind: SessionKind::Named,` to that literal (and `kind: SessionKind::Unknown,` to the other four literals so they compile).

**Step 2: Run tests to verify they fail**

Run: `just debug-cargo test -p posh session::ipc`
Expected: compile FAIL, `no field kind`.

**Step 3: Write minimal implementation**

In `ipc.rs`, add `use posh_proto::caps::SessionKind;` to the imports. Change the struct:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub clients: u64,
    pub pid: i32,
    pub cmd: String,
    pub cwd: String,
    /// The RFC 0013 §5 activity label (`title · process`, or empty). Appended
    /// after the fixed core on the wire; empty from a pre-activity daemon.
    pub activity: String,
    /// The session's kind (design 2026-09-21 §1), one byte appended after
    /// the activity label; `Unknown` from a daemon that predates it.
    pub kind: SessionKind,
}
```

At the end of `encode`, after the activity bytes:

```rust
        // The kind byte follows the activity label (design 2026-09-21 §1);
        // a pre-kind client stops reading after the label.
        out.push(self.kind.to_byte());
        out
```

In `decode`, replace the activity block so it also yields the kind:

```rust
        let tail = &payload[INFO_LEN..];
        let (activity, kind) = if tail.len() >= 2 {
            let len = (u16::from_le_bytes([tail[0], tail[1]]) as usize).min(MAX_ACTIVITY_LEN);
            if tail.len() >= 2 + len {
                let activity = String::from_utf8_lossy(&tail[2..2 + len]).into_owned();
                let kind = tail.get(2 + len).copied().map(SessionKind::from_byte).unwrap_or_default();
                (activity, kind)
            } else {
                (String::new(), SessionKind::Unknown)
            }
        } else {
            (String::new(), SessionKind::Unknown)
        };
        Some(SessionInfo { clients, pid, cmd, cwd, activity, kind })
```

**Step 4: Run tests to verify they pass**

Run: `just debug-cargo test -p posh session::ipc`
Expected: all `info_*` tests pass. Then `just debug-cargo build -p posh` to find every other `SessionInfo { .. }` literal (`daemon.rs:1545`, and any in `session/mod.rs` tests) and add `kind: SessionKind::Unknown` for now; Task 4 sets the real value in the daemon.

**Step 5: Commit**

```bash
git add crates/posh/src/session/ipc.rs crates/posh/src/session/daemon.rs crates/posh/src/session/mod.rs
git commit -m "posh: SessionInfo carries the session kind after the activity label (skew-tolerant)"
```

---

### Task 3: `ensure_session` takes the kind

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/session/daemon.rs:66-105` (`ensure_session`), `:915-1003` (`daemon_main`), `:1085-1094` (`daemon_loop` signature), `:1545` (the `Tag::Info` reply)
- Modify every caller: `session/client.rs:78,183(via connect_or_create),243,246,286`; `session/mod.rs:197-205` (`connect_or_create`), `:843` (`cmd_run`), `:922` (`cmd_fork`); `remote/relay.rs:455,739`; `remote/server.rs:255`
- Test: `crates/posh/src/session/daemon.rs` `mod tests`

**Step 1: Write the failing test**

Find the existing in-process daemon test helper in `daemon.rs`'s `tests` module (search for a test that connects to a daemon socket and reads `Tag::Info`; if none exists, the mux tests' `start_inprocess_daemon` in `remote/mux.rs:3594` shows the pattern: bind a listener in a temp base, run `daemon_loop` on a thread with a stub `PtyChild`). Add:

```rust
#[test]
fn info_reports_the_kind_the_session_was_created_with() {
    let dir = temp_base();
    let cfg = Config { socket_dir: dir.clone(), group: "default".into() };
    // The daemon runs in-process on a thread; `kind` is the new parameter.
    let handle = spawn_test_daemon(&cfg, "k1", None, SessionKind::Anonymous);
    let probe = crate::session::probe_session(&cfg.socket_path("k1").unwrap()).unwrap();
    assert_eq!(probe.info.kind, SessionKind::Anonymous);
    handle.shutdown();
}
```

If no `spawn_test_daemon` helper exists yet, write it in the test module: it must call the same `daemon_loop` production code with a `kind` argument, on a thread, against a `PtyChild` from `pty::spawn_shell(Some(&["sleep".into(), "30".into()]), 24, 80, &[], None)`. Keep it minimal; it is reused by later sections.

**Step 2: Run test to verify it fails**

Run: `just debug-cargo test -p posh info_reports_the_kind`
Expected: compile FAIL (no `kind` parameter).

**Step 3: Write minimal implementation**

`ensure_session`:

```rust
pub fn ensure_session(
    cfg: &Config,
    name: &str,
    command: Option<Vec<String>>,
    kind: SessionKind,
) -> Result<bool> {
    // ... unchanged body ...
    daemon_main(cfg, name, listener, command, kind);
}
```

`daemon_main` gains `kind: SessionKind`, logs it (`daemon started session={name} kind={} pid=…`, `kind.as_str()`), and passes it to `daemon_loop`, which gains `kind: SessionKind` after `cwd: &str` and uses it in the `Tag::Info` reply:

```rust
let info = SessionInfo {
    clients: (total_clients - 1) as u64,
    pid: child.pid,
    cmd: info_cmd.to_string(),
    cwd: cwd.to_string(),
    activity,
    kind,
};
```

`connect_or_create` in `session/mod.rs` gains `kind: SessionKind` and forwards it. Every other caller passes `SessionKind::Named` for now (the default for anything that does not say). Do NOT change behavior at any site in this task.

**Step 4: Run tests to verify they pass**

Run: `just debug-cargo test -p posh session::` then `just debug-cargo build -p posh`.
Expected: green; the build is the check that every call site was updated.

**Step 5: Commit**

```bash
git add crates/posh/src
git commit -m "posh: ensure_session/connect_or_create take a SessionKind; every caller says Named"
```

---

### Task 4: `--kind` on `posh start` and `posh attach --create`, derived from the target

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/main.rs:420-448` (`parse_attach_args`), `:454-477` (`parse_start_args`), `:540-592` (`cmd_start`), `:350-394` (`cmd_attach`)
- Modify: `crates/posh/src/session/client.rs:77-85` (`ensure_detached`), `:149-189` (`cmd_attach`), `:215-264` (`switch_in_place`), `:271-297` (`cmd_start_local`)
- Test: `crates/posh/src/main.rs` `mod tests` (`parse_attach_args_*`, `parse_start_args_grammar`, `classify_start_target_*`)

**Step 1: Write the failing tests**

```rust
#[test]
fn parse_start_args_reads_kind() {
    let v = |xs: &[&str]| -> Vec<String> { xs.iter().map(|s| s.to_string()).collect() };
    let a = v(&["--kind", "anonymous", ":+"]);
    let (d, kind, t, c) = parse_start_args(&a);
    assert!(!d);
    assert_eq!(kind, Some(SessionKind::Anonymous));
    assert_eq!(t, Some(":+"));
    assert!(c.is_empty());
    // Absent: None (the caller derives it from the target class).
    let a = v(&["dev"]);
    assert_eq!(parse_start_args(&a).1, None);
    // `--kind` after the target parses too, like --detach.
    let a = v(&["dev", "--kind", "named", "--", "htop"]);
    let (_, kind, t, c) = parse_start_args(&a);
    assert_eq!((kind, t), (Some(SessionKind::Named), Some("dev")));
    assert_eq!(c, &v(&["htop"])[..]);
}

#[test]
fn parse_attach_args_reads_kind() {
    let v = |xs: &[&str]| -> Vec<String> { xs.iter().map(|s| s.to_string()).collect() };
    let a = v(&["--create", "--kind", "anonymous", "s-1"]);
    let (d, cr, kind, n, _c) = parse_attach_args(&a).unwrap();
    assert!(!d && cr && n == "s-1");
    assert_eq!(kind, Some(SessionKind::Anonymous));
    // A bad value is an error, not a silent Named.
    assert!(parse_attach_args(&v(&["--kind", "system", "x"])).is_err());
    assert!(parse_attach_args(&v(&["--kind", "bogus", "x"])).is_err());
}

#[test]
fn start_kind_derives_from_the_target_class() {
    use StartClass::*;
    assert_eq!(start_kind(&LocalAuto { group: None }, None), SessionKind::Anonymous);
    assert_eq!(
        start_kind(&LocalNamed { group: None, session: "dev".into() }, None),
        SessionKind::Named
    );
    assert_eq!(
        start_kind(&RemoteAuto { user: None, host: "box".into(), group: None }, None),
        SessionKind::Anonymous
    );
    // An explicit --kind wins over the derivation.
    assert_eq!(start_kind(&LocalAuto { group: None }, Some(SessionKind::Named)), SessionKind::Named);
}
```

Update the existing `parse_start_args_grammar` and `parse_attach_args_*` destructurings for the new tuple arity.

**Step 2: Run tests to verify they fail**

Run: `just debug-cargo test -p posh parse_start_args parse_attach_args start_kind`
Expected: compile FAIL.

**Step 3: Write minimal implementation**

`parse_start_args` returns `(bool, Option<SessionKind>, Option<&str>, &[String])`. In both leading and trailing flag loops accept `--kind <value>` alongside `--detach`; a `--kind` with a missing or unparsable value makes `parse_start_args` return… it currently cannot error, so change its signature to `Result<(...)>` and update its callers (`cmd_start`). `parse_attach_args` likewise returns `Result<(bool, bool, Option<SessionKind>, &str, &[String])>` and errors with `--kind: expected anonymous or named, got {v}`.

Add:

```rust
/// The kind a `posh start` creates: an explicit `--kind` wins, else the
/// target class decides — an auto-id target (`:+`, `host:+`, no target) is
/// anonymous, a named one is named (design 2026-09-21 §1).
fn start_kind(class: &StartClass, explicit: Option<SessionKind>) -> SessionKind {
    explicit.unwrap_or(match class {
        StartClass::LocalAuto { .. } | StartClass::RemoteAuto { .. } => SessionKind::Anonymous,
        StartClass::LocalNamed { .. } | StartClass::RemoteNamed { .. } => SessionKind::Named,
    })
}
```

The handshake line (go-plugin shape, `POSH CONNECT` family). Add to `session/client.rs` beside `ensure_detached`:

```rust
/// The `posh start --detach` handshake line, printed to stdout BEFORE the
/// pinned prose when the caller asked for it with the `POSH_HANDSHAKE=1`
/// env (the ssh-crossing env-prefix convention of `sshwrap::remote_command`,
/// doubling as go-plugin's magic cookie): `POSH START <version> <name>
/// <kind>`. Version 1; a later version appends fields, never reorders.
/// Same family as `POSH IP` / `POSH CONNECT` (remote/server.rs) so every
/// posh-over-ssh exchange reads alike; the pipe-delimited cutover is a
/// tracked follow-on.
pub fn start_handshake_line(name: &str, kind: SessionKind) -> String {
    format!("POSH START 1 {name} {}", kind.as_str())
}

/// `POSH_HANDSHAKE=1|on|true|yes` requests the handshake line.
fn handshake_requested() -> bool {
    matches!(
        std::env::var("POSH_HANDSHAKE").as_deref(),
        Ok("1") | Ok("on") | Ok("true") | Ok("yes")
    )
}
```

and in `ensure_detached`, before the `println!` pair:

```rust
    if handshake_requested() {
        println!("{}", start_handshake_line(name, kind));
    }
```

with a test (pure, no env):

```rust
#[test]
fn start_handshake_line_is_the_posh_family_with_a_version_first() {
    assert_eq!(start_handshake_line("s-3", SessionKind::Anonymous), "POSH START 1 s-3 anonymous");
    assert_eq!(start_handshake_line("dev", SessionKind::Named), "POSH START 1 dev named");
}
```

Thread the kind: `cmd_start` computes `let kind = start_kind(&class, explicit);` and passes it to `cmd_start_local(cfg, name, command, detach, kind)`; `cmd_start_local` passes it to `ensure_detached` / `switch_in_place`'s `SwitchCreate::Strict` (add a `kind` field to `Ensure` and `Strict`) / `ensure_session`. `cmd_attach` (main.rs) passes the parsed `kind.unwrap_or(SessionKind::Named)` to `session::client::cmd_attach`, which passes it to `connect_or_create` / `SwitchCreate::Ensure` / `ensure_detached`. The remote branches of `cmd_start` / `cmd_attach` do NOT change in this task (Task 5 owns the remote auto path; a remote named create stays `Named` by default).

**Step 4: Run tests to verify they pass**

Run: `just debug-cargo test -p posh main::tests`
Expected: green.

**Step 5: Commit**

```bash
git add crates/posh/src/main.rs crates/posh/src/session/client.rs
git commit -m "posh: --kind on start/attach --create, derived from the target class; local :+ creates anonymous"
```

---

### Task 5: remote auto-id create is remote-atomic and anonymous, answered by a handshake line

**Promotion criteria:** the pre-existing probe-then-attach path (`remote_session_names` + `first_free_autoid`) can be removed once every fleet host runs a `--kind`-aware posh (signal: no `remote start --kind unavailable` log line for 30 days). The space-separated `POSH START` line is itself slated for a pipe-delimited go-plugin-style cutover, tracked as posh#202.

**Files:**
- Modify: `crates/posh/src/main.rs:654-670` (`start_remote_auto`), plus a new `remote_start_argv` beside `remote_list_argv` (`:1345`)
- Test: `crates/posh/src/main.rs` `mod tests`

**Step 1: Write the failing tests**

```rust
#[test]
fn remote_start_argv_creates_detached_anonymous_with_the_handshake_cookie() {
    let dest = remote::sshwrap::SshDest::resolve("box");
    assert_eq!(
        remote_start_argv(&dest, "default", true),
        ["ssh", "-o", "BatchMode=yes", "box", "POSH_HANDSHAKE=1", "posh", "start", "--detach", "--kind", "anonymous"]
            .map(String::from)
    );
    assert_eq!(
        remote_start_argv(&dest, "grp", false),
        ["ssh", "box", "POSH_HANDSHAKE=1", "posh", "-g", "grp", "start", "--detach", "--kind", "anonymous"]
            .map(String::from)
    );
}

#[test]
fn start_handshake_is_parsed_and_prose_is_not() {
    // The new remote: handshake line first, then the pinned prose.
    assert_eq!(
        parse_start_handshake("POSH START 1 s-3 anonymous\nsession \"s-3\" created\n"),
        Some(("s-3".to_string(), SessionKind::Anonymous))
    );
    // Noise before the line (a motd, a warning) is skipped, like LineScraper.
    assert_eq!(
        parse_start_handshake("warning: x\nPOSH START 1 s-12 named\n"),
        Some(("s-12".to_string(), SessionKind::Named))
    );
    // A newer version with extra fields still yields the first three.
    assert_eq!(
        parse_start_handshake("POSH START 2 s-3 anonymous extra=1\n"),
        Some(("s-3".to_string(), SessionKind::Anonymous))
    );
    // Prose alone is an OLD remote (no handshake) — None, so the caller falls back.
    assert_eq!(parse_start_handshake("session \"s-3\" created\n"), None);
    assert_eq!(parse_start_handshake("session \"s-3\" already exists\n"), None);
    assert_eq!(parse_start_handshake(""), None);
    // A version we cannot read (0, or non-numeric) is rejected, not guessed.
    assert_eq!(parse_start_handshake("POSH START x s-3 anonymous\n"), None);
}
```

`SshDest::resolve` may substitute a tailnet host; if the test environment makes the first assertion brittle, construct the expected argv from `dest.ssh_args()` and `dest.target()` as `remote_kill_argv`'s tests do.

**Step 2: Run tests to verify they fail**

Run: `just debug-cargo test -p posh remote_start_argv created_name`
Expected: compile FAIL.

**Step 3: Write minimal implementation**

```rust
/// `ssh [-o BatchMode=yes] … <dest> POSH_HANDSHAKE=1 posh [-g G] start --detach
/// --kind anonymous`: the remote-atomic auto-id create (design 2026-09-21
/// §1). The env prefix is the ssh-crossing cookie (`sshwrap::remote_command`
/// precedent) that asks the remote for the `POSH START` handshake line; the
/// remote picks the free `s-N` itself, and the attach that follows finds it
/// live.
fn remote_start_argv(dest: &remote::sshwrap::SshDest, group: &str, batch: bool) -> Vec<String> {
    let mut argv: Vec<String> = vec!["ssh".to_string()];
    if batch {
        argv.push("-o".into());
        argv.push("BatchMode=yes".into());
    }
    argv.extend(dest.ssh_args());
    argv.push(dest.target());
    argv.push("POSH_HANDSHAKE=1".into());
    argv.push("posh".into());
    if group != "default" {
        argv.push("-g".into());
        argv.push(group.into());
    }
    argv.extend(["start", "--detach", "--kind", "anonymous"].map(String::from));
    argv
}

/// The `POSH START <version> <name> <kind>` handshake line
/// (`session::client::start_handshake_line`), scanned line by line past any
/// motd noise the way `LineScraper` scans for `POSH CONNECT`. `None` when no
/// such line is present — the prose-only answer of a remote that predates
/// the handshake, the caller's cue to fall back. A version we cannot parse
/// is `None` too; extra trailing fields from a newer version are ignored.
fn parse_start_handshake(stdout: &str) -> Option<(String, SessionKind)> {
    stdout.lines().find_map(|l| {
        let rest = l.strip_prefix("POSH START ")?;
        let mut f = rest.split_whitespace();
        let _version: u32 = f.next()?.parse().ok()?;
        let name = f.next()?.to_string();
        let kind = SessionKind::parse(f.next()?)?;
        Some((name, kind))
    })
}
```

Rewrite `start_remote_auto`:

```rust
fn start_remote_auto(
    user: Option<String>,
    host: String,
    target_group: Option<String>,
    global_group: &str,
    extra: &[String],
    forward_flag: &remote::agent::ForwardFlag,
) -> Result<()> {
    let grp = target_group.clone().unwrap_or_else(|| global_group.to_string());
    let dest = remote::sshwrap::SshDest::resolve(&ph_dest(user.as_deref(), &host));
    let batch = !util::is_tty(0);
    // Remote-atomic anonymous create answered by the POSH START handshake
    // line. A remote too old for --kind fails the exec, and one too old for
    // the handshake answers prose only; both fall back to the
    // probe-then-attach path (kind unknown).
    let argv = remote_start_argv(&dest, &grp, batch);
    let created = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| parse_start_handshake(&String::from_utf8_lossy(&o.stdout)));
    let id = match created {
        Some((id, _kind)) => id,
        None => {
            util::log_write("info", "remote start --kind unavailable; falling back to probe");
            let names = remote_session_names(user.as_deref(), &host, &grp)?;
            first_free_autoid(&names).ok_or_else(|| Error::from("posh start: too many remote sessions"))?
        }
    };
    cmd_ssh_session(user, host, target_group, global_group, id, extra, forward_flag)
}
```

Note: a `--detach` in `extra` (a detached remote auto spawn) already runs its own `posh attach --detach` over ssh in `cmd_ssh_session`; with the session now pre-created that inner call prints `already exists` and returns, which is the FDR 0010 idempotent contract. Leave it.

**Step 4: Run tests to verify they pass**

Run: `just debug-cargo test -p posh main::tests`
Expected: green.

**Step 5: Commit**

```bash
git add crates/posh/src/main.rs
git commit -m "posh: ph host:+ creates the auto-id session remote-atomically as anonymous, then attaches"
```

---

### Task 6: `posh list` surfaces the kind (JSON, remote parse, picker, table)

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/session/mod.rs:362-376` (`SessionEntry`), `:424-443` (`PickerEntry`), `:502-557` (`scan_sessions`), `:581-600` (`remote_entries`), `:603-643` (`json_list`)
- Modify: `crates/posh/src/session/mesa.rs:81-88` (columns), `:116-138` (`row`)
- Test: `session/mod.rs` `json_list_shape_matches_zmx`, `remote_entries_maps_json_rows_with_prefix_and_errors`; `mesa.rs` tests

**Step 1: Write the failing tests**

In `session/mod.rs` tests, give the `alpha` literal `kind: Some(SessionKind::Anonymous)` and the `minimal` literal `kind: None`, and change the expected JSON so `alpha` reads `…,"echo":"optimistic auto-escalated 412ms","kind":"anonymous","current":false}` while `minimal` is unchanged (no `kind` key when unknown). In `remote_entries_maps_json_rows_with_prefix_and_errors`, add `"kind":"named"` to the `dev` row and assert `entries[0].kind == Some(SessionKind::Named)` and `entries[2].kind == None`.

In `mesa.rs` tests, add:

```rust
#[test]
fn header_has_a_kind_column_and_rows_fill_it() {
    let mut s = entry("dev", 1);
    s.kind = Some(SessionKind::Anonymous);
    let records = parse_lines(&build_ndjson(&[s], None, None, Path::new("/x")));
    let cols = records[0]["columns"].as_array().unwrap();
    assert!(cols.iter().any(|c| c["name"] == "KIND"));
    let cells = records[1]["cells"].as_array().unwrap();
    assert!(cells.iter().any(|c| c == "anonymous"));
}
```

Add a `PickerEntry` test in `session/mod.rs`:

```rust
#[test]
fn picker_entry_status_names_an_anonymous_session() {
    let mut e = SessionEntry { /* detached, no error */ ..entry_fixture("s-1") };
    e.kind = Some(SessionKind::Anonymous);
    assert_eq!(PickerEntry::from_entry(&e).status, "detached · anonymous");
    e.kind = Some(SessionKind::Named);
    assert_eq!(PickerEntry::from_entry(&e).status, "detached");
}
```

(Write `entry_fixture` if no such helper exists.)

**Step 2: Run tests to verify they fail**

Run: `just debug-cargo test -p posh session::`
Expected: compile FAIL.

**Step 3: Write minimal implementation**

- `SessionEntry` gains `kind: Option<SessionKind>` (`None` = unknown or unreachable).
- `scan_sessions`: `kind: (probe.info.kind != SessionKind::Unknown).then_some(probe.info.kind)`; the error arm `kind: None`.
- `json_list`: after `echo`, `if let Some(k) = s.kind { out.push_str(",\"kind\":"); out.push_str(&json_string(k.as_str())); }`.
- `remote_entries`: `kind: v["kind"].as_str().and_then(|k| SessionKind::parse(k).or((k == "system").then_some(SessionKind::System)))`.
- `PickerEntry::from_entry`: append ` · anonymous` to the status word when `s.kind == Some(SessionKind::Anonymous)` (named is the unmarked case; system never lists yet).
- `mesa.rs`: add `{"name": "KIND", "role": "pin"}` after `CLIENTS`; the error row gets an extra `""`; the data row inserts `s.kind.map(|k| k.as_str().to_string()).unwrap_or_default()` after the clients cell. Adjust the `shrink_of` test only if it indexes by position.

**Step 4: Run tests to verify they pass**

Run: `just debug-cargo test -p posh session::`
Expected: green.

**Step 5: Commit**

```bash
git add crates/posh/src/session
git commit -m "posh: list/picker/table carry the session kind; JSON omits it when unknown"
```

---

### Task 7: man pages and the FDR 0015 note

**Files:**
- Modify: `doc/posh.1.scd` (`start` and `attach` synopses: `[--kind anonymous|named]`; a KIND paragraph under `list`; ENVIRONMENT gains `POSH_HANDSHAKE` and a HANDSHAKE paragraph under `start --detach` documenting `POSH START 1 <name> <kind>` beside the existing `POSH CONNECT` description in `posh-server(1)`)
- Modify: `docs/features/0015-ph-front-door.md` (one sentence: `:+` / create-new sessions are created with kind `anonymous`, recorded by the daemon)
- Modify: `docs/rfcs/0001-target-grammar-and-caps.md` only if a capability id changes (it does not in Section 1; skip)

**Step 1:** Edit the scdoc. **Step 2:** `just lint-doc`, expected clean. **Step 3:** Commit:

```bash
git add doc docs/features/0015-ph-front-door.md
git commit -m "docs: --kind on start/attach, KIND in posh list; FDR 0015 notes anonymous creates"
```

**Step 4:** `merge-this-session` (the `just` gate runs there). Section 1 ships alone.

---

## Section 2: viewport registry

**Two corrections to the earlier outline (2026-09-21, after Section 1 merged):**
(a) the kind does NOT ride the activity entry with a format bump — an old
roaming client's `decode_session_activity` rejects an unknown format byte and
would lose the whole label against a new daemon. It rides a NEW capability id,
`CAP_SESSION_KIND` (20), which an old client ignores (RFC 0001 table rule). (b)
RFC 0014 §5 UPSTREAM is the nested-session entry (id 18), not a viewport
registry; the viewport socket gets its own new §6.

**Dev loop for this section (lesson from Section 1's two gate failures):** before
merging, run `just debug-cargo clippy --all-targets -- -D warnings` AND
`just debug-cargo test -p posh --test session_integration`, not only the unit
binary.

### Task 8: `CAP_SESSION_KIND` on the frame

**Promotion criteria:** N/A (additive cap id).

**Files:**
- Modify: `crates/posh-proto/src/caps.rs:159` (after `CAP_EXIT_CAUSE`), plus encode/decode helpers beside `encode_session_activity` (~387-428); tests.
- Modify: `docs/rfcs/0001-target-grammar-and-capability-table.md:252-253` — a `| 20 | SESSION_KIND | server | 1 byte | … |` row in the style of id 19, and the unassigned range becomes `21–223`.
- Modify: `crates/posh/src/session/daemon.rs:209-216` (`ClientConn` activity fields), `:383-391` (`queue_frame`'s activity cap), `daemon_loop`'s per-iteration activity block (~1257-1276); the kind reaches `daemon_loop` already (Task 3).
- Modify: `crates/posh/src/remote/client.rs:3013-3019` (decode beside `CAP_SESSION_ACTIVITY`; `ClientState` gains `session_kind: SessionKind`) and `crates/posh/src/session/client.rs:671-676` (same, a `kind` field beside `activity`).
- Relay and bridge: nothing — they forward server cap entries unchanged (verify with a test, not an edit).

**Step 1: failing tests**

posh-proto:
```rust
#[test]
fn session_kind_cap_roundtrip_and_rejections() {
    for k in [SessionKind::Anonymous, SessionKind::Named, SessionKind::System] {
        let cap = encode_session_kind(k);
        assert_eq!(cap.id, CAP_SESSION_KIND);
        assert_eq!(cap.payload, vec![k.to_byte()]);
        assert_eq!(decode_session_kind(&cap.payload), Some(k));
    }
    assert_eq!(decode_session_kind(&[]), None);
    assert_eq!(decode_session_kind(&[1, 2]), None, "exactly one byte");
    assert_eq!(decode_session_kind(&[0]), Some(SessionKind::Unknown));
    assert_eq!(decode_session_kind(&[77]), Some(SessionKind::Unknown));
}
```

daemon.rs tests (extend `spawn_test_daemon`'s in-process daemon; look at how the existing activity-on-frame test at ~3242-3285 drives `queue_frame` with `wants_activity`):
```rust
#[test]
fn kind_rides_the_first_activity_bearing_frame_once() {
    // A client that requested CAP_SESSION_ACTIVITY gets CAP_SESSION_KIND on
    // the same frame as its first activity entry, and never again (the kind
    // never changes). A client that did not request activity gets neither.
}
```

remote/client.rs tests (beside the existing test at ~5689 that feeds a frame with an activity cap and asserts `st.session_activity`):
```rust
#[test]
fn session_kind_cap_is_held_and_feeds_the_current_target() {
    // feed a frame carrying encode_session_kind(Anonymous); assert
    // st.session_kind == Anonymous and picker::current_kind() == Anonymous.
}
```

**Step 2:** run them; compile failures.

**Step 3: implementation**

caps.rs:
```rust
/// The session's kind (design 2026-09-21 §1): server entry, one byte
/// (`SessionKind::to_byte`), attached to the same visible frame as the
/// client's FIRST `SESSION_ACTIVITY` entry and never again — a session's
/// kind is fixed at create time. A relay / M2 bridge forwards it unchanged;
/// a standalone Arch-A server (an ephemeral shell, no daemon) never sends
/// it. Display / policy on the viewport side only (the FDR 0016 stack and
/// the leave prompt); an old client ignores the id.
pub const CAP_SESSION_KIND: u8 = 20;

pub fn encode_session_kind(kind: SessionKind) -> Cap {
    Cap { id: CAP_SESSION_KIND, payload: vec![kind.to_byte()] }
}

/// `None` for anything but exactly one byte (malformed); an unknown byte
/// value reads `Unknown`, per `SessionKind::from_byte`.
pub fn decode_session_kind(payload: &[u8]) -> Option<SessionKind> {
    match payload {
        [b] => Some(SessionKind::from_byte(*b)),
        _ => None,
    }
}
```

daemon.rs: `ClientConn` gains `kind_sent: bool`. In `queue_frame`, when the
activity cap is being attached (the existing branch) and `!self.kind_sent`,
push `caps::encode_session_kind(kind)` too and set `kind_sent = true`. That
needs the kind at `queue_frame`: store it on `ClientConn` at accept
(`kind: SessionKind`, set from `daemon_loop`'s parameter where new conns are
built ~1339), so no signature churn.

remote/client.rs: in the frame-cap block, after the activity decode:
```rust
    if let Some(cap) = caps::find(&frame.caps, caps::CAP_SESSION_KIND) {
        if let Some(kind) = caps::decode_session_kind(&cap.payload) {
            st.session_kind = kind;
            crate::picker::set_current_kind(kind);
        }
    }
```
session/client.rs: the same in `render_frame_acking` (a `kind` field on the
renderer struct beside `activity`). `picker::set_current_kind` is Task 9's;
for this task add it as a minimal `pub fn set_current_kind(_: SessionKind) {}`
stub ONLY if Task 9 is not being done in the same dispatch — otherwise land
them together.

**Step 4:** `just debug-cargo test -p posh-proto`, `-p posh session::`,
`-p posh remote::client`, `-p posh --test session_integration`; clippy.

**Step 5:** commit `posh: CAP_SESSION_KIND (20) rides the first activity frame; both clients hold it`.

### Task 9: typed stack entries and the current kind

**Files:**
- Modify: `crates/posh/src/picker.rs:213-297` (the `STACK` / `CURRENT` statics and their accessors), `:350-373` (`auto_pop`), `:384-390` (`set_current` / `current`), tests `:640-730`.
- Modify: `crates/posh/src/main.rs:46-79` (`run()`), `dispatch_ph` (`LocalNew` / `RemoteNew` arms ~778-804).
- Modify: `crates/posh/src/session/client.rs:1003-1006` and `remote/client.rs:415`, `:1154` (the `stack_top` readers: they now get an entry, use `.target`).

**Step 1: failing tests** (picker.rs):
```rust
#[test]
fn stack_entries_carry_the_kind_of_the_session_left() {
    while stack_pop().is_some() {}
    set_current(":s-1");
    set_current_kind(SessionKind::Anonymous);
    stack_push_current();
    set_current("box:dev");
    set_current_kind(SessionKind::Named);
    stack_push_current();
    assert_eq!(stack_top().map(|e| (e.target, e.kind)), Some(("box:dev".into(), SessionKind::Named)));
    stack_pop();
    assert_eq!(stack_top().map(|e| e.kind), Some(SessionKind::Anonymous));
    while stack_pop().is_some() {}
}

#[test]
fn current_kind_falls_back_to_anonymous_only_for_a_created_dispatch() {
    // A created (`:+`) dispatch against a daemon that never says (Unknown)
    // reads Anonymous; the same daemon after a plain attach reads Unknown;
    // a daemon that DOES say wins over the fallback.
    set_current(":s-9");
    mark_current_created();
    assert_eq!(current_kind(), SessionKind::Anonymous);
    set_current_kind(SessionKind::Named);
    assert_eq!(current_kind(), SessionKind::Named);
    set_current(":dev"); // a new attach clears both
    assert_eq!(current_kind(), SessionKind::Unknown);
}
```
Update the existing stack tests (`:653-730`) and the two client `stack_top`
readers for `StackEntry`.

**Step 3: implementation**
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackEntry {
    pub target: String,
    pub kind: SessionKind,
}
static STACK: Mutex<Vec<StackEntry>> = Mutex::new(Vec::new());
/// The attach in progress: its target, the kind its daemon reported
/// (`Unknown` until it does), and whether this front door CREATED it
/// (`:+` / create-new) — the design §2 fallback: a created session whose
/// daemon predates the kind reads Anonymous.
struct Current { target: String, kind: SessionKind, created: bool }
static CURRENT: Mutex<Option<Current>> = Mutex::new(None);

pub fn set_current(target: &str)            // resets kind=Unknown, created=false
pub fn set_current_kind(kind: SessionKind)  // from a frame / probe; no-op with no current
pub fn mark_current_created()               // by run()/dispatch_ph on a LocalNew/RemoteNew route
pub fn current() -> Option<String>          // unchanged signature
pub fn current_kind() -> SessionKind        // kind, else Anonymous if created, else Unknown
pub fn current_entry() -> Option<StackEntry>
pub fn stack_push_current()                 // replaces stack_push(&str); pushes current_entry()
pub fn stack_top() -> Option<StackEntry>
pub fn stack_pop() -> Option<StackEntry>
```
`run()`: `picker::stack_push_current()` in place of `stack_push(&leaving)`;
`dispatch_ph`'s `LocalNew` / `RemoteNew` arms call `picker::mark_current_created()`
AFTER the entry point has called `set_current` — simplest: `cmd_start_local`
and `start_remote_auto` are the two creators, so call it right after their
`set_current` (client.rs:295 for local; main.rs `start_remote_auto` before
`cmd_ssh_session`, which calls `set_current` itself — so instead mark from
`cmd_ssh_session` when its caller says `created`; add a `created: bool`
parameter there or a `picker::mark_current_created()` call in
`start_remote_auto` immediately after `cmd_ssh_session` returns is too late —
choose: `cmd_ssh_session` gains no parameter; `start_remote_auto` sets a
one-shot `picker::next_attach_is_created()` flag that `set_current` consumes.
Keep whichever is smaller; test it.)

**Step 5:** commit `posh: the FDR 0016 stack carries each session's kind; a created session defaults to anonymous`.

### Task 10: the viewport status socket

**Files:**
- Create: `crates/posh/src/viewport_status.rs` (module registered in `main.rs`).
- Modify: `crates/posh/src/main.rs:46-79` (`run()` binds before `run_once`, refreshes after every stack/current change), `session/mod.rs:167-191` (`cmd_status` gains `--viewport <pid>`), the `status` arm in `main.rs:~244`.
- Modify: `picker.rs` — `overlay_open(kind: &'static str, over: Option<&str>)` / `overlay_close()` and an `overlays()` reader; called around `p.open(..)` in `session/client.rs:1050,1737,1747`, `remote/client.rs:425,1132,1160`, and around `choose_standalone` in `main.rs:981`.

**Response grammar** (design §2; RFC 0014 §6 in Task 11):
```
viewport pid=<pid> current=<target|-> kind=<kind> created=<0|1>
stack depth=<n> target=<target> kind=<kind>        (one per entry, bottom first, depth 1..)
overlay kind=<palette|picker|leave> over=<target|->  (one per live overlay)
```

**Implementation sketch:**
```rust
//! RFC 0014 §6: the viewport is the daemon for its own history. One
//! `<base>/viewports/<pid>.status.sock` (+ `.status.pid`) per front-door
//! process, answering connect → response → EOF like a session daemon's
//! socket (§4.1). The response is rebuilt by `refresh()` whenever the
//! stack, the current attach, or an overlay changes; the accept thread
//! serves the latest snapshot. `POSH_VIEWPORT_STATUS=0|off|false|no`
//! skips the bind (diagnostic only; nothing depends on it).
pub fn bind() -> Option<ViewportStatus>          // mirrors server.rs bind_remote_status_socket
impl ViewportStatus { pub fn refresh(&self) }    // renders picker state into the shared String
impl Drop for ViewportStatus                     // removes sock + pidfile
pub fn dir() -> PathBuf                          // <base>/viewports
pub fn reap_dead()                               // unlink pairs whose .status.pid is not alive (kill -0); called by bind() and by `posh status --viewport`
pub(crate) fn render(current: Option<&Current-ish view>, stack: &[StackEntry], overlays: &[Overlay]) -> String  // pure; the tests pin it
```
`picker` exposes a `pub fn snapshot() -> (Option<(String, SessionKind, bool)>, Vec<StackEntry>, Vec<Overlay>)` so `render` stays pure and `refresh` is one call. `run()` calls `refresh()` after `stack_push_current` / `stack_pop` / each `dispatch_ph` return; the overlay helpers call it themselves.

**Tests:** `render` golden for empty / one entry / entries + overlay; bind →
`read_status_socket` → the same text; `POSH_VIEWPORT_STATUS=0` skips (test via
a `bind_with(enabled: bool, base: &Path)` seam, not the env); `reap_dead`
removes a pair whose pidfile names a dead pid and keeps a live one.

**Step 5:** commit `posh: per-viewport status socket serving the session stack and live overlays (RFC 0014 §6)`.

### Task 11: RFC 0014 §6 and the man page

- `docs/rfcs/0014-client-introspection-caps.md`: new `### 6. Viewport status socket` (path, liveness pidfile, the three line grammars above, reaping, the env gate), and a one-line cross-reference from §4.1. Status stays `proposed`.
- `docs/rfcs/0001-target-grammar-and-capability-table.md`: the id 20 row (if Task 8 did not already add it).
- `doc/posh.1.scd`: `status [--viewport pid] [session]`; ENVIRONMENT gains `POSH_VIEWPORT_STATUS`.
- `just lint-doc` clean; commit `docs: RFC 0014 §6 viewport status socket; posh status --viewport; CAP_SESSION_KIND in RFC 0001`.
- Then the Section 2 merge (attestation + `merge-this-session-async`).

---

## Section 3: StackView + palette_view

**State of the code this section starts from (verified 2026-09-21):** both
palettes already put *Back to X* as the SECOND row after *Switch session…*
(`remote/client.rs:353-357`, `session/client.rs:1018-1021`) and the stack
question dialogs (`back_commands`, `leave_commands`) live in `picker.rs`. The
remote heading is `rtt 12ms · echo: always[ (auto)]` (`palette_title`,
`remote/client.rs:404-407`, ~23-30 columns); the local palette opens with the
literal title `Commands`; the picker heading is `picker::title()` (`sessions ·
N to go back`). The renderer's panel is 46 columns with 2 of padding and
word-wraps a longer heading, so the budget is ~42.

### Task 12: `StackView` schema and the `palette_view` module

**Promotion criteria:** N/A.

**Files:**
- Modify: `crates/posh/src/picker.rs` — add `StackView` + `stack_view()` beside `stack_top` (~line 470); move `leave_commands` / `back_commands` / `leave_commands_for` (~174-211) and `title()` (~239-247) OUT to the new module (keep thin `pub use` re-exports in picker for one release so nothing else moves).
- Create: `crates/posh/src/remote/palette_view.rs` (registered in `remote/mod.rs`), the ONLY place a stack fact becomes RFC 0005 JSON or a heading string.
- Test: both files.

**Step 1: failing tests**

picker.rs:
```rust
#[test]
fn stack_view_reports_top_depth_and_current() {
    let _g = switch_test_guard();
    while stack_pop().is_some() {}
    assert_eq!(stack_view(), StackView { top: None, depth: 0, current: None });
    set_current(":s-1"); set_current_kind(SessionKind::Anonymous); stack_push_current();
    set_current("box:dev"); set_current_kind(SessionKind::Named);
    let v = stack_view();
    assert_eq!(v.depth, 1);
    assert_eq!(v.top.as_ref().map(|e| e.target.as_str()), Some(":s-1"));
    assert_eq!(v.current.as_ref().map(|e| (e.target.as_str(), e.kind)), Some(("box:dev", SessionKind::Named)));
    while stack_pop().is_some() {}
}
```

palette_view.rs (golden, pure — no statics touched):
```rust
fn entry(t: &str, k: SessionKind) -> StackEntry { StackEntry { target: t.into(), kind: k } }

#[test]
fn commands_title_names_the_top_and_counts_the_rest() {
    let none = StackView { top: None, depth: 0, current: None };
    assert_eq!(commands_title(&none, Some("rtt 12ms · echo: always")), "rtt 12ms · echo: always");
    assert_eq!(commands_title(&none, None), "Commands");
    let one = StackView { top: Some(entry(":s-1", SessionKind::Anonymous)), depth: 1, current: None };
    assert_eq!(commands_title(&one, None), "Commands · back: flac:s-1");       // ":" = this host, via display_target
    let many = StackView { top: Some(entry("me@box.example.com:grp/ff9fe216-9652-4e23-805c-6f4dd5ce7eca", SessionKind::Named)), depth: 3, current: None };
    assert_eq!(commands_title(&many, Some("rtt 12ms · echo: always")), "rtt 12ms · echo: always · back: box:grp/ff9fe216 +2");
}

#[test]
fn commands_title_stays_inside_the_renderer_budget() {
    // 46-column panel, 2 padding: a long host + a UUID name + the rtt/echo prefix
    // must abbreviate to ≤ 42 columns (chars; the strings are ASCII-ish plus '·').
    let v = StackView { top: Some(entry("someone@very-long-hostname.internal.example.com:grp/ff9fe216-9652-4e23-805c-6f4dd5ce7eca", SessionKind::Named)), depth: 12, current: None };
    let t = commands_title(&v, Some("rtt 1234ms · echo: optimistic (auto)"));
    assert!(t.chars().count() <= 42, "{t} is {} cols", t.chars().count());
}

#[test]
fn back_row_is_present_only_with_a_top() {
    let none = StackView { top: None, depth: 0, current: None };
    assert_eq!(back_row(&none), None);
    let one = StackView { top: Some(entry("box:dev", SessionKind::Named)), depth: 1, current: None };
    assert_eq!(back_row(&one), Some(json!({ "name": "Back to box:dev", "action": { "method": "session.pop" } })));
}

#[test]
fn picker_title_counts_the_stack() {
    let none = StackView { top: None, depth: 0, current: None };
    assert_eq!(picker_title(&none), "sessions");
    let two = StackView { top: Some(entry(":a", SessionKind::Named)), depth: 2, current: None };
    assert_eq!(picker_title(&two), "sessions · 2 to go back");
}
```
plus the existing `leave_commands_carry_target_and_previous` and the
`back_commands` test moved over unchanged.

**Step 2:** run `just debug-cargo test -p posh -- palette_view picker`; compile failures.

**Step 3: implementation**

picker.rs:
```rust
/// The stack as a VIEW MODEL (design 2026-09-21 §3): the session *Back*
/// returns to, how many are stacked, and the attach in progress. The only
/// producer; `remote::palette_view` is the only consumer that turns it into
/// RFC 0005 JSON or a heading — so the palette redesign changes that module
/// and nothing here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackView {
    pub top: Option<StackEntry>,
    pub depth: usize,
    pub current: Option<StackEntry>,
}
pub fn stack_view() -> StackView {
    StackView { top: stack_top(), depth: stack_depth(), current: current_entry() }
}
```
`display_target` (picker.rs ~377) becomes `pub(crate)`; `short_session` /
`short_host` too (they abbreviate the UUID and the host).

palette_view.rs:
```rust
//! Presentation of the FDR 0016 session stack for the RFC 0005 renderer —
//! the ONLY module that turns a `picker::StackView` into a heading string or
//! a palette row. Schema (`picker`) and presentation (here) are split on
//! purpose (design 2026-09-21 §3): the palette redesign edits this file.

/// Budget for a heading: posh-palette's panel is 46 columns with 2 of
/// padding and word-wraps beyond it.
const TITLE_BUDGET: usize = 42;

/// The Commands palette heading: the client's own prefix (the remote's
/// `rtt … · echo: …`, or `Commands` when the client has none) plus, with a
/// stack, ` · back: <top> [+N]`. The top is abbreviated like the default
/// title (`short_host`:`short_session`); if the whole line still exceeds
/// the budget the host is dropped from the back target, then the prefix is
/// truncated with `…` — the stack part is the reason the user opened it.
pub fn commands_title(view: &StackView, prefix: Option<&str>) -> String
/// `Back to <top>` → `session.pop`, or None without a stack.
pub fn back_row(view: &StackView) -> Option<Value>
/// The picker heading: `sessions` plus ` · N to go back` with a stack.
pub fn picker_title(view: &StackView) -> String
/// The leave question for a switch / a Back (moved from picker.rs, unchanged).
pub fn leave_commands(target: &str) -> Value
pub fn back_commands() -> Value
```
`picker::title()` becomes `palette_view::picker_title(&stack_view())` at its
call sites (`session/client.rs:1752`, `remote/client.rs:1119`, `main.rs`
`cmd_ph_picker`); `picker::leave_commands` / `back_commands` callers move to
`palette_view::` (or keep the re-export and delete it in Task 13).

**Step 5:** commit `posh: StackView schema + palette_view presentation split for the session stack`.

### Task 13: the presentation for this iteration

**Files:** `remote/client.rs:305-437` (`palette_commands`, `palette_title`, `open_palette`), `session/client.rs:1000-1065` (`palette_commands`, `open_local_palette`), their tests (`palette_commands_*` in both; `remote/client.rs:4677-4678` asserts the Back row position).

**Step 1: failing tests** — in each client, a test that with a stack top the FIRST row is `Back to <top>` and *Switch session…* is second; without a top the first row is *Switch session…* and no row starts with `Back to`; and (remote) that the title passed to `p.open` is `commands_title(&view, Some(palette_title(..)))`. Update `remote/client.rs:4677` (`off[1]` → `off[0]`).

**Step 3:** both `palette_commands` take `view: &StackView` instead of `back_to: Option<&str>`; the row list starts with `back_row(view)` (when Some) then *Switch session…*; `open_palette` passes `commands_title(&view, Some(&palette_title(..)))`, `open_local_palette` passes `commands_title(&view, None)`. No `format!("Back to …")` or `"to go back"` literal remains outside `palette_view.rs` (add a test in palette_view.rs that greps? no — a `rg` in the self-review is enough).

**Step 5:** commit `posh: Back to <top> is the first palette row; the heading names the predecessor`.

Then the Section 3 merge (attestation + `merge-this-session-async`); run clippy and `--test session_integration` first.

---

## Section 4: leave prompt

**State of the code this section starts from (verified 2026-09-21):** RFC 0005
§3.2 `ui.show` has `view` / `commands` / `title` / `prompt` / `body` / `rows` /
`empty` and no description text; posh-palette (`posh-palette/main.go`,
`showParams` ~125, `Update` ~231, `paletteView` ~526) renders title → filter
input → rows. `remote::palette::choose_standalone(title, rows, empty)` hosts
ONLY the `picker` view on a blank frame; there is no standalone COMMAND-list
chooser. `run()` (`main.rs:54-94`) exits through `exit_with_attach_end` when
no switch / auto-pop follows; `picker::kill_target(target, force)` (~475)
kills locally or over ssh with `--unless-attached` unless forced and returns
a notice string. `picker::AttachEnd { Ended{code,cause} | Lost | Quit }`.
`picker::overlay_open("leave")` is reserved and unused.

### Task 14: RFC 0005 `description` + the standalone command chooser

**Promotion criteria:** N/A (additive protocol field; an old renderer ignores
an unknown `ui.show` param per §9).

**Files:**
- Modify: `docs/rfcs/0005-palette-control-protocol.md` §3.2 table (+ a §9
  compatibility sentence): `description` (string, optional, `palette` and
  `picker`): free text the renderer shows between the heading and the
  filter input, word-wrapped to the panel, never filtered on.
- Modify: `posh-palette/main.go` — `showParams.Description string
  \`json:"description,omitempty"\``; `model.description`; set in the `palette`
  and `picker` arms of `Update` (cleared when absent); `paletteView` and
  `pickerView` write it (dim style, one blank line after) between the title
  and the input. Test in `posh-palette/main_test.go`: a `showMsg{View:
  "palette", Description: "a\nb"}` yields a `View()` containing both lines
  between the title and the prompt.
- Modify: `crates/posh/src/remote/palette.rs` — `Palette::open_described(title,
  description, commands)` (`show` with the extra field; `open` stays as is);
  `pub fn choose_standalone_commands(title, description, commands) ->
  Result<Choice>` sharing `choose_standalone`'s loop (factor the loop into a
  private `run_standalone(palette, first_show: impl FnOnce(&mut Palette))`)
  — a `Choice::Action` for a command's action, `Cancelled` on Esc / renderer
  gone, `Unsupported` if the renderer rejects the view. Tests: `open_described`
  sends the field (socketpair test like `show_dialog_sends_…`); the
  `real_binary_*` round-trip gains a described-palette case (skips without a
  fresh renderer; `just debug-palette-e2e` runs it).
- Commit: `posh: RFC 0005 ui.show description; a standalone command chooser`.

### Task 15: `LeavePolicy`, `leave_candidates`, and the prompt's JSON

**Files:**
- Modify: `crates/posh/src/picker.rs` (beside `auto_pop`), `crates/posh/src/remote/palette_view.rs`.

**Step 1: failing tests**

picker.rs:
```rust
#[test]
fn leave_policy_parses_the_env_spellings() {
    assert_eq!(LeavePolicy::parse(None), LeavePolicy::Ask);
    assert_eq!(LeavePolicy::parse(Some("ask")), LeavePolicy::Ask);
    assert_eq!(LeavePolicy::parse(Some("keep")), LeavePolicy::Keep);
    assert_eq!(LeavePolicy::parse(Some("kill")), LeavePolicy::Kill);
    assert_eq!(LeavePolicy::parse(Some("bogus")), LeavePolicy::Ask); // unknown ⇒ the safe default
}

#[test]
fn leave_candidates_are_the_anonymous_entries_plus_a_live_anonymous_current() {
    let _g = switch_test_guard();
    while stack_pop().is_some() {}
    // stack: :s-1 (anon), box:dev (named), box:s-3 (anon); current :s-9 (anon)
    set_current(":s-1"); set_current_kind(SessionKind::Anonymous); stack_push_current();
    set_current("box:dev"); set_current_kind(SessionKind::Named); stack_push_current();
    set_current("box:s-3"); set_current_kind(SessionKind::Anonymous); stack_push_current();
    set_current(":s-9"); set_current_kind(SessionKind::Anonymous);
    let t = |v: Vec<StackEntry>| v.into_iter().map(|e| e.target).collect::<Vec<_>>();
    // Quit / detach: stack anon entries in order, then the current LAST.
    assert_eq!(t(leave_candidates(Some(&AttachEnd::Quit))), [":s-1", "box:s-3", ":s-9"]);
    assert_eq!(t(leave_candidates(None)), [":s-1", "box:s-3", ":s-9"]);
    // The current ENDED: it is gone, not a candidate.
    let ended = AttachEnd::Ended { code: 0, cause: None };
    assert_eq!(t(leave_candidates(Some(&ended))), [":s-1", "box:s-3"]);
    // Lost: the daemon may live on; still a candidate.
    assert_eq!(t(leave_candidates(Some(&AttachEnd::Lost("x".into())))), [":s-1", "box:s-3", ":s-9"]);
    // A named current adds nothing; an unknown-kind entry is never a candidate.
    set_current_kind(SessionKind::Named); // no downgrade — use a fresh current instead
    set_current("box:named"); set_current_kind(SessionKind::Named);
    assert_eq!(t(leave_candidates(Some(&AttachEnd::Quit))), [":s-1", "box:s-3"]);
    while stack_pop().is_some() {}
}
```

palette_view.rs:
```rust
#[test]
fn leave_prompt_lists_candidates_in_the_description_and_offers_keep_first() {
    let c = vec![entry(":s-1", SessionKind::Anonymous), entry("box:s-3", SessionKind::Anonymous)];
    let p = leave_prompt(&c);
    assert_eq!(p.title, "Leaving — 2 anonymous sessions you created");
    assert_eq!(p.description, "flac:s-1\nbox:s-3"); // display_target / short_target form; hostname-dependent for ":" — assert with starts/ends like commands_title
    let names: Vec<&str> = p.commands.as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Keep them running", "Kill them (kept if other viewports are attached)", "Kill them even with other viewports attached"]);
    assert_eq!(p.commands[0]["action"]["method"], "session.leave");
    assert_eq!(p.commands[0]["action"]["params"]["previous"], "keep");
    assert_eq!(p.commands[1]["action"]["params"]["previous"], "kill");
    assert_eq!(p.commands[2]["action"]["params"]["previous"], "force-kill");
    // One candidate: singular wording.
    assert!(leave_prompt(&c[..1]).title.starts_with("Leaving — 1 anonymous session "));
}
```

**Step 3: implementation**

picker.rs:
```rust
/// `POSH_LEAVE_ANONYMOUS`: what the front door does with the anonymous
/// sessions this viewport created when it leaves posh (design 2026-09-21
/// §4). `Ask` (default) prompts; `Keep` never prompts; `Kill` kills without
/// asking. The env var is the config surface until posh has a config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeavePolicy { Ask, Keep, Kill }
impl LeavePolicy {
    pub fn parse(v: Option<&str>) -> LeavePolicy   // "keep" | "kill" | anything else ⇒ Ask
    pub fn from_env() -> LeavePolicy               // std::env::var("POSH_LEAVE_ANONYMOUS")
}

/// The sessions the leave prompt is about: every `Anonymous` stack entry,
/// bottom first, then the current attach when it is `Anonymous` and did
/// not itself END (an ended session has nothing to kill; a lost one may).
/// `Unknown` never qualifies — the prompt only names what the daemon (or
/// the `:+` fallback) called anonymous.
pub fn leave_candidates(end: Option<&AttachEnd>) -> Vec<StackEntry>
```

palette_view.rs:
```rust
/// The leave prompt (design 2026-09-21 §4): the candidates in the
/// `description` block, the three fates as rows, *Keep* first so Enter
/// keeps. Rows re-issue `session.leave` with a `previous` like the switch
/// dialogs; the front door reads the selection back.
pub struct LeavePrompt { pub title: String, pub description: String, pub commands: Value }
pub fn leave_prompt(candidates: &[StackEntry]) -> LeavePrompt
```
(the rows reuse `leave_commands_for("session.leave", None)` minus the Cancel
row — or keep Cancel as a fourth row meaning keep; choose Keep-first + no
Cancel so Esc and Enter both keep. Note in the doc.)

Commit: `posh: LeavePolicy, leave_candidates, and the leave prompt view`.

### Task 16: the prompt in `run()` and the kills

**Files:** `crates/posh/src/main.rs` (`run`, `exit_with_attach_end`), `crates/posh/src/picker.rs` (a pure decision fn + a kill runner with a seam).

**Step 1: failing tests** (picker.rs, pure):
```rust
#[test]
fn leave_action_decision_table() {
    use LeaveAction::*;
    let c = vec![entry(":s-1", SessionKind::Anonymous)];
    // no candidates ⇒ nothing, whatever the policy
    assert_eq!(leave_action(LeavePolicy::Ask, true, false, &[]), Nothing);
    // keep ⇒ nothing, silently
    assert_eq!(leave_action(LeavePolicy::Keep, true, false, &c), Nothing);
    // kill ⇒ kill (unless-attached), no prompt
    assert_eq!(leave_action(LeavePolicy::Kill, true, false, &c), Kill { force: false });
    // ask on a tty, not a signal ⇒ prompt
    assert_eq!(leave_action(LeavePolicy::Ask, true, false, &c), Prompt);
    // ask off-tty or after a signal ⇒ report, keep
    assert_eq!(leave_action(LeavePolicy::Ask, false, false, &c), Report);
    assert_eq!(leave_action(LeavePolicy::Ask, true, true, &c), Report);
}

#[test]
fn run_leave_kills_in_stack_order_current_last_and_collects_every_notice() {
    // a stub killer records the order and fails one host; the runner never stops early
    let order = std::cell::RefCell::new(vec![]);
    let notices = run_leave_kills(&[entry(":s-1", ..), entry("box:s-3", ..), entry(":s-9", ..)], false, |t, f| {
        order.borrow_mut().push(t.to_string());
        if t == "box:s-3" { Err(Error::from("box: ssh: no route")) } else { Ok(format!("killed {t}")) }
    });
    assert_eq!(*order.borrow(), [":s-1", "box:s-3", ":s-9"]);
    assert_eq!(notices, ["killed :s-1", "box:s-3 not killed: box: ssh: no route", "killed :s-9"]);
}
```

**Step 3: implementation**

picker.rs:
```rust
#[derive(Debug, PartialEq, Eq)]
pub enum LeaveAction { Nothing, Prompt, Kill { force: bool }, Report }
/// Pure: what to do on leaving, from the policy, whether stdin+stdout are a
/// tty, whether the attach ended by a signal (`util::signal_received()` or
/// the AttachEnd's Quit-by-signal marker — read how the clients note it),
/// and the candidates.
pub fn leave_action(policy: LeavePolicy, tty: bool, signaled: bool, candidates: &[StackEntry]) -> LeaveAction
/// Kill each candidate in order (stack bottom first, the current last —
/// `leave_candidates` already orders them), never stopping on a failure;
/// one notice per entry. `kill` is `kill_target` in production.
pub fn run_leave_kills(candidates: &[StackEntry], force: bool, kill: impl FnMut(&str, bool) -> Result<String>) -> Vec<String>
pub fn left_running_notice(candidates: &[StackEntry]) -> String   // "left running: flac:s-1, box:s-3 (POSH_LEAVE_ANONYMOUS=kill to kill on exit)"
```

main.rs `run()`: in the `else` arm before `viewport_status::shutdown()`:
```rust
            let candidates = picker::leave_candidates(end.as_ref());
            let tty = util::is_tty(libc::STDIN_FILENO) && util::is_tty(libc::STDOUT_FILENO);
            match picker::leave_action(picker::LeavePolicy::from_env(), tty, signaled(&end), &candidates) {
                picker::LeaveAction::Nothing => {}
                picker::LeaveAction::Report => eprintln!("posh: {}", picker::left_running_notice(&candidates)),
                picker::LeaveAction::Kill { force } => {
                    for n in picker::run_leave_kills(&candidates, force, picker::kill_target) { eprintln!("posh: {n}"); }
                }
                picker::LeaveAction::Prompt => {
                    let p = remote::palette_view::leave_prompt(&candidates);
                    picker::overlay_open("leave");
                    let choice = remote::palette::choose_standalone_commands(&p.title, &p.description, p.commands);
                    picker::overlay_close("leave");
                    match choice {
                        Ok(remote::palette::Choice::Action { params, .. }) => match picker::Previous::parse(params["previous"].as_str()) {
                            Some(picker::Previous::Kill) => { for n in picker::run_leave_kills(&candidates, false, picker::kill_target) { eprintln!("posh: {n}"); } }
                            Some(picker::Previous::ForceKill) => { for n in picker::run_leave_kills(&candidates, true, picker::kill_target) { eprintln!("posh: {n}"); } }
                            _ => {}
                        },
                        // Cancelled / Unsupported / a renderer error: keep, and say what was left.
                        _ => eprintln!("posh: {}", picker::left_running_notice(&candidates)),
                    }
                }
            }
```
The current session, when a candidate, is last in the list and its attach has
already returned, so its kill goes through the same `kill_target`
(`--unless-attached` protects another viewport). Print notices AFTER
`choose_standalone_commands` restored the tty (it does before returning).

Commit: `posh: ask what to do with the anonymous sessions a viewport created when it leaves (POSH_LEAVE_ANONYMOUS)`.

### Task 17: docs and manual verification

- `docs/features/0016-cross-host-session-switcher.md`: "Stacked switching" gains the kind rule, the predecessor heading, and a "Leaving posh" paragraph (the prompt, the lever, the off-tty report); the trade-off table gains a `leave policy` row (ask / keep / kill; signal: the tuning lever from the design §6).
- `doc/posh.1.scd`: ENVIRONMENT gains `POSH_LEAVE_ANONYMOUS`; the `ph`/palette section describes the prompt in two sentences.
- `docs/rfcs/0005-palette-control-protocol.md` §7 (client methods the renderer issues): `session.leave {previous}` beside `session.switch` / `session.pop`.
- `justfile`: `debug-verify-leave-prompt` (`debug` group, depends on `build-palette`): in a detached tmux pane with an isolated `POSH_DIR` under `.tmp/`, run the worktree `ph :+`, type `exit`-safe filler, keep-switch to a named session created in the same dir, then detach with the palette; capture the pane (the prompt must be visible), send Enter (keep), then print `posh list` showing both sessions alive; a second run with `POSH_LEAVE_ANONYMOUS=kill` prints a list with the anonymous one gone. Model it on `debug-verify-establish-modal` (`justfile:519`).
- `just lint-doc` clean; commit `docs: the leave prompt (FDR 0016), POSH_LEAVE_ANONYMOUS, session.leave in RFC 0005; a debug-verify-leave-prompt recipe`.
- Then the Section 4 merge (attestation + `merge-this-session-async`), and the memory note in `posh-devshell-gotchas` if the recipe taught anything new.
