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
    fn new(prefix: &str) -> Fixture {
        let dir = test_dir(prefix);
        std::fs::create_dir_all(&dir).unwrap();
        let out = posh(&dir, &["attach", "--detach", "m", "sleep", "300"]);
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
