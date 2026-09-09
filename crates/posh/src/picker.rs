//! FDR 0016 session picker: the row source shared by the top-level `ph`
//! chooser and the in-session palette's *Switch session…* command, the
//! switch hand-off between a client that ended with a selection and the front
//! door that re-attaches to it, and the "leaving a session" choice (keep it
//! running, or kill it once the new attach is up).
//!
//! The renderer draws the rows without knowing what a session is (RFC 0005
//! §3.5); this module is where a row's cells and its `session.switch` target
//! are decided. A target is an RFC 0001 spelling the front door already
//! routes (`crate::ph_parse`), so a selection attaches exactly as typing it
//! would.

use std::sync::Mutex;

use serde_json::{json, Value};

use crate::session::{self, Config, KillOutcome};
use crate::util::{Error, Result};

/// The picker's heading and its no-rows text (RFC 0005 §3.2 `title` /
/// §3.5 `empty`).
pub const TITLE: &str = "sessions";
pub const EMPTY: &str = "(no sessions)";

/// One row: the target a selection attaches to and its display cells —
/// label (activity, else launch command, else blank), session id, host,
/// status. The id rides beside the label so same-label sessions (a fleet of
/// detached workers running one command) stay tellable.
#[derive(Debug, PartialEq, Eq)]
pub struct PickerRow {
    pub target: String,
    pub cells: Vec<String>,
}

/// The RFC 0001 target for `session` on `dest` (`None` = this machine) in
/// `group` (`None` = the default group): `:name`, `:group/name`,
/// `dest:name`, `dest:group/name` — the spelling `crate::ph_parse` routes.
pub fn target_for(dest: Option<&str>, group: Option<&str>, session: &str) -> String {
    let scoped = match group {
        Some(g) if g != "default" => format!("{g}/{session}"),
        _ => session.to_string(),
    };
    format!("{}:{scoped}", dest.unwrap_or(""))
}

/// The rows for one host's entries: `dest` is the `[user@]host` the host
/// cell shows and the target carries; `None` is this machine. A trailing
/// `+ create new session…` row routes to `posh start` there (`:+`).
pub fn rows_for(dest: Option<&str>, group: &str, entries: &[session::PickerEntry]) -> Vec<PickerRow> {
    let host_cell = dest.unwrap_or("local").to_string();
    let mut rows: Vec<PickerRow> = entries
        .iter()
        .map(|e| PickerRow {
            target: target_for(dest, Some(group), &e.name),
            cells: vec![e.label.clone(), e.name.clone(), host_cell.clone(), e.status.clone()],
        })
        .collect();
    rows.push(PickerRow {
        target: target_for(dest, Some(group), "+"),
        cells: vec![
            "+ create new session…".to_string(),
            String::new(),
            host_cell,
            String::new(),
        ],
    });
    rows
}

/// The rows the picker shows: one host (`ph host:`, `scope` = its
/// `(user, host)`), or this machine plus every host with a live mux endpoint
/// (bare `ph`, the in-session switcher) — the connected set. A host whose
/// listing fails is reported on stderr and skipped, never fatal.
pub fn rows(scope: Option<(Option<String>, String)>, group: &str) -> Result<Vec<PickerRow>> {
    let mut rows = Vec::new();
    let mut dests: Vec<String> = Vec::new();
    match scope {
        Some((user, host)) => dests.push(crate::ph_dest(user.as_deref(), &host)),
        None => {
            rows.extend(rows_for(None, group, &session::picker_entries_local(&Config::new(group)?)?));
            match crate::remote::mux::live_endpoint_dests() {
                Ok(d) => dests = d,
                Err(e) => eprintln!("posh: mux endpoints unavailable: {e}"),
            }
        }
    }
    for dest in dests {
        let (user, host) = split_dest(&dest);
        match crate::remote_list_output(user, host, group, "--json")
            .and_then(|json| session::picker_entries_from_json(&json))
        {
            Ok(entries) => rows.extend(rows_for(Some(&dest), group, &entries)),
            Err(e) => eprintln!("posh: {dest}: {e}"),
        }
    }
    Ok(rows)
}

