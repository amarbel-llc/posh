//! End-to-end exercise of the session daemon through the posh binary:
//! create a detached session running `sleep`, list it, then kill it.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn posh(dir: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_posh"))
        .args(args)
        .env("POSH_DIR", dir)
        .env_remove("POSH_SESSION")
        .env_remove("POSH_GROUP")
        .output()
        .expect("run posh")
}

fn wait_for<F: FnMut() -> bool>(mut cond: F, what: &str) {
    for _ in 0..100 {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// Unix socket paths cap at ~107 bytes; the deeply nested TMPDIR that
/// `nix develop` exports blows that through temp_dir(), so fall back to
/// /tmp when the base is already long.
fn test_dir(prefix: &str) -> PathBuf {
    let base = std::env::temp_dir();
    let base = if base.as_os_str().len() > 40 {
        PathBuf::from("/tmp")
    } else {
        base
    };
    base.join(format!("{prefix}-{}", std::process::id()))
}

/// A session daemon is double-forked from whatever created it — a CLI here,
/// but also long-lived processes (the relay, `posh-server mux`, and for
/// push-cmd a daemon). It must not keep any of its creator's descriptors
/// open: a leaked socket or PTY outlives its owner and hides its EOF.
/// A pipe opened here WITHOUT close-on-exec rides into `posh` through
/// `exec`, then into the daemon through `fork`; the daemon must not hold it.
// macOS gap (posh#214): the shedding itself works everywhere
// (`close_inherited_fds` sweeps to `_SC_OPEN_MAX`); only this check reads
// `/proc/<pid>/fd` — `lsof -p` would do on macOS.
#[cfg(target_os = "linux")]
#[test]
fn a_new_session_daemon_sheds_its_creators_descriptors() {
    use std::os::unix::fs::MetadataExt;

    let dir = test_dir("posh-shed");
    std::fs::create_dir_all(&dir).unwrap();
    let mut pipe = [0 as libc::c_int; 2];
    // SAFETY: pipe(2) fills the two-element array we own.
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0, "pipe");
    let inode = std::fs::metadata(format!("/proc/self/fd/{}", pipe[0])).unwrap().ino();
    let leaked = format!("pipe:[{inode}]");

    let out = posh(&dir, &["attach", "--detach", "shed", "sleep", "300"]);
    assert!(out.status.success(), "attach --detach failed: {out:?}");

    // `posh list`'s PID column is the session SHELL; the daemon is its parent.
    let mut shell_pid = String::new();
    wait_for(
        || {
            let out = posh(&dir, &["list"]);
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            match stdout.lines().nth(1).map(|row| row.split('\t').collect::<Vec<_>>()) {
                Some(fields) if fields.len() > 2 && fields[0] == "shed" && !fields[2].is_empty() => {
                    shell_pid = fields[2].to_string();
                    true
                }
                _ => false,
            }
        },
        "the session and its shell pid to appear in list",
    );
    let status = std::fs::read_to_string(format!("/proc/{shell_pid}/status")).unwrap();
    let daemon_pid = status
        .lines()
        .find_map(|l| l.strip_prefix("PPid:"))
        .map(str::trim)
        .expect("the shell's PPid")
        .to_string();

    let held: Vec<String> = std::fs::read_dir(format!("/proc/{daemon_pid}/fd"))
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .map(|p| p.display().to_string())
        .collect();

    let _ = posh(&dir, &["kill", "shed"]);
    // SAFETY: closing the two fds pipe(2) gave us.
    unsafe {
        libc::close(pipe[0]);
        libc::close(pipe[1]);
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !held.contains(&leaked),
        "daemon {daemon_pid} still holds its creator's {leaked}: {held:?}"
    );
}

/// Read `Tag` frames (tag byte, u32 LE length, payload) off `stream` until one
/// tagged `want` arrives; `None` when the read times out first.
fn read_until_tag(stream: &mut std::os::unix::net::UnixStream, want: u8) -> Option<Vec<u8>> {
    use std::io::Read;
    loop {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).ok()?;
        let len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).ok()?;
        if header[0] == want {
            return Some(payload);
        }
    }
}

