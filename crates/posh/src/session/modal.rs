//! App-presented modal state (PROTOTYPE — throwaway, unmerged).
//!
//! A process outside the agent scope asks the session daemon to raise a
//! countdown on the attached viewports (`posh modal`); the DAEMON owns the
//! clock and arbitrates the first answer. This module is the pure state
//! machine: no sockets, no clock reads (the caller passes `now` in ms), so
//! every rule is unit-testable. Design:
//! `docs/plans/2026-10-24-app-modals-for-restart.md`.
//!
//! # Opt-in, and how to remove it
//!
//! Off unless `POSH_APP_MODAL=1` (see [`parse_gate`]): a daemon without it
//! answers `disabled`, and a viewer without it does not advertise
//! `CAP_APP_MODAL`, so an ordinary session is unchanged. To delete the whole
//! prototype, remove these and let the compiler find the rest:
//!
//! - this file and `tests/modal_integration.rs` (and `mod modal;` in `mod.rs`);
//! - `Tag::Modal` (`ipc.rs`) and `CAP_APP_MODAL` (`posh-proto/src/caps.rs`);
//! - `daemon.rs`: the `modal` state and `modal_on`, `modal_*` / `merge_poll_timeouts`
//!   helpers, the `Tag::Modal` arm, the poll-timeout merge, the requester reset;
//! - `client.rs`: `modal_banner`, `open_modal_view`, the `modal` / `modal_view_open`
//!   state and its three hooks (receive, `Ctrl-^`, palette events), the
//!   `app.modal.choose` dispatch arm, the cap advertisement;
//! - `remote/palette.rs`: `show_countdown` / `update_countdown` / `hide` + tests;
//! - `session/mod.rs` `cmd_modal` and the `modal` arm in `main.rs`;
//! - `posh-palette/countdown.go`, `countdown_test.go` and the `countdown` /
//!   `ui.update` cases in `main.go`;
//! - the `debug-modal-*` recipes in the justfile.
//!
//! `rg 'PROTOTYPE|Tag::Modal|CAP_APP_MODAL|app\.modal|modal_'` lists every site.
//!
//! Wire: every message is one `Tag::Modal` frame whose payload is a JSON
//! object with an `op`:
//!
//! | direction | `op` | meaning |
//! |---|---|---|
//! | requester → daemon | `request` | start a modal ([`Spec`] fields) |
//! | requester → daemon | `probe` | ask who could see a modal |
//! | daemon → requester | `result` | the single final outcome |
//! | daemon → requester | `probe` | probe answer |
//! | daemon → viewer | `show` | current state, re-sent every [`TICK_MS`] |
//! | daemon → viewer | `clear` | the modal ended |
//! | viewer → daemon | `choose` | the user picked a button |

use std::os::fd::RawFd;

use serde_json::{json, Value};

/// The opt-IN gate `POSH_APP_MODAL`: `1`, `true`, `on` or `yes` (trimmed,
/// case-insensitive) turn the feature on; anything else, unset included,
/// leaves it off. Read by the session DAEMON (from the environment of the
/// process that created the session) and by each local VIEWER at attach. Off,
/// the daemon answers every request `disabled`, and a viewer does not
/// advertise `CAP_APP_MODAL`, so nothing about a session changes.
pub fn parse_gate(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "on" | "yes")
    )
}

/// [`parse_gate`] of `$POSH_APP_MODAL`.
pub fn enabled() -> bool {
    parse_gate(std::env::var("POSH_APP_MODAL").ok().as_deref())
}

/// The `result` a daemon with the gate off gives a request.
pub fn disabled_payload(id: &str) -> Vec<u8> {
    refusal_payload(id, "disabled", "the daemon runs without POSH_APP_MODAL=1")
}

/// The `probe` answer of a daemon with the gate off.
pub fn disabled_probe_payload() -> Vec<u8> {
    json!({"op": "probe", "modal": "disabled", "active": Value::Null, "capable": 0, "attached": 0})
        .to_string()
        .into_bytes()
}

/// How often the daemon re-sends `show` (the viewport displays the remaining
/// time the daemon names, never its own clock).
pub const TICK_MS: u64 = 1_000;