/// `[user@]host` split at the last `@` (ssh's rule); an empty user is absent.
fn split_dest(dest: &str) -> (Option<&str>, &str) {
    match dest.rsplit_once('@') {
        Some((u, h)) if !u.is_empty() => (Some(u), h),
        _ => (None, dest),
    }
}

/// The RFC 0005 §3.5 `rows` array: each row's cells plus a
/// `session.switch {target}` action (§7). No `previous` yet — the client
/// asks about the session it is leaving in a second step
/// ([`leave_commands`]) when there is one.
pub fn rows_json(rows: &[PickerRow]) -> Value {
    Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "cells": r.cells,
                    "action": { "method": "session.switch", "params": { "target": r.target } },
                })
            })
            .collect(),
    )
}

/// The candidate targets (every row but the create rows), for the non-TUI
/// error message.
pub fn candidates(rows: &[PickerRow]) -> String {
    rows.iter()
        .filter(|r| !r.target.ends_with(":+"))
        .map(|r| r.target.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The session group an in-session switch lists and re-attaches in:
/// `$POSH_GROUP`, else `default` (the front door's own resolution).
pub fn default_group() -> String {
    std::env::var("POSH_GROUP").unwrap_or_else(|_| "default".to_string())
}

/// What happens to the session a viewport is LEAVING on a switch (RFC 0005
/// §7 `session.switch.previous`, FDR 0016).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Previous {
    /// Leave it running detached (FDR 0011's default: nothing is reaped).
    Keep,
    /// Kill it once the new attach is up — unless other viewports are still
    /// attached to it, in which case it is kept and the client is told.
    Kill,
    /// Kill it once the new attach is up even with other viewports attached
    /// (they are thrown out, as `posh kill` does).
    ForceKill,
}

impl Previous {
    /// The `previous` parameter's spelling; absent means [`Previous::Keep`].
    pub fn parse(value: Option<&str>) -> Option<Previous> {
        match value {
            None | Some("keep") => Some(Previous::Keep),
            Some("kill") => Some(Previous::Kill),
            Some("force-kill") => Some(Previous::ForceKill),
            Some(_) => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Previous::Keep => "keep",
            Previous::Kill => "kill",
            Previous::ForceKill => "force-kill",
        }
    }
}

/// The second step after a picker row is chosen from INSIDE a session: the
/// palette asking what to do with the session being left. Each command
/// re-issues `session.switch` with the same `target` and a `previous`.
pub fn leave_commands(target: &str, leaving: &str) -> Value {
    let cmd = |name: String, previous: Previous| {
        json!({
            "name": name,
            "action": {
                "method": "session.switch",
                "params": { "target": target, "previous": previous.as_str() },
            },
        })
    };
    json!([
        cmd(format!("Switch, keep {leaving} running"), Previous::Keep),
        cmd(format!("Switch, kill {leaving} (kept if other viewports are attached)"), Previous::Kill),
        cmd(format!("Switch, kill {leaving} even with other viewports attached"), Previous::ForceKill),
        { "name": "Cancel" },
    ])
}

/// A recorded switch: the target to re-attach to and what to do with the
/// session being left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switch {
    pub target: String,
    pub previous: Previous,
}

