//! PROTOTYPE app-presented modal, end to end through a real session daemon
//! (`session/modal.rs`, docs/plans/2026-10-24-app-modals-for-restart.md): a
//! requester raises a countdown, a viewer that advertised `CAP_APP_MODAL`
//! sees it and may answer, and the DAEMON's clock decides when nobody does.

use posh_proto::caps;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const TAG_INIT: u8 = 7;
const TAG_MODAL: u8 = 19;

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

fn test_dir(prefix: &str) -> PathBuf {
    let base = std::env::temp_dir();
    let base = if base.as_os_str().len() > 40 { PathBuf::from("/tmp") } else { base };
    base.join(format!("{prefix}-{}", std::process::id()))
}

fn send(s: &mut UnixStream, tag: u8, payload: &[u8]) {
    let mut f = vec![tag];
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(payload);
    s.write_all(&f).unwrap();
}

/// Next `Tag::Modal` payload as JSON, skipping every other tag; `None` on
/// read timeout.
fn next_modal(s: &mut UnixStream) -> Option<Value> {
    loop {
        let mut header = [0u8; 5];
        s.read_exact(&mut header).ok()?;
        let len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; len];
        s.read_exact(&mut payload).ok()?;
        if header[0] == TAG_MODAL {
            return serde_json::from_slice(&payload).ok();
        }
    }
}

/// The next modal message with `op`, discarding other ops.
fn next_op(s: &mut UnixStream, op: &str) -> Value {
    loop {
        let v = next_modal(s).unwrap_or_else(|| panic!("timed out waiting for op {op}"));
        if v["op"] == op {
            return v;
        }
    }
}

struct Fixture {
    dir: PathBuf,
    sock: PathBuf,
}

impl Fixture {
    /// A session whose daemon has the feature gate on (`POSH_APP_MODAL=1`).
    fn new(prefix: &str) -> Fixture {
        Fixture::with_gate(prefix, Some("1"))
    }

    fn with_gate(prefix: &str, gate: Option<&str>) -> Fixture {
        let dir = test_dir(prefix);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_posh"));
        cmd.args(["attach", "--detach", "m", "sleep", "300"])
            .env("POSH_DIR", &dir)
            .env_remove("POSH_SESSION")
            .env_remove("POSH_GROUP")
            .env_remove("POSH_APP_MODAL");
        if let Some(g) = gate {
            cmd.env("POSH_APP_MODAL", g);
        }
        let out = cmd.output().expect("run posh");
        assert!(out.status.success(), "attach --detach failed: {out:?}");
        let sock = dir.join("default").join("m");
        wait_for(|| UnixStream::connect(&sock).is_ok(), "the session's socket");
        Fixture { dir, sock }
    }

    fn conn(&self) -> UnixStream {
        let c = UnixStream::connect(&self.sock).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c
    }

