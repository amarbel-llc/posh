//! End-to-end test for `poshterity record`: exec the real binary to record a
//! deterministic command under a PTY, then replay the produced `.castx`
//! (phase 1) and assert it reproduces the expected screen.

use std::process::{Command, Stdio};

use poshterity::cli::{replay_source, Dump};

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