/// The switch hand-off: a client that dispatched `session.switch` records
/// the switch and ends its attach (quit / detach); the front door reads it
/// back once the attach returns and re-attaches to it (FDR 0016 re-dial).
/// Process-global because the attach entry points (mux channel, bootstrap,
/// local socket) each return their own shape — this is the one seam they
/// all pass through.
static SWITCH: Mutex<Option<Switch>> = Mutex::new(None);
/// The target of the attach in progress — the session a switch would be
/// LEAVING. Set by the attach entry points; read by the front door when a
/// switch asks to kill it.
static CURRENT: Mutex<Option<String>> = Mutex::new(None);
/// A kill the front door armed for the NEW attach to carry out once it is
/// established (`(target, force)`): kill-after-attach, so a failed switch
/// never destroys the session it was leaving.
static PENDING_KILL: Mutex<Option<(String, bool)>> = Mutex::new(None);

pub fn request_switch(target: &str, previous: Previous) {
    *SWITCH.lock().unwrap_or_else(|e| e.into_inner()) = Some(Switch {
        target: target.to_string(),
        previous,
    });
}

pub fn take_switch() -> Option<Switch> {
    SWITCH.lock().unwrap_or_else(|e| e.into_inner()).take()
}

pub fn set_current(target: &str) {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(target.to_string());
}

pub fn current() -> Option<String> {
    CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The title a viewport shows for a session that has set none of its own:
/// `host:session` for the attach in progress — the host as typed minus any
/// `user@` and trailing domain labels (`box` for `me@box.example`), this
/// machine's hostname for a local attach — with the group kept for a
/// non-default one (`box:grp/dev`). `None` before any attach entry point
/// recorded a target. Asserted by both clients on every compose whose model
/// title is empty (a set title always wins), so a switch into a session
/// that never titled itself replaces the previous session's title instead
/// of leaving it stale (the posh#108 first-frame rule leaves an EMPTY title
/// untouched by design).
pub fn default_title() -> Option<String> {
    let cur = current()?;
    let (dest, session) = cur.rsplit_once(':')?;
    let host = if dest.is_empty() {
        crate::remote::mux::hostname()
    } else {
        short_host(dest.rsplit_once('@').map_or(dest, |(_, h)| h))
    };
    Some(format!("{host}:{session}"))
}

/// `box.example.com` → `box`; a bracketed / numeric address is kept whole.
fn short_host(host: &str) -> String {
    let literal = host.starts_with('[') || host.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ':');
    if literal {
        host.to_string()
    } else {
        host.split('.').next().unwrap_or(host).to_string()
    }
}

pub fn arm_kill(target: &str, force: bool) {
    *PENDING_KILL.lock().unwrap_or_else(|e| e.into_inner()) = Some((target.to_string(), force));
}

pub fn disarm_kill() {
    PENDING_KILL.lock().unwrap_or_else(|e| e.into_inner()).take();
}

/// Carry out the armed kill, if any — called by a client once its new
/// attach is established. Returns the one-line notice for the user.
pub fn run_pending_kill() -> Option<String> {
    let (target, force) = PENDING_KILL.lock().unwrap_or_else(|e| e.into_inner()).take()?;
    Some(match kill_target(&target, force) {
        Ok(notice) => notice,
        Err(e) => format!("previous session {target} not killed: {e}"),
    })
}

/// Kill the session `target` names — locally through its daemon socket, or
/// on its host through `posh kill` over ssh — refusing (kept, with a
/// notice) when other viewports are attached unless `force`.
pub fn kill_target(target: &str, force: bool) -> Result<String> {
    match crate::ph_parse(Some(target)) {
        crate::PhRoute::LocalResolve { group, session } => {
            let cfg = Config::new(group.as_deref().unwrap_or("default"))?;
            Ok(kill_notice(target, session::kill_session(&cfg, &session, !force)?))
        }
        crate::PhRoute::RemoteResolve { user, host, group, session } => {
            let dest = crate::ph_dest(user.as_deref(), &host);
            let argv = remote_kill_argv(&crate::remote::sshwrap::SshDest::resolve(&dest), group.as_deref(), &session, force);
            let out = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|e| Error::Msg(format!("cannot exec ssh: {e}")))?;
            let stdout = String::from_utf8_lossy(&out.stdout);
            let line = stdout.lines().last().unwrap_or("").trim().to_string();
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let why = stderr.lines().last().unwrap_or("").trim();
                return Err(Error::Msg(format!("{dest}: {}", if why.is_empty() { &line } else { why })));
            }
            Ok(format!("{line} on {dest}"))
        }
        _ => Err(Error::Msg(format!("{target} names no session"))),
    }
}

