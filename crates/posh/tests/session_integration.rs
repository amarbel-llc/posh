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

#[test]
fn daemon_lifecycle_create_list_kill() {
    let dir = std::env::temp_dir().join(format!("posh-itest-{}", std::process::id()));
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
    let out = posh(&dir, &["list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("session_name=itest"),
        "list output: {stdout}"
    );
    assert!(stdout.contains("clients=0"), "list output: {stdout}");
    assert!(stdout.contains("cmd=sleep 300"), "list output: {stdout}");

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
    let dir = std::env::temp_dir().join(format!("posh-itest-run-{}", std::process::id()));
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