/// The countdown length when a request names none.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// The longest countdown a request may ask for (a day): bounds the deadline
/// arithmetic and what a stuck requester can leave on viewers' screens.
pub const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Button {
    pub id: String,
    pub label: String,
}

/// A validated `request`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub id: String,
    pub title: String,
    pub body: String,
    pub buttons: Vec<Button>,
    pub timeout_ms: u64,
    /// The button id taken when the clock reaches zero.
    pub on_timeout: String,
    /// The button id taken when a viewer dismisses (Esc); `None` = Esc only
    /// hides the view and the countdown continues.
    pub on_dismiss: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Probe,
    Start(Spec),
}

/// Parse a requester's payload. `Err` is a human-readable reason (sent back
/// as a `result` with outcome `invalid`).
pub fn parse_request(payload: &[u8]) -> Result<Request, String> {
    let v: Value = serde_json::from_slice(payload).map_err(|e| format!("not JSON: {e}"))?;
    match v.get("op").and_then(Value::as_str) {
        Some("probe") => return Ok(Request::Probe),
        Some("request") => {}
        other => return Err(format!("unknown op {other:?}")),
    }
    let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let id = text("id");
    if id.is_empty() {
        return Err("request needs an id".into());
    }
    let buttons: Vec<Button> = v
        .get("buttons")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|b| {
                    Some(Button {
                        id: b.get("id")?.as_str()?.to_string(),
                        label: b.get("label")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if buttons.is_empty() {
        return Err("request needs at least one button".into());
    }
    let on_timeout = text("on_timeout");
    if !buttons.iter().any(|b| b.id == on_timeout) {
        return Err(format!("on_timeout {on_timeout:?} is not a button id"));
    }
    let on_dismiss = v.get("on_dismiss").and_then(Value::as_str).map(str::to_string);
    if let Some(d) = &on_dismiss {
        if !buttons.iter().any(|b| &b.id == d) {
            return Err(format!("on_dismiss {d:?} is not a button id"));
        }
    }
    let timeout_ms = v.get("timeout_ms").and_then(Value::as_u64).unwrap_or(DEFAULT_TIMEOUT_MS);
    if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
        return Err(format!("timeout_ms must be 1..={MAX_TIMEOUT_MS}"));
    }
    Ok(Request::Start(Spec {
        id,
        title: text("title"),
        body: text("body"),
        buttons,
        timeout_ms,
        on_timeout,
        on_dismiss,
    }))
}

/// How a modal ended. `kind` is one of `button`, `timeout`, `no-viewer`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    pub kind: &'static str,
    pub button: Option<String>,
}

/// `result` payload for the requester.
pub fn result_payload(id: &str, r: &Resolution) -> Vec<u8> {
    json!({"op": "result", "id": id, "outcome": r.kind, "button": r.button})
        .to_string()
        .into_bytes()
}

/// `result` for a request the daemon refused (`busy` / `invalid`).
pub fn refusal_payload(id: &str, outcome: &str, reason: &str) -> Vec<u8> {
    json!({"op": "result", "id": id, "outcome": outcome, "button": Value::Null, "reason": reason})
        .to_string()
        .into_bytes()
}

/// `probe` answer: what a caller would get if it raised a modal now.
pub fn probe_payload(active: Option<&str>, capable: usize, attached: usize) -> Vec<u8> {
    json!({"op": "probe", "modal": "ok", "active": active, "capable": capable, "attached": attached})
        .to_string()
        .into_bytes()
}

pub fn clear_payload(id: &str) -> Vec<u8> {
    json!({"op": "clear", "id": id}).to_string().into_bytes()
}

/// What one daemon-side tick decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Tick {
    /// Nothing due.
    Idle,
    /// Re-send `show` to the capable viewers.
    Show,
    /// The clock ran out: resolve and clear.
    Resolved(Resolution),
}

/// One running modal (at most one per session — the prototype rejects a
/// second as `busy`).
#[derive(Debug)]
pub struct Run {
    pub spec: Spec,
    /// The requester's connection fd; its `result` goes there when it ends.
    pub requester: RawFd,
    deadline_ms: u64,
    next_show_ms: u64,
}