fn kill_notice(target: &str, outcome: KillOutcome) -> String {
    match outcome {
        KillOutcome::Killed => format!("killed previous session {target}"),
        KillOutcome::CleanedStale => format!("cleaned up stale previous session {target}"),
        KillOutcome::Kept { clients } => format!(
            "kept previous session {target}: {clients} other viewport(s) attached (force-kill to override)"
        ),
    }
}

/// `ssh -o BatchMode=yes … <dest> posh [-g G] kill [--unless-attached] <session>`:
/// the remote kill, non-interactive (an auth prompt cannot be answered from
/// inside a session), with the same tailnet substitution as the bootstrap.
pub fn remote_kill_argv(
    dest: &crate::remote::sshwrap::SshDest,
    group: Option<&str>,
    session: &str,
    force: bool,
) -> Vec<String> {
    let mut argv = vec![
        "ssh".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=5".to_string(),
    ];
    argv.extend(dest.ssh_args());
    argv.push(dest.target());
    argv.push("posh".to_string());
    if let Some(g) = group.filter(|g| *g != "default") {
        argv.push("-g".to_string());
        argv.push(g.to_string());
    }
    argv.push("kill".to_string());
    if !force {
        argv.push("--unless-attached".to_string());
    }
    argv.push(session.to_string());
    argv
}

