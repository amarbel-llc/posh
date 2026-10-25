//! End-to-end test for `poshterity record`: exec the real binary to record a
//! deterministic command under a PTY, then replay the produced `.castx`
//! (phase 1) and assert it reproduces the expected screen.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use poshterity::cli::{replay_source, Dump};

fn temp(tag: &str, ext: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("poshterity-rec-{}-{tag}.{ext}", std::process::id()))
}

/// Wait for `child` to exit, killing it (and failing) after `limit`.
fn wait_within(child: &mut Child, limit: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            panic!("poshterity record still running after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn record_size_fixes_the_pty_and_the_header_whatever_stdout_is() {
    let out = temp("size", "castx");
    // `stty size` prints "ROWS COLS" as the child's tty sees it.
    let status = Command::new(env!("CARGO_BIN_EXE_poshterity"))
        .args(["record", "--size", "40x10", "--out", out.to_str().unwrap(), "--", "stty", "size"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn poshterity");
    assert!(status.success(), "{status:?}");
    let doc = std::fs::read_to_string(&out).unwrap();
    let _ = std::fs::remove_file(&out);
    assert!(doc.starts_with("{\"version\":2,\"width\":40,\"height\":10"), "header: {doc:?}");
    let text = String::from_utf8(replay_source(&doc, Dump::Text).unwrap()).unwrap();
    assert!(text.contains("10 40"), "the child saw a 10x40 tty: {text:?}");
}

#[test]
fn record_end_on_eof_stops_a_child_that_would_run_on() {
    let out = temp("eof", "castx");
    let mut child = Command::new(env!("CARGO_BIN_EXE_poshterity"))
        .args(["record", "--end-on-eof", "--out", out.to_str().unwrap(), "--", "sleep", "60"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn poshterity");
    std::thread::sleep(Duration::from_millis(200));
    drop(child.stdin.take()); // EOF on poshterity's stdin
    wait_within(&mut child, Duration::from_secs(10));
    assert!(std::fs::metadata(&out).is_ok_and(|m| m.len() > 0), "the recording is finalized");
    let _ = std::fs::remove_file(&out);
}

#[test]
fn record_mark_fifo_writes_markers_at_the_moment_they_arrive() {
    let out = temp("mark", "castx");
    let fifo = temp("mark", "fifo");
    let _ = std::fs::remove_file(&fifo);
    // before; wait for a line on the pty (poshterity's stdin); after.
    let mut child = Command::new(env!("CARGO_BIN_EXE_poshterity"))
        .args([
            "record", "--mark-fifo", fifo.to_str().unwrap(), "--out", out.to_str().unwrap(),
            "--", "sh", "-c", "echo before; read x; echo after",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn poshterity");
    let start = Instant::now();
    while !fifo.exists() {
        assert!(start.elapsed() < Duration::from_secs(5), "the marker fifo never appeared");
        std::thread::sleep(Duration::from_millis(25));
    }
    std::thread::sleep(Duration::from_millis(400)); // "before" is out
    std::fs::OpenOptions::new().write(true).open(&fifo).unwrap().write_all(b"mid\n").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    child.stdin.as_mut().unwrap().write_all(b"\n").unwrap(); // releases `read`
    wait_within(&mut child, Duration::from_secs(10));
    assert!(!fifo.exists(), "the fifo is removed when recording ends");

    let doc = std::fs::read_to_string(&out).unwrap();
    let _ = std::fs::remove_file(&out);
    assert!(doc.contains("\"m\",\"mid\""), "a marker event: {doc:?}");
    // Replayed to the marker: "before" is on screen, "after" is not yet.
    let mut player = poshterity::player::Player::from_source(&doc).unwrap();
    assert!(player.step_to_marker("mid"));
    let text = player.terminal().dump_text();
    assert!(text.contains("before") && !text.contains("after"), "{text:?}");
}

#[test]
fn record_produces_a_castx_that_replays_to_the_screen() {
    let out = std::env::temp_dir().join(format!("poshterity-record-{}.castx", std::process::id()));

    // stdin is /dev/null (not a tty) -> raw mode is skipped, input loop sees
    // EOF; the child's PTY output is still teed and recorded.
    let status = Command::new(env!("CARGO_BIN_EXE_poshterity"))
        .args([
            "record",
            "--out",
            out.to_str().unwrap(),
            "--",
            "printf",
            "hello world",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn poshterity");
    assert!(status.success(), "record exited unsuccessfully: {status:?}");

    let doc = std::fs::read_to_string(&out).expect("read recording");
    let _ = std::fs::remove_file(&out);

    // A real .castx: header line + at least one event row.
    assert!(doc.starts_with("{\"version\":2"), "header: {doc:?}");

    let text =
        String::from_utf8(replay_source(&doc, Dump::Text).expect("replay recording")).unwrap();
    assert!(text.contains("hello world"), "replayed screen: {text:?}");
}