impl Run {
    /// Start `spec` at `now`. The first `show` is due immediately.
    pub fn start(spec: Spec, requester: RawFd, now: u64) -> Run {
        let deadline_ms = now.saturating_add(spec.timeout_ms);
        Run { spec, requester, deadline_ms, next_show_ms: now }
    }

    pub fn remaining_ms(&self, now: u64) -> u64 {
        self.deadline_ms.saturating_sub(now)
    }

    /// The `show` payload: the daemon's remaining time, not a deadline, so a
    /// viewer needs no synchronised clock.
    pub fn show_payload(&self, now: u64) -> Vec<u8> {
        json!({
            "op": "show",
            "id": self.spec.id,
            "title": self.spec.title,
            "body": self.spec.body,
            "buttons": self.spec.buttons.iter()
                .map(|b| json!({"id": b.id, "label": b.label}))
                .collect::<Vec<_>>(),
            "remaining_ms": self.remaining_ms(now),
            "total_ms": self.spec.timeout_ms,
            "on_dismiss": self.spec.on_dismiss,
        })
        .to_string()
        .into_bytes()
    }

    /// Milliseconds until the next thing is due, for the poll timeout
    /// (never negative).
    pub fn wake_in_ms(&self, now: u64) -> i32 {
        let due = self.next_show_ms.min(self.deadline_ms);
        due.saturating_sub(now).min(i32::MAX as u64) as i32
    }

    pub fn tick(&mut self, now: u64) -> Tick {
        if now >= self.deadline_ms {
            return Tick::Resolved(Resolution {
                kind: "timeout",
                button: Some(self.spec.on_timeout.clone()),
            });
        }
        if now >= self.next_show_ms {
            self.next_show_ms = now + TICK_MS;
            return Tick::Show;
        }
        Tick::Idle
    }

    /// A viewer's `choose`. First valid answer wins (the caller drops the
    /// run on `Some`); a stale id or unknown button is ignored.
    pub fn choose(&self, id: &str, button: &str) -> Option<Resolution> {
        if id != self.spec.id || !self.spec.buttons.iter().any(|b| b.id == button) {
            return None;
        }
        Some(Resolution { kind: "button", button: Some(button.to_string()) })
    }
}

/// A viewer's `choose` payload → `(id, button)`.
pub fn parse_choose(payload: &[u8]) -> Option<(String, String)> {
    let v: Value = serde_json::from_slice(payload).ok()?;
    if v.get("op")?.as_str()? != "choose" {
        return None;
    }
    Some((v.get("id")?.as_str()?.to_string(), v.get("button")?.as_str()?.to_string()))
}

/// The `choose` payload a viewer sends.
pub fn choose_payload(id: &str, button: &str) -> Vec<u8> {
    json!({"op": "choose", "id": id, "button": button}).to_string().into_bytes()
}

/// The viewer's copy of the running modal, built from a daemon `show`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub id: String,
    pub title: String,
    pub body: String,
    pub buttons: Vec<Button>,
    /// The button a viewer's Esc takes; `None` = Esc only hides the view.
    pub on_dismiss: Option<String>,
    pub remaining_ms: u64,
    pub total_ms: u64,
}

/// A daemon → viewer message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Show(View),
    /// The modal with this id ended.
    Clear(String),
}

/// Parse a daemon → viewer payload; `None` for anything else (a viewer
/// ignores ops it does not know).
pub fn parse_notice(payload: &[u8]) -> Option<Notice> {
    let v: Value = serde_json::from_slice(payload).ok()?;
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    match v.get("op")?.as_str()? {
        "clear" => Some(Notice::Clear(text("id")?)),
        "show" => Some(Notice::Show(View {
            id: text("id")?,
            title: text("title").unwrap_or_default(),
            body: text("body").unwrap_or_default(),
            buttons: v
                .get("buttons")?
                .as_array()?
                .iter()
                .filter_map(|b| {
                    Some(Button {
                        id: b.get("id")?.as_str()?.to_string(),
                        label: b.get("label")?.as_str()?.to_string(),
                    })
                })
                .collect(),
            on_dismiss: text("on_dismiss"),
            remaining_ms: v.get("remaining_ms")?.as_u64()?,
            total_ms: v.get("total_ms").and_then(Value::as_u64).unwrap_or(0),
        })),
        _ => None,
    }
}