#[cfg(test)]
pub(crate) fn switch_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: Mutex<()> = Mutex::new(());
    GUARD.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PhRoute::*;

    #[test]
    fn default_title_is_short_host_colon_session() {
        let _g = switch_test_guard();
        set_current(&target_for(Some("me@box.example.com"), None, "dev"));
        assert_eq!(default_title().as_deref(), Some("box:dev"));
        set_current(&target_for(Some("box"), Some("grp"), "dev"));
        assert_eq!(default_title().as_deref(), Some("box:grp/dev"));
        set_current(&target_for(Some("[fe80::1]"), None, "dev"));
        assert_eq!(default_title().as_deref(), Some("[fe80::1]:dev"));
        set_current(&target_for(Some("10.0.0.7"), None, "dev"));
        assert_eq!(default_title().as_deref(), Some("10.0.0.7:dev"));
        // A local attach names this machine.
        set_current(&target_for(None, None, "dev"));
        let local = default_title().unwrap();
        assert!(local.ends_with(":dev") && local.len() > 4, "{local}");
    }

    /// Picker rows carry targets that `ph_parse` routes exactly as the typed
    /// form would — local `:name` / `:group/name`, remote `dest:name`, the
    /// create row `:+` / `dest:+` — and cells that label a session by
    /// activity beside its id.
    #[test]
    fn rows_round_trip_through_ph_parse() {
        let entries = vec![
            session::PickerEntry { name: "dev".into(), label: "~/x · nvim".into(), status: "detached".into() },
            session::PickerEntry { name: "s-2".into(), label: String::new(), status: "attached (1)".into() },
        ];
        let local = rows_for(None, "default", &entries);
        assert_eq!(local[0].cells, vec!["~/x · nvim", "dev", "local", "detached"]);
        assert_eq!(local[1].cells[..2], ["", "s-2"], "no label ⇒ blank, the id still names it");
        assert_eq!(
            crate::ph_parse(Some(&local[0].target)),
            LocalResolve { group: None, session: "dev".into() }
        );
        assert_eq!(crate::ph_parse(Some(&local[2].target)), LocalNew { group: None });
        assert_eq!(local[2].cells[0], "+ create new session…");

        let grouped = rows_for(None, "work", &entries[..1]);
        assert_eq!(
            crate::ph_parse(Some(&grouped[0].target)),
            LocalResolve { group: Some("work".into()), session: "dev".into() }
        );

        let remote = rows_for(Some("me@box"), "default", &entries[..1]);
        assert_eq!(remote[0].cells[2], "me@box");
        assert_eq!(
            crate::ph_parse(Some(&remote[0].target)),
            RemoteResolve { user: Some("me".into()), host: "box".into(), group: None, session: "dev".into() }
        );
        assert_eq!(
            crate::ph_parse(Some(&remote[1].target)),
            RemoteNew { user: Some("me".into()), host: "box".into(), group: None }
        );
        assert_eq!(candidates(&remote), "me@box:dev", "create rows are not candidates");

        // The RFC 0005 §3.5 rows: cells verbatim, a session.switch action per row.
        let json = rows_json(&remote);
        assert_eq!(json[0]["cells"][0], "~/x · nvim");
        assert_eq!(json[0]["action"]["method"], "session.switch");
        assert_eq!(json[0]["action"]["params"]["target"], "me@box:dev");
        assert_eq!(json[1]["action"]["params"]["target"], "me@box:+");
        // The attach entry points spell the current target the same way.
        assert_eq!(target_for(None, Some("default"), "dev"), ":dev");
        assert_eq!(target_for(None, None, "dev"), ":dev");
        assert_eq!(target_for(Some("me@box"), Some("work"), "s-1"), "me@box:work/s-1");
    }

    /// The hand-off is a one-shot: the front door takes the recorded switch
    /// once, and nothing is pending afterwards.
    #[test]
    fn switch_handoff_is_one_shot() {
        let _g = switch_test_guard();
        assert_eq!(take_switch(), None);
        request_switch("box:dev", Previous::Kill);
        assert_eq!(
            take_switch(),
            Some(Switch { target: "box:dev".into(), previous: Previous::Kill })
        );
        assert_eq!(take_switch(), None);
    }

    /// The leave step: three `session.switch` re-issues carrying the chosen
    /// target and a `previous`, plus a no-op cancel; `previous` parses back,
    /// and an unknown spelling is rejected (absent = keep).
    #[test]
    fn leave_commands_carry_target_and_previous() {
        let cmds = leave_commands("box:dev", ":s-1");
        let arr = cmds.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        for (i, want) in ["keep", "kill", "force-kill"].iter().enumerate() {
            assert_eq!(arr[i]["action"]["method"], "session.switch");
            assert_eq!(arr[i]["action"]["params"]["target"], "box:dev");
            assert_eq!(arr[i]["action"]["params"]["previous"], *want);
            assert_eq!(Previous::parse(Some(want)).map(Previous::as_str), Some(*want));
            assert!(arr[i]["name"].as_str().unwrap().contains(":s-1"));
        }
        assert!(arr[3]["action"].is_null(), "Cancel is a no-op entry");
        assert_eq!(Previous::parse(None), Some(Previous::Keep));
        assert_eq!(Previous::parse(Some("nuke")), None);
    }

    /// The remote kill runs non-interactively through the resolved
    /// destination (tailnet alias included), scoped to the group, and asks
    /// the remote to refuse an attached session unless forced.
    #[test]
    fn remote_kill_argv_shape() {
        let dest = crate::remote::sshwrap::SshDest::verbatim("me@box");
        assert_eq!(
            remote_kill_argv(&dest, None, "dev", false),
            ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "me@box", "posh", "kill", "--unless-attached", "dev"]
        );
        assert_eq!(
            remote_kill_argv(&dest, Some("work"), "s-1", true),
            ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "me@box", "posh", "-g", "work", "kill", "s-1"]
        );
        // A create target names no session to kill.
        assert!(kill_target(":+", false).is_err());
        assert!(kill_target("box:+", true).is_err());
    }
}