/// RFC 0016 §4: a push-cmd request makes the daemon create an anonymous
/// session running `$POSH_ESCAPE_CMD` in the session's directory (ADR 0008),
/// then re-home THE REQUESTING connection onto it with `Tag::Switch`. A
/// repeat of the same token creates nothing.
#[test]
fn a_push_cmd_request_creates_a_session_here_and_rehomes_the_requester() {
    use posh_proto::caps;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    const TAG_CLIENT_CAPS: u8 = 15;
    const TAG_SWITCH: u8 = 17;

    let dir = test_dir("posh-push");
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let out_file = dir.join("pushed.out");
    let probe = dir.join("probe.sh");
    std::fs::write(
        &probe,
        format!(
            "pwd > {out}.tmp; echo \"$POSH_SESSION\" >> {out}.tmp; mv {out}.tmp {out}; exec sleep 300\n",
            out = out_file.display()
        ),
    )
    .unwrap();

    // The parent runs in `dir`; its daemon (and so the push) inherits the
    // escape command. Kernel cwd on Linux, start dir elsewhere: both `dir`.
    let out = Command::new(env!("CARGO_BIN_EXE_posh"))
        .args(["attach", "--detach", "par", "sleep", "300"])
        .current_dir(&dir)
        .env("POSH_DIR", &dir)
        .env("POSH_ESCAPE_CMD", format!("sh {}", probe.display()))
        .env_remove("POSH_SESSION")
        .env_remove("POSH_GROUP")
        .output()
        .expect("run posh");
    assert!(out.status.success(), "attach --detach failed: {out:?}");

    let sock = dir.join("default").join("par");
    wait_for(|| UnixStream::connect(&sock).is_ok(), "the parent's socket");
    let mut conn = UnixStream::connect(&sock).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let request = |conn: &mut UnixStream, token: u64| {
        let table = caps::encode_table(&[caps::encode_push_cmd_request(token)]);
        let mut frame = vec![TAG_CLIENT_CAPS];
        frame.extend_from_slice(&(table.len() as u32).to_le_bytes());
        frame.extend_from_slice(&table);
        conn.write_all(&frame).unwrap();
    };

    request(&mut conn, 7);
    let target = read_until_tag(&mut conn, TAG_SWITCH).expect("the requester is re-homed");
    let nul = target.iter().position(|b| *b == 0).expect("group\\0session");
    let (group, pushed) = (
        String::from_utf8_lossy(&target[..nul]).into_owned(),
        String::from_utf8_lossy(&target[nul + 1..]).into_owned(),
    );
    assert_eq!((group.as_str(), pushed.as_str()), ("default", "s-1"));

    wait_for(|| out_file.exists(), "the pushed command to run");
    let ran = std::fs::read_to_string(&out_file).unwrap();
    let mut lines = ran.lines();
    assert_eq!(lines.next(), Some(dir.to_str().unwrap()), "runs in the session's directory");
    assert_eq!(lines.next(), Some("s-1"), "as the new session");
    // Forked from a daemon, the new daemon must log to ITS OWN file: the
    // creator's inherited logger is dropped before its fd is shed, else the
    // fd number is closed twice (under the new log or the PTY master).
    let log = std::fs::read_to_string(dir.join("default").join("s-1.log")).unwrap_or_default();
    assert!(log.contains("daemon started session=s-1"), "the pushed daemon logs: {log:?}");

    let listed = String::from_utf8_lossy(&posh(&dir, &["list", "--json"]).stdout).into_owned();
    assert!(
        listed.contains("\"name\":\"s-1\"") && listed.contains("\"kind\":\"anonymous\""),
        "the pushed session is anonymous: {listed}"
    );

    // The same token again: served already, so no second switch or session.
    request(&mut conn, 7);
    conn.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    assert!(read_until_tag(&mut conn, TAG_SWITCH).is_none(), "a repeat is ignored");
    let names = String::from_utf8_lossy(&posh(&dir, &["list", "--short"]).stdout).into_owned();
    assert!(!names.lines().any(|l| l == "s-2"), "no second session: {names}");

    drop(conn);
    let _ = posh(&dir, &["kill", "s-1"]);
    let _ = posh(&dir, &["kill", "par"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// RFC 0013 §5.2 / RFC 0016 §2: the attach replay frame already answers a
/// client's Init requests — activity label, kind, push-cmd offer — so an idle
/// session (no later frame) still tells a fresh viewport all three.
#[test]
fn an_idle_sessions_replay_frame_answers_the_attach_requests() {
    use posh_proto::caps;
    use posh_proto::frame::ServerFrame;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    const TAG_INIT: u8 = 7;
    const TAG_FRAME: u8 = 12;

    let dir = test_dir("posh-replay-caps");
    std::fs::create_dir_all(&dir).unwrap();
    let out = posh(&dir, &["attach", "--detach", "idle", "sh", "-c", "echo ready; exec sleep 300"]);
    assert!(out.status.success(), "attach --detach failed: {out:?}");
    let sock = dir.join("default").join("idle");
    wait_for(|| UnixStream::connect(&sock).is_ok(), "the session's socket");
    // Let the shell's output land so the daemon has something to replay.
    std::thread::sleep(Duration::from_millis(500));

    let mut conn = UnixStream::connect(&sock).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut init = Vec::new();
    init.extend_from_slice(&24u16.to_le_bytes());
    init.extend_from_slice(&80u16.to_le_bytes());
    init.extend_from_slice(&caps::encode_table(&caps::own_table(&[
        caps::Cap { id: caps::CAP_SESSION_ACTIVITY, payload: vec![] },
        caps::encode_push_cmd(),
    ])));
    let mut frame = vec![TAG_INIT];
    frame.extend_from_slice(&(init.len() as u32).to_le_bytes());
    frame.extend_from_slice(&init);
    conn.write_all(&frame).unwrap();

    let first = read_until_tag(&mut conn, TAG_FRAME).expect("the replay frame");
    let first = ServerFrame::decode(&first).unwrap();
    let has = |id| caps::find(&first.caps, id).is_some();
    let answered = (
        has(caps::CAP_SESSION_ACTIVITY),
        has(caps::CAP_SESSION_KIND),
        has(caps::CAP_PUSH_CMD),
    );


    // The M2 bridge's shape: a plain Init, then the requests in a later
    // Tag::ClientCaps. Nothing is printed meanwhile, yet the answer arrives.
    const TAG_CLIENT_CAPS: u8 = 15;
    let mut bridged = UnixStream::connect(&sock).unwrap();
    bridged.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let send = |s: &mut UnixStream, tag: u8, payload: &[u8]| {
        let mut f = vec![tag];
        f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        f.extend_from_slice(payload);
        s.write_all(&f).unwrap();
    };
    let mut plain = init[..4].to_vec();
    plain.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
    send(&mut bridged, TAG_INIT, &plain);
    let _replay = read_until_tag(&mut bridged, TAG_FRAME).expect("the plain replay");
    send(
        &mut bridged,
        TAG_CLIENT_CAPS,
        &caps::encode_table(&[caps::Cap { id: caps::CAP_SESSION_ACTIVITY, payload: vec![] }, caps::encode_push_cmd()]),
    );
    let later = read_until_tag(&mut bridged, TAG_FRAME).map(|f| ServerFrame::decode(&f).unwrap());
    let bridged_offer = later.is_some_and(|f| caps::find(&f.caps, caps::CAP_PUSH_CMD).is_some());

    drop(conn);
    drop(bridged);
    let _ = posh(&dir, &["kill", "idle"]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(answered, (true, true, true), "activity, kind, offer on the replay frame");
    assert!(bridged_offer, "a ClientCaps request on an idle session is answered");
}

/// FDR 0020: `posh fork` is retired in favor of push-cmd. Muscle memory gets
/// the replacement, not an unknown-command error.
#[test]
fn fork_is_retired_and_names_its_replacement() {
    let dir = test_dir("posh-fork-retired");
    std::fs::create_dir_all(&dir).unwrap();
    for spelling in ["fork", "f"] {
        let out = posh(&dir, &[spelling]);
        assert!(!out.status.success(), "`posh {spelling}` must fail: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("fork was removed; use 'posh start -- <cmd>' inside the session (FDR 0020)"),
            "`posh {spelling}` stderr: {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn daemon_lifecycle_create_list_kill() {
    let dir = test_dir("posh-itest");
    std::fs::create_dir_all(&dir).unwrap();

    // Create without attaching; the daemon runs `sleep 300` in a PTY.
    let out = posh(&dir, &["attach", "--detach", "itest", "sleep", "300"]);
    assert!(out.status.success(), "attach --detach failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("session \"itest\" created"),
        "unexpected output: {stdout}"
    );

    // The session shows up in list with zero attached clients.
    wait_for(
        || {
            let out = posh(&dir, &["list", "--short"]);
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l == "itest")
        },
        "session to appear in list",
    );
    // The default `posh list` output now pipes RFC 0003 NDJSON to the
    // `mesa` renderer, which on a non-tty pipe (as here, via
    // `Command::output()`) prints a plain header line plus one
    // TAB-separated line per row (purse-first#185).
    let out = posh(&dir, &["list"]);
    assert!(
        out.status.success(),
        "list failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "expected header + 1 row: {stdout}");
    let fields: Vec<&str> = lines[1].split('\t').collect();
    // NAME STATUS PID CLIENTS KIND CWD STARTED-IN ACTIVITY ECHO
    assert_eq!(fields.len(), 9, "row: {fields:?}");
    assert_eq!(fields[0], "itest", "row: {fields:?}"); // NAME
    assert_eq!(fields[3], "0", "row: {fields:?}"); // CLIENTS
    // A plain `attach --detach <name>` creates a named session (2026-09-21
    // session-stack plan §1), and the daemon reports that kind on Info.
    assert_eq!(fields[4], "named", "row: {fields:?}"); // KIND
    // ACTIVITY prefers the RFC 0013 activity label over the launch cmd once
    // the daemon has one (here, the foreground process name); either way it
    // names the `sleep` process.
    assert!(fields[7].contains("sleep"), "row: {fields:?}"); // ACTIVITY

    // Creating it again is a no-op.
    let out = posh(&dir, &["attach", "--detach", "itest"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("session \"itest\" already exists"),
        "unexpected output: {stdout}"
    );

    // FDR 0016: `--unless-attached` kills a session nothing is attached to
    // (the switcher's non-forced cleanup), reporting it the same way.
    let out = posh(&dir, &["kill", "--unless-attached", "itest"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("killed session itest"),
        "kill output: {stdout}"
    );
    wait_for(
        || {
            let out = posh(&dir, &["list", "--short"]);
            !String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l == "itest")
        },
        "session to disappear after kill",
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_sends_command_into_new_session() {
    let dir = test_dir("posh-itest-run");
    std::fs::create_dir_all(&dir).unwrap();

    // `run` must create the session (default shell) and ack the command.
    let out = posh(&dir, &["run", "runtest", "--", "true"]);
    assert!(out.status.success(), "run failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("session \"runtest\" created"),
        "run output: {stdout}"
    );
    assert!(stdout.contains("command sent"), "run output: {stdout}");

    let out = posh(&dir, &["kill", "runtest"]);
    assert!(out.status.success(), "kill failed: {out:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn start_detach_creates_then_idempotent() {
    let dir = test_dir("posh-itest-start");
    std::fs::create_dir_all(&dir).unwrap();

    // `start --detach` creates the session and returns (the FDR 0010 ensure,
    // shared with `attach --detach`).
    let out = posh(&dir, &["start", "--detach", "stest", "sleep", "300"]);
    assert!(out.status.success(), "start --detach failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("session \"stest\" created"),
        "start output: {stdout}"
    );

    // A re-spawn is idempotent.
    let out = posh(&dir, &["start", "--detach", "stest"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("session \"stest\" already exists"),
        "start re-spawn output: {stdout}"
    );

    let _ = posh(&dir, &["kill", "stest"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn start_strict_errors_on_existing_session() {
    let dir = test_dir("posh-itest-start-strict");
    std::fs::create_dir_all(&dir).unwrap();

    // Create it detached, then a plain `start` of the same name must error
    // (strict create) — and it errors BEFORE touching a tty, so no PTY needed.
    let out = posh(&dir, &["start", "--detach", "dup", "sleep", "300"]);
    assert!(out.status.success(), "start --detach failed: {out:?}");

    let out = posh(&dir, &["start", "dup"]);
    assert!(
        !out.status.success(),
        "strict start should have failed: {out:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already exists"),
        "strict start stderr: {stderr}"
    );

    let _ = posh(&dir, &["kill", "dup"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn start_detach_autoid_creates_session() {
    let dir = test_dir("posh-itest-start-autoid");
    std::fs::create_dir_all(&dir).unwrap();

    // No target -> an auto-id `s-N` session (first free slot is s-1).
    let out = posh(&dir, &["start", "--detach", "--", "sleep", "300"]);
    assert!(
        out.status.success(),
        "start --detach (auto-id) failed: {out:?}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("session \"s-1\" created"),
        "auto-id start output: {stdout}"
    );

    wait_for(
        || {
            let out = posh(&dir, &["list", "--short"]);
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l == "s-1")
        },
        "auto-id session to appear in list",
    );

    let _ = posh(&dir, &["kill", "s-1"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn start_remote_attempts_the_host() {
    let dir = test_dir("posh-itest-start-remote");
    std::fs::create_dir_all(&dir).unwrap();

    // Remote `posh start` is implemented (the FDR 0015 deferred slice): a
    // remote target now probes the host rather than erroring "not yet
    // supported". An unreachable host fails fast at the ssh probe (name
    // resolution), for the named, auto-id, and session-less host forms alike.
    for target in ["me@nohost.invalid:dev", "nohost.invalid:+", "nohost.invalid:"] {
        let out = posh(&dir, &["start", target]);
        assert!(
            !out.status.success(),
            "start {target} on an unreachable host should fail: {out:?}"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("not yet supported"),
            "start {target} should be implemented now: {stderr}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn attach_remote_is_strict_and_literal_names_survive() {
    let dir = test_dir("posh-itest-attach-remote");
    std::fs::create_dir_all(&dir).unwrap();

    // posh#176: a host:session-shaped attach target rides the remote path —
    // against an unreachable host the strictness probe fails fast (BatchMode
    // resolution), instead of being read as a weird local name.
    let out = posh(&dir, &["attach", "nohost.invalid:dev"]);
    assert!(!out.status.success(), "remote attach should fail: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("attach requires a session name"),
        "remote attach misparsed: {stderr}"
    );

    // The RFC 0001 escape hatch survives: a dotted token that PARSES as a
    // host is still a literal local session name under explicit attach.
    let out = posh(&dir, &["attach", "--detach", "my.project", "sleep", "300"]);
    assert!(
        out.status.success(),
        "dotted literal name should create locally: {out:?}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("session \"my.project\" created"),
        "literal-name output: {stdout}"
    );

    let _ = posh(&dir, &["kill", "my.project"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bare_host_and_posh_ssh_are_retired() {
    let dir = test_dir("posh-itest-durable-default");
    std::fs::create_dir_all(&dir).unwrap();

    // FDR 0011: a bare host errors with guidance instead of spawning an
    // ephemeral roaming shell. Against an unreachable host the candidate
    // probe fails fast (BatchMode resolution) and the guidance still prints.
    let out = posh(&dir, &["nohost.invalid"]);
    assert!(!out.status.success(), "bare host should error: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("durable sessions are the default"),
        "bare-host stderr: {stderr}"
    );
    assert!(
        stderr.contains("--ephemeral"),
        "bare-host stderr must hint the opt-out: {stderr}"
    );

    // `posh ssh` is retired with the bare form.
    let out = posh(&dir, &["ssh", "nohost.invalid"]);
    assert!(!out.status.success(), "posh ssh should be retired: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("retired"), "ssh stderr: {stderr}");

    // `start --ephemeral` validates its target shape before any network
    // attempt: a session-shaped target and a local name are both rejected.
    let out = posh(&dir, &["start", "--ephemeral", "nohost.invalid:dev"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("names a session"),
        "ephemeral with a session: {stderr}"
    );
    let out = posh(&dir, &["start", "--ephemeral", "scratch"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("remote-only"),
        "ephemeral with a local name: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ph_argv0_routes_and_defers_picker() {
    let dir = test_dir("posh-itest-ph");
    std::fs::create_dir_all(&dir).unwrap();

    // A `ph` symlink to the posh binary exercises the argv[0] front-door
    // (busybox-style multi-call: argv[0] is the invoking name, not the target).
    let ph = dir.join("ph");
    let _ = std::fs::remove_file(&ph);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_posh"), &ph).unwrap();
    let run = |args: &[&str]| {
        Command::new(&ph)
            .args(args)
            .env("POSH_DIR", &dir)
            .env_remove("POSH_SESSION")
            .env_remove("POSH_GROUP")
            .output()
            .expect("run ph")
    };

    // Bare `ph` off a terminal -> the FDR 0016 picker never launches (the
    // FDR 0011 non-TTY discipline): a non-zero error naming the candidates,
    // no hang. The candidates here are the local create row's siblings —
    // an empty POSH_DIR lists no sessions.
    let out = run(&[]);
    assert!(!out.status.success(), "bare ph should error off a tty: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("needs a terminal"),
        "bare ph stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `ph box:` (host picker) off a terminal -> the same discipline.
    let out = run(&["box:"]);
    assert!(!out.status.success(), "ph host: should error off a tty: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("needs a terminal"),
        "ph host: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `ph host:+` (remote auto-id) now ATTEMPTS the remote (it was deferred);
    // an unreachable host fails fast (resolution) rather than hanging.
    let out = run(&["me@nohost.invalid:+"]);
    assert!(
        !out.status.success(),
        "ph host:+ on an unreachable host should fail: {out:?}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("not yet supported"),
        "ph host:+ should be implemented now: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `ph user@host` (no colon, host-looking) -> a clean host-needs-session hint,
    // NOT posh start's remote-target error.
    let out = run(&["me@nohost.example.com"]);
    assert!(!out.status.success(), "ph @host should guide: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("host with no session"),
        "ph @host stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn attach_strict_errors_on_absent_session() {
    let dir = test_dir("posh-itest-attach-strict");
    std::fs::create_dir_all(&dir).unwrap();

    // Phase B (FDR 0015): bare `posh attach <absent>` errors (no create) — and
    // errors before any tty use, so no PTY is needed.
    let out = posh(&dir, &["attach", "ghost"]);
    assert!(!out.status.success(), "strict attach should fail: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no session"), "strict attach stderr: {stderr}");

    let _ = std::fs::remove_dir_all(&dir);
}