impl View {
    /// Whole seconds left, rounded up so the banner never reads 0s early.
    pub fn seconds_left(&self) -> u64 {
        self.remaining_ms.div_ceil(1000)
    }

    /// The one-line banner shown over the session's top row while the
    /// countdown runs (banner first; the modal opens on `Ctrl-^`).
    pub fn banner(&self) -> String {
        format!(" {} - {}s left - Ctrl-^ to review ", self.title, self.seconds_left())
    }

    /// The renderer's `commands` for the countdown view (RFC 0005 §5): each
    /// button issues `app.modal.choose {id, button}`.
    pub fn commands(&self) -> Value {
        Value::Array(
            self.buttons
                .iter()
                .map(|b| {
                    json!({
                        "name": b.label,
                        "action": {"method": "app.modal.choose",
                                   "params": {"id": self.id, "button": b.id}},
                    })
                })
                .collect(),
        )
    }
}

/// The restart countdown's two buttons: the cancel id the exit code keys on,
/// and the proceed id a timeout takes.
pub const CANCEL_BUTTON: &str = "cancel";
pub const PROCEED_BUTTON: &str = "now";

/// `posh modal [SESSION] [--probe] [--title T] [--body B] [--timeout SECS]
/// [--id ID] [--on-dismiss cancel|hide]`, parsed.
#[derive(Debug, PartialEq, Eq)]
pub struct CliArgs {
    pub session: Option<String>,
    pub probe: bool,
    /// The `request` / `probe` payload to send.
    pub payload: Vec<u8>,
    /// How long to wait for the daemon's `result` before giving up.
    pub wait_ms: u64,
}

/// Parse `posh modal`'s arguments. `pid` names the default request id.
pub fn parse_cli(args: &[String], pid: u32) -> Result<CliArgs, String> {
    let (mut session, mut probe) = (None, false);
    let mut title = "Restarting agent".to_string();
    let mut body = "The agent restarts when the countdown ends.".to_string();
    let (mut timeout_s, mut id) = (DEFAULT_TIMEOUT_MS / 1000, format!("modal-{pid}"));
    let mut on_dismiss = Some(CANCEL_BUTTON.to_string());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| it.next().cloned().ok_or(format!("{flag} needs a value"));
        match a.as_str() {
            "--probe" => probe = true,
            "--title" => title = value("--title")?,
            "--body" => body = value("--body")?,
            "--id" => id = value("--id")?,
            "--timeout" => {
                timeout_s = value("--timeout")?
                    .parse::<u64>()
                    .ok()
                    .filter(|s| (1..=MAX_TIMEOUT_MS / 1000).contains(s))
                    .ok_or("--timeout wants whole seconds, 1..=86400")?
            }
            "--on-dismiss" => {
                on_dismiss = match value("--on-dismiss")?.as_str() {
                    "cancel" => Some(CANCEL_BUTTON.to_string()),
                    "hide" => None,
                    other => return Err(format!("--on-dismiss is cancel or hide, not {other:?}")),
                }
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag {flag}")),
            name if session.is_none() => session = Some(name.to_string()),
            extra => return Err(format!("unexpected argument {extra:?}")),
        }
    }
    let payload = if probe {
        json!({"op": "probe"})
    } else {
        json!({
            "op": "request", "id": id, "title": title, "body": body,
            "buttons": [
                {"id": CANCEL_BUTTON, "label": "Cancel restart"},
                {"id": PROCEED_BUTTON, "label": "Restart now"},
            ],
            "on_timeout": PROCEED_BUTTON, "on_dismiss": on_dismiss,
            "timeout_ms": timeout_s * 1000,
        })
    };
    Ok(CliArgs {
        session,
        probe,
        payload: payload.to_string().into_bytes(),
        wait_ms: timeout_s * 1000 + 10_000,
    })
}