    /// A viewer that advertises `CAP_APP_MODAL` (and frames, as a real one).
    fn viewer(&self) -> UnixStream {
        let mut c = self.conn();
        let mut init = Vec::new();
        init.extend_from_slice(&24u16.to_le_bytes());
        init.extend_from_slice(&80u16.to_le_bytes());
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[caps::Cap {
            id: caps::CAP_APP_MODAL,
            payload: vec![],
        }])));
        send(&mut c, TAG_INIT, &init);
        c
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = posh(&self.dir, &["kill", "m"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn request(id: &str, timeout_ms: u64, on_dismiss: Value) -> Vec<u8> {
    json!({
        "op": "request", "id": id, "title": "Restarting agent", "body": "restart soon",
        "buttons": [{"id": "cancel", "label": "Cancel restart"}, {"id": "now", "label": "Restart now"}],
        "on_timeout": "now", "on_dismiss": on_dismiss, "timeout_ms": timeout_ms
    })
    .to_string()
    .into_bytes()
}

#[test]
fn without_the_gate_the_daemon_refuses_and_a_viewer_is_never_shown_anything() {
    // The feature is opt-in (POSH_APP_MODAL on the DAEMON): off, a request is
    // answered `disabled` at once and nothing reaches a viewer.
    let fx = Fixture::with_gate("posh-modal-off", None);
    let mut viewer = fx.viewer();
    let mut req = fx.conn();
    send(&mut req, TAG_MODAL, &request("r1", 30_000, json!("cancel")));
    let r = next_op(&mut req, "result");
    assert_eq!((r["outcome"].as_str(), r["id"].as_str()), (Some("disabled"), Some("r1")));
    send(&mut req, TAG_MODAL, br#"{"op":"probe"}"#);
    assert_eq!(next_op(&mut req, "probe")["modal"], "disabled", "a probe says so too");
    viewer.set_read_timeout(Some(Duration::from_millis(600))).unwrap();
    assert!(next_modal(&mut viewer).is_none(), "the viewer saw no modal traffic");
}

#[test]
fn a_session_that_never_calls_modal_carries_no_modal_traffic_gate_off_or_on() {
    // Inertness proof for the fleet bump: nobody runs `posh modal`, so a viewer
    // that advertises the capability must see no `Tag::Modal` frame, whether
    // or not the daemon has the gate. The viewer still gets its normal frames
    // (the first read below proves the connection is live, not just quiet).
    for (prefix, gate) in [("posh-modal-idle-off", None), ("posh-modal-idle-on", Some("1"))] {
        let fx = Fixture::with_gate(prefix, gate);
        let mut viewer = fx.viewer();
        viewer.set_read_timeout(Some(Duration::from_millis(800))).unwrap();
        let mut header = [0u8; 5];
        viewer.read_exact(&mut header).expect("the viewer is served its normal first frame");
        assert_ne!(header[0], TAG_MODAL, "{prefix}: the first frame is not a modal");
        let len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; len];
        viewer.read_exact(&mut payload).unwrap();
        assert!(next_modal(&mut viewer).is_none(), "{prefix}: no modal traffic without a request");
    }
}

#[test]
fn an_unrecognized_gate_value_leaves_the_feature_off() {
    let fx = Fixture::with_gate("posh-modal-maybe", Some("maybe"));
    let mut req = fx.conn();
    send(&mut req, TAG_MODAL, &request("r1", 30_000, json!("cancel")));
    assert_eq!(next_op(&mut req, "result")["outcome"], "disabled");
}

#[test]
fn with_no_viewer_the_caller_gets_no_viewer_at_once() {
    let fx = Fixture::new("posh-modal-none");
    let mut req = fx.conn();
    let t = Instant::now();
    send(&mut req, TAG_MODAL, &request("r1", 30_000, json!("cancel")));
    let r = next_op(&mut req, "result");
    assert_eq!(r["outcome"], "no-viewer");
    assert!(t.elapsed() < Duration::from_secs(2), "proceeds at once, never waits for nobody");
}

#[test]
fn a_viewer_sees_the_countdown_and_the_daemons_clock_times_it_out() {
    let fx = Fixture::new("posh-modal-timeout");
    let mut viewer = fx.viewer();
    let mut req = fx.conn();
    send(&mut req, TAG_MODAL, &request("r1", 1_500, json!("cancel")));

    let first = next_op(&mut viewer, "show");
    assert_eq!(first["id"], "r1");
    assert_eq!(first["title"], "Restarting agent");
    assert_eq!(first["buttons"].as_array().unwrap().len(), 2);
    let first_ms = first["remaining_ms"].as_u64().unwrap();
    assert!(first_ms <= 1_500 && first_ms > 1_000, "first show carries ~the full time: {first_ms}");

    let second = next_op(&mut viewer, "show");
    assert!(second["remaining_ms"].as_u64().unwrap() < first_ms, "the daemon's clock advances");

    let r = next_op(&mut req, "result");
    assert_eq!((r["outcome"].as_str(), r["button"].as_str()), (Some("timeout"), Some("now")));
    assert_eq!(next_op(&mut viewer, "clear")["id"], "r1", "viewers are told it ended");
}

#[test]
fn a_viewers_cancel_wins_and_the_requester_hears_which_button() {
    let fx = Fixture::new("posh-modal-cancel");
    let mut viewer = fx.viewer();
    let mut req = fx.conn();
    send(&mut req, TAG_MODAL, &request("r1", 30_000, json!("cancel")));
    next_op(&mut viewer, "show");

    send(&mut viewer, TAG_MODAL, json!({"op": "choose", "id": "r1", "button": "cancel"}).to_string().as_bytes());
    let r = next_op(&mut req, "result");
    assert_eq!((r["outcome"].as_str(), r["button"].as_str()), (Some("button"), Some("cancel")));
    next_op(&mut viewer, "clear");

    // A late second answer changes nothing: the modal is over and a new one may start.
    send(&mut viewer, TAG_MODAL, json!({"op": "choose", "id": "r1", "button": "now"}).to_string().as_bytes());
    send(&mut req, TAG_MODAL, &request("r2", 30_000, json!("cancel")));
    assert_eq!(next_op(&mut viewer, "show")["id"], "r2");
}

#[test]
fn a_second_modal_while_one_runs_is_busy_and_probe_reports_viewers() {
    let fx = Fixture::new("posh-modal-busy");
    let mut viewer = fx.viewer();
    let mut req = fx.conn();

    send(&mut req, TAG_MODAL, br#"{"op":"probe"}"#);
    let p = next_op(&mut req, "probe");
    assert_eq!(p["capable"], 1, "one viewer can render a modal (the requester is not one)");
    assert!(p["active"].is_null());

    send(&mut req, TAG_MODAL, &request("r1", 30_000, json!("cancel")));
    next_op(&mut viewer, "show");

    let mut other = fx.conn();
    send(&mut other, TAG_MODAL, &request("r2", 30_000, json!("cancel")));
    let r = next_op(&mut other, "result");
    assert_eq!((r["outcome"].as_str(), r["id"].as_str()), (Some("busy"), Some("r2")));

    send(&mut req, TAG_MODAL, br#"{"op":"probe"}"#);
    assert_eq!(next_op(&mut req, "probe")["active"], "r1");
}

#[test]
fn losing_the_last_viewer_mid_countdown_resolves_no_viewer() {
    let fx = Fixture::new("posh-modal-leave");
    let mut viewer = fx.viewer();
    let mut req = fx.conn();
    send(&mut req, TAG_MODAL, &request("r1", 30_000, json!("cancel")));
    next_op(&mut viewer, "show");
    drop(viewer);
    let r = next_op(&mut req, "result");
    assert_eq!(r["outcome"], "no-viewer", "nobody left to cancel: the caller proceeds");
}

#[test]
fn an_invalid_request_is_refused_not_ignored() {
    let fx = Fixture::new("posh-modal-bad");
    let mut req = fx.conn();
    send(&mut req, TAG_MODAL, br#"{"op":"request","id":"x","buttons":[]}"#);
    assert_eq!(next_op(&mut req, "result")["outcome"], "invalid");
}
