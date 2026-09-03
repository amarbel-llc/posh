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
    assert_eq!(fields.len(), 7, "row: {fields:?}");
    assert_eq!(fields[0], "itest", "row: {fields:?}"); // NAME
    assert_eq!(fields[3], "0", "row: {fields:?}"); // CLIENTS
    // ACTIVITY prefers the RFC 0013 activity label over the launch cmd once
    // the daemon has one (here, the foreground process name); either way it
    // names the `sleep` process.
    assert!(fields[5].contains("sleep"), "row: {fields:?}"); // ACTIVITY

    // Creating it again is a no-op.
    let out = posh(&dir, &["attach", "--detach", "itest"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("session \"itest\" already exists"),
        "unexpected output: {stdout}"
    );

    // Kill tears down the daemon and removes the socket.
    let out = posh(&dir, &["kill", "itest"]);
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

    // Bare `ph` -> the picker is deferred (FDR 0016): a non-zero error, no hang.
    let out = run(&[]);
    assert!(!out.status.success(), "bare ph should defer: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("picker not yet available"),
        "bare ph stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `ph box:` (host picker) -> deferred.
    let out = run(&["box:"]);
    assert!(!out.status.success(), "ph host: should defer: {out:?}");

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
