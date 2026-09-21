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

**Promotion criteria:** the pre-existing probe-then-attach path (`remote_session_names` + `first_free_autoid`) can be removed once every fleet host runs a `--kind`-aware posh (signal: no `falling back to probe` log line for 30 days). The space-separated `POSH START` line is itself slated for a pipe-delimited go-plugin-style cutover, tracked as posh#202.

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

## Section 2: viewport registry (outline, expand after Section 1 merges)

### Task 8: typed stack entries

- Modify `crates/posh/src/picker.rs:213-297` — `StackEntry { target: String, kind: SessionKind }`, `STACK: Mutex<Vec<StackEntry>>`, `CURRENT: Mutex<Option<StackEntry>>`; `set_current(target)` keeps its signature and records `Unknown`, and a new `set_current_kind(kind)` is called by the entry points once the daemon answered. `stack_push` takes the current entry whole. `stack_top` returns `Option<StackEntry>`.
- Modify `crates/posh/src/main.rs:46-79` — `run()` pushes `picker::current_entry()`; on a `LocalNew` / `RemoteNew` dispatch it calls `picker::set_current_origin_created()` so a daemon answering `Unknown` reads as `Anonymous` (the fallback in design §2).
- Local client (`session/client.rs:187,295`): after `probe_session` / the first `Tag::Info`, call `set_current_kind(info.kind)`. Roaming client: needs Task 9's frame extension.
- Tests: `picker.rs` stack tests updated for the struct; a `current_kind_falls_back_to_anonymous_only_for_a_created_dispatch` test.

### Task 9: kind on the activity frame (relay / bridge → roaming client)

- Modify `crates/posh-proto/src/caps.rs:299-365` — `SessionActivity` gains `kind: SessionKind`; `encode_session_activity` appends the byte AFTER the two strings (a pre-kind decoder rejects trailing bytes today, so bump `SESSION_ACTIVITY_FMT` to 2 and have `decode_session_activity` accept fmt 1 without the byte and fmt 2 with it). Update RFC 0013 §5.1 in place.
- Modify `session/daemon.rs` (`activity_now` / the label attach) to fill `kind`; the relay and M2 bridge forward the entry unchanged.
- Modify `remote/client.rs` — where `CAP_SESSION_ACTIVITY` is decoded for `default_title_with`, also `set_current_kind`.
- Tests: proto round-trips for fmt 1 and 2; the daemon test from Task 3 extended to read the kind off the first frame.

### Task 10: the viewport status socket

- New `crates/posh/src/viewport_status.rs` — bind `<base>/viewports/<pid>.status.sock` + `.status.pid` (mirror `remote/server.rs:2634-2660`), gated by `POSH_VIEWPORT_STATUS` (`0`/`off`/`false`/`no` skips), answering the RFC 0014 §4 one-shot with the `viewport` / `stack` / `overlay` lines from design §2; a reaper like the `remote/` one.
- Modify `main.rs` `run()` — bind before `run_once`, refresh the rendered response whenever the stack or current changes (a `Mutex<String>` the accept thread serves).
- Modify `session/mod.rs` `cmd_status` — `posh status --viewport <pid>`.
- Overlay registration: `picker::overlay_open(kind: &str, over: Option<&str>)` / `overlay_close()` called around `palette.open` in both clients and around `choose_standalone`.
- Tests: bind/answer/reap/skip in the style of `remote/server.rs`'s status-socket tests.

### Task 11: RFC 0014 §5 amendment + man page

- `docs/rfcs/0014-client-introspection-caps.md` §5 UPSTREAM gets the viewport socket's response grammar; `doc/posh.1.scd` `status` gains `--viewport`.

---

## Section 3: StackView + palette_view (outline)

### Task 12: `StackView` schema

- `picker.rs`: `pub struct StackView { top: Option<StackEntry>, depth: usize, current: Option<StackEntry> }` and `pub fn stack_view() -> StackView`. Tests pin the values for empty / one / many.

### Task 13: `remote/palette_view.rs`

- New module holding every function from `StackView` (+ existing inputs) to RFC 0005 JSON: `commands_title(view, rtt_suffix)`, `back_row(view)`, `leave_prompt(candidates)` (used in Section 4), `picker_title(view)`. Both clients' `palette_commands` and `palette_title` call these; no string literal about the stack remains in `session/client.rs` or `remote/client.rs`.
- Tests: golden JSON per `StackView` shape; a test asserting the title stays under 42 columns for a long host.

### Task 14: presentation for this iteration

- `Commands · back: flac:s-1 [+N]`; *Back to X* as the first row when `top.is_some()`. Update the two `palette_commands_*` tests in each client to go through `palette_view`.

---

## Section 4: leave prompt (outline)

### Task 15: `leave_candidates` and the lever

- `picker.rs`: `pub fn leave_candidates(end: Option<&AttachEnd>) -> Vec<StackEntry>` (every `Anonymous` stack entry + the current when `Anonymous` and `end` is not `Ended`); `pub enum LeavePolicy { Ask, Keep, Kill }` parsed from `POSH_LEAVE_ANONYMOUS` (default `Ask`). Six-combination test table.

### Task 16: the prompt in `run()`

- `main.rs`: after `take_switch().or_else(auto_pop)` yields `None`, compute candidates; on `Ask` with a tty and a non-signal end, call `palette::choose_standalone` with `palette_view::leave_prompt(&candidates)` (description block + the three rows, *Keep* first); on `Kill` or a kill choice run `kill_target` per entry in stack order, current last, collecting notices printed after the tty is restored; off-tty / signal ends print `posh: left running: <targets>`.
- Tests: the decision function `leave_action(policy, has_tty, end, candidates) -> LeaveAction` unit-tested; the kill ordering tested with a stub killer.

### Task 17: docs and manual verification

- FDR 0016 amendment (stacked switching gains the kind rule and the leave prompt); `doc/posh.1.scd` ENVIRONMENT gains `POSH_LEAVE_ANONYMOUS`; a `debug-verify-leave-prompt` justfile recipe (`debug` group) driving `ph :+` → keep-switch → detach in a tmux pane and printing the prompt capture and `posh list`.