/// The process exit code for a daemon reply: `0` proceed (a timeout, "restart
/// now", or no viewer to ask), `10` the user cancelled, `11` busy, `12`
/// invalid, `13` the feature is off in that daemon (so the caller can tell
/// "nobody was asked" from "nobody objected"). Probes exit `0`, or `13` when
/// disabled.
pub fn exit_code(reply: &Value) -> i32 {
    match reply.get("op").and_then(Value::as_str) {
        Some("probe") if reply["modal"] == "disabled" => 13,
        Some("probe") => 0,
        _ => match (reply["outcome"].as_str(), reply["button"].as_str()) {
            (Some("button"), Some(CANCEL_BUTTON)) => 10,
            (Some("busy"), _) => 11,
            (Some("invalid"), _) => 12,
            (Some("disabled"), _) => 13,
            _ => 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_absurd_timeout_is_refused_and_a_late_clock_cannot_overflow() {
        assert!(parse_request(&req(json!({"timeout_ms": MAX_TIMEOUT_MS}))).is_ok());
        assert!(parse_request(&req(json!({"timeout_ms": MAX_TIMEOUT_MS + 1}))).is_err());
        assert!(parse_request(&req(json!({"timeout_ms": u64::MAX}))).is_err());
        assert!(parse_cli(&argv(&["--timeout", "86400"]), 1).is_ok());
        assert!(parse_cli(&argv(&["--timeout", "86401"]), 1).is_err());
        assert!(parse_cli(&argv(&["--timeout", "18446744073709551"]), 1).is_err());
        // Even with a clock near the top of u64 the deadline saturates.
        let run = Run::start(spec(), 7, u64::MAX - 5);
        assert_eq!(run.remaining_ms(u64::MAX - 5), 5, "saturated, not wrapped to a tiny value");
    }

    #[test]
    fn the_gate_is_opt_in_and_strict() {
        for on in ["1", "true", "ON", " yes "] {
            assert!(parse_gate(Some(on)), "{on:?}");
        }
        for off in [None, Some(""), Some("0"), Some("maybe"), Some("2"), Some("false")] {
            assert!(!parse_gate(off), "{off:?}");
        }
    }

    #[test]
    fn a_disabled_daemon_answers_with_its_own_outcome_and_exit_code() {
        let v: Value = serde_json::from_slice(&disabled_payload("r1")).unwrap();
        assert_eq!((v["outcome"].as_str(), v["id"].as_str()), (Some("disabled"), Some("r1")));
        assert_eq!(exit_code(&v), 13, "not 0: the caller must not read it as 'proceed'");
        let p: Value = serde_json::from_slice(&disabled_probe_payload()).unwrap();
        assert_eq!(p["modal"], "disabled");
        assert_eq!(exit_code(&p), 13);
    }

    #[test]
    fn a_show_payload_roundtrips_into_the_viewers_view() {
        let run = Run::start(spec(), 7, 1_000);
        let Some(Notice::Show(v)) = parse_notice(&run.show_payload(11_400)) else { panic!() };
        assert_eq!((v.id.as_str(), v.title.as_str()), ("r1", "Restarting"));
        assert_eq!(v.buttons.len(), 2);
        assert_eq!((v.remaining_ms, v.total_ms), (19_600, 30_000));
        assert_eq!(v.on_dismiss.as_deref(), Some("cancel"));
        assert_eq!(v.seconds_left(), 20, "19.6 s rounds up");
    }

    #[test]
    fn a_clear_payload_parses_and_unknown_ops_are_ignored() {
        assert_eq!(parse_notice(&clear_payload("r1")), Some(Notice::Clear("r1".into())));
        assert_eq!(parse_notice(br#"{"op":"result","id":"r1"}"#), None);
        assert_eq!(parse_notice(b"junk"), None);
    }

    #[test]
    fn the_banner_names_the_time_and_the_chord_and_never_reads_zero_early() {
        let run = Run::start(spec(), 7, 0);
        let Some(Notice::Show(v)) = parse_notice(&run.show_payload(29_900)) else { panic!() };
        let b = v.banner();
        assert!(b.contains("1s left") && b.contains("Ctrl-^") && b.contains("Restarting"), "{b}");
    }

    #[test]
    fn the_buttons_become_renderer_commands_that_choose() {
        let run = Run::start(spec(), 7, 0);
        let Some(Notice::Show(v)) = parse_notice(&run.show_payload(0)) else { panic!() };
        let cmds = v.commands();
        assert_eq!(cmds[0]["name"], "Cancel");
        assert_eq!(cmds[0]["action"]["method"], "app.modal.choose");
        assert_eq!(cmds[0]["action"]["params"], json!({"id": "r1", "button": "cancel"}));
        assert_eq!(cmds[1]["action"]["params"]["button"], "now");
    }

    #[test]
    fn cli_defaults_make_a_30_second_restart_countdown() {
        let c = parse_cli(&[], 42).unwrap();
        assert_eq!((c.session.as_deref(), c.probe, c.wait_ms), (None, false, 40_000));
        let Request::Start(s) = parse_request(&c.payload).unwrap() else { panic!() };
        assert_eq!(s.id, "modal-42");
        assert_eq!(s.timeout_ms, 30_000);
        assert_eq!(s.on_timeout, PROCEED_BUTTON, "timeout proceeds");
        assert_eq!(s.on_dismiss.as_deref(), Some(CANCEL_BUTTON), "Esc cancels");
    }

    #[test]
    fn cli_flags_override_and_name_the_session() {
        let c = parse_cli(&argv(&["box", "--timeout", "5", "--title", "T", "--on-dismiss", "hide"]), 1).unwrap();
        assert_eq!(c.session.as_deref(), Some("box"));
        let Request::Start(s) = parse_request(&c.payload).unwrap() else { panic!() };
        assert_eq!((s.timeout_ms, s.title.as_str(), s.on_dismiss), (5_000, "T", None));
    }

    #[test]
    fn cli_probe_sends_a_probe() {
        let c = parse_cli(&argv(&["--probe", "box"]), 1).unwrap();
        assert!(c.probe);
        assert_eq!(parse_request(&c.payload), Ok(Request::Probe));
    }

    #[test]
    fn cli_rejects_bad_arguments() {
        for bad in [
            &["--timeout", "0"][..],
            &["--timeout", "x"],
            &["--timeout"],
            &["--on-dismiss", "explode"],
            &["--wat"],
            &["a", "b"],
        ] {
            assert!(parse_cli(&argv(bad), 1).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn exit_codes_separate_proceed_from_cancel() {
        let code = |v: Value| exit_code(&v);
        assert_eq!(code(json!({"op":"result","outcome":"timeout","button":"now"})), 0);
        assert_eq!(code(json!({"op":"result","outcome":"no-viewer","button":null})), 0);
        assert_eq!(code(json!({"op":"result","outcome":"button","button":"now"})), 0);
        assert_eq!(code(json!({"op":"result","outcome":"button","button":"cancel"})), 10);
        assert_eq!(code(json!({"op":"result","outcome":"busy","button":null})), 11);
        assert_eq!(code(json!({"op":"result","outcome":"invalid","button":null})), 12);
        assert_eq!(code(json!({"op":"probe","capable":0})), 0);
    }

    fn req(extra: Value) -> Vec<u8> {
        let mut v = json!({
            "op": "request", "id": "r1", "title": "Restarting", "body": "soon",
            "buttons": [{"id": "cancel", "label": "Cancel"}, {"id": "now", "label": "Now"}],
            "on_timeout": "now", "on_dismiss": "cancel", "timeout_ms": 30000
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v.to_string().into_bytes()
    }

    fn spec() -> Spec {
        match parse_request(&req(json!({}))).unwrap() {
            Request::Start(s) => s,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_a_full_request() {
        let s = spec();
        assert_eq!(s.id, "r1");
        assert_eq!(s.buttons.len(), 2);
        assert_eq!(s.on_timeout, "now");
        assert_eq!(s.on_dismiss.as_deref(), Some("cancel"));
        assert_eq!(s.timeout_ms, 30_000);
    }

    #[test]
    fn probe_parses_and_defaults_apply() {
        assert_eq!(parse_request(br#"{"op":"probe"}"#), Ok(Request::Probe));
        let s = match parse_request(&req(json!({"timeout_ms": null, "on_dismiss": null}))).unwrap() {
            Request::Start(s) => s,
            other => panic!("{other:?}"),
        };
        assert_eq!(s.timeout_ms, DEFAULT_TIMEOUT_MS, "no timeout names the 30 s default");
        assert_eq!(s.on_dismiss, None, "Esc then only hides the view");
    }

    #[test]
    fn rejects_malformed_requests() {
        assert!(parse_request(b"nope").is_err());
        assert!(parse_request(br#"{"op":"fly"}"#).is_err());
        assert!(parse_request(&req(json!({"id": ""}))).is_err(), "needs an id");
        assert!(parse_request(&req(json!({"buttons": []}))).is_err(), "needs a button");
        assert!(parse_request(&req(json!({"on_timeout": "ghost"}))).is_err());
        assert!(parse_request(&req(json!({"on_dismiss": "ghost"}))).is_err());
        assert!(parse_request(&req(json!({"timeout_ms": 0}))).is_err());
    }

    #[test]
    fn first_show_is_due_at_once_then_every_tick() {
        let mut run = Run::start(spec(), 7, 1_000);
        assert_eq!(run.wake_in_ms(1_000), 0);
        assert_eq!(run.tick(1_000), Tick::Show);
        assert_eq!(run.tick(1_500), Tick::Idle);
        assert_eq!(run.wake_in_ms(1_500), 500);
        assert_eq!(run.tick(2_000), Tick::Show);
    }

    #[test]
    fn the_clock_is_the_daemons_remaining_time() {
        let run = Run::start(spec(), 7, 1_000);
        assert_eq!(run.remaining_ms(1_000), 30_000);
        assert_eq!(run.remaining_ms(11_000), 20_000);
        assert_eq!(run.remaining_ms(99_000), 0, "never underflows");
        let v: Value = serde_json::from_slice(&run.show_payload(11_000)).unwrap();
        assert_eq!(v["remaining_ms"], 20_000);
        assert_eq!(v["op"], "show");
    }

    #[test]
    fn timeout_resolves_to_the_on_timeout_button() {
        let mut run = Run::start(spec(), 7, 0);
        assert_eq!(
            run.tick(30_000),
            Tick::Resolved(Resolution { kind: "timeout", button: Some("now".into()) })
        );
    }

    #[test]
    fn a_late_poll_still_times_out_rather_than_showing() {
        let mut run = Run::start(spec(), 7, 0);
        assert!(matches!(run.tick(45_000), Tick::Resolved(_)));
    }

    #[test]
    fn wake_never_exceeds_the_deadline() {
        let mut run = Run::start(spec(), 7, 0);
        assert_eq!(run.tick(0), Tick::Show);
        assert_eq!(run.wake_in_ms(29_500), 0, "the deadline (30 s) is nearer than the next show (30.0 s) tie");
        let short = Spec { timeout_ms: 400, ..spec() };
        let mut run = Run::start(short, 7, 0);
        assert_eq!(run.tick(0), Tick::Show);
        assert_eq!(run.wake_in_ms(0), 400, "a 400 ms modal wakes at its deadline, not at the 1 s tick");
    }

    #[test]
    fn choose_accepts_a_known_button_for_this_modal_only() {
        let run = Run::start(spec(), 7, 0);
        assert_eq!(
            run.choose("r1", "cancel"),
            Some(Resolution { kind: "button", button: Some("cancel".into()) })
        );
        assert_eq!(run.choose("other", "cancel"), None, "a stale id is ignored");
        assert_eq!(run.choose("r1", "ghost"), None, "an unknown button is ignored");
    }

    #[test]
    fn choose_payload_roundtrips() {
        let p = choose_payload("r1", "cancel");
        assert_eq!(parse_choose(&p), Some(("r1".into(), "cancel".into())));
        assert_eq!(parse_choose(br#"{"op":"show"}"#), None);
        assert_eq!(parse_choose(b"junk"), None);
    }

    #[test]
    fn result_and_probe_payloads_carry_the_facts() {
        let r = Resolution { kind: "no-viewer", button: None };
        let v: Value = serde_json::from_slice(&result_payload("r1", &r)).unwrap();
        assert_eq!(v["outcome"], "no-viewer");
        assert!(v["button"].is_null());
        let v: Value = serde_json::from_slice(&probe_payload(Some("r1"), 1, 2)).unwrap();
        assert_eq!((v["capable"].as_u64(), v["attached"].as_u64()), (Some(1), Some(2)));
        assert_eq!(v["active"], "r1");
        let v: Value = serde_json::from_slice(&refusal_payload("r2", "busy", "r1 is running")).unwrap();
        assert_eq!((v["outcome"].as_str(), v["reason"].as_str()), (Some("busy"), Some("r1 is running")));
    }
}
