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
/// Keeping it is a PUSH: the session goes on the stack and *Back* returns
/// to it (FDR 0016, stacked switching).
pub fn leave_commands(target: &str, leaving: &str) -> Value {
    leave_commands_for("session.switch", "Switch", Some(target), leaving)
}

/// The leave question for *Back* (`session.pop`): the same three fates for
/// the session being left, re-issued as `session.pop` with a `previous`
/// (the target is the stack's top, never named by the renderer).
pub fn back_commands(leaving: &str) -> Value {
    leave_commands_for("session.pop", "Back", None, leaving)
}

fn leave_commands_for(method: &str, verb: &str, target: Option<&str>, leaving: &str) -> Value {
    let cmd = |name: String, previous: Previous| {
        let mut params = json!({ "previous": previous.as_str() });
        if let Some(t) = target {
            params["target"] = json!(t);
        }
        json!({
            "name": name,
            "action": { "method": method, "params": params },
        })
    };
    json!([
        cmd(format!("{verb}, keep {leaving} running"), Previous::Keep),
        cmd(format!("{verb}, kill {leaving} (kept if other viewports are attached)"), Previous::Kill),
        cmd(format!("{verb}, kill {leaving} even with other viewports attached"), Previous::ForceKill),
        { "name": "Cancel" },
    ])
}

/// A recorded switch: the target to re-attach to, what to do with the
/// session being left, and whether this is a POP (the target came off the
/// stack; the front door pops it) or a push (the front door pushes the
/// session being left).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switch {
    pub target: String,
    pub previous: Previous,
    pub pop: bool,
}

/// The session stack (FDR 0016, stacked switching): the targets a viewport
/// switched AWAY from in this front-door process, most recent last. A switch
/// pushes the session it leaves; *Back* pops. Lives as long as the process
/// — the `run()` re-attach loop — and no longer: leaving posh empties it.
static STACK: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The session *Back* would return to, if any.
pub fn stack_top() -> Option<String> {
    STACK.lock().unwrap_or_else(|e| e.into_inner()).last().cloned()
}

pub fn stack_depth() -> usize {
    STACK.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// The picker's heading: [`TITLE`], plus how deep the session stack is when
/// a *Back* would return somewhere (`sessions · 2 to go back`).
pub fn title() -> String {
    match stack_depth() {
        0 => TITLE.to_string(),
        1 => format!("{TITLE} \u{b7} 1 to go back"),
        n => format!("{TITLE} \u{b7} {n} to go back"),
    }
}

/// Push the session a switch is leaving (the front door, before the re-dial).
pub fn stack_push(target: &str) {
    STACK.lock().unwrap_or_else(|e| e.into_inner()).push(target.to_string());
}

/// Pop the stack (the front door, when dispatching a *Back*).
pub fn stack_pop() -> Option<String> {
    STACK.lock().unwrap_or_else(|e| e.into_inner()).pop()
}

/// Record a *Back*: the switch target is the stack's top. `None` (nothing
/// recorded) when the stack is empty.
pub fn request_pop(previous: Previous) -> Option<String> {
    let target = stack_top()?;
    *SWITCH.lock().unwrap_or_else(|e| e.into_inner()) = Some(Switch {
        target: target.clone(),
        previous,
        pop: true,
    });
    Some(target)
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
        pop: false,
    });
}

pub fn take_switch() -> Option<Switch> {
    SWITCH.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Why an attach ended — noted by the client loop as it returns, read by
/// the front door to decide whether the stack pops on its own (FDR 0016
/// stacked switching: a top session that goes away returns the viewport to
/// the session under it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEnd {
    /// The session itself ended: its shell exited with this status.
    Ended(i32),
    /// The attach lost the session without being asked to leave (the reason).
    Lost(String),
    /// The user quit or detached (a signal counts), or a switch ended it.
    Quit,
}

static ATTACH_END: Mutex<Option<AttachEnd>> = Mutex::new(None);

/// Notice text for the NEXT attach's first frame (the local client prints
/// it once its tty is restored): what happened to the session that was
/// left behind by an automatic pop.
static PENDING_NOTICE: Mutex<Option<String>> = Mutex::new(None);

pub fn note_attach_end(end: AttachEnd) {
    *ATTACH_END.lock().unwrap_or_else(|e| e.into_inner()) = Some(end);
}

pub fn take_attach_end() -> Option<AttachEnd> {
    ATTACH_END.lock().unwrap_or_else(|e| e.into_inner()).take()
}

pub fn take_pending_notice() -> Option<String> {
    PENDING_NOTICE.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// The automatic pop: an attach that ended WITHOUT a switch, because the
/// session ended or the connection was lost, returns the viewport to the
/// stack's top (a *Back, keep* the user did not have to ask for) and leaves a
/// notice for the new attach's banner. An explicit quit or detach — or no
/// stack — pops nothing: the viewport exits and the stack goes with it.
/// `None` when nothing is to be popped.
pub fn auto_pop(end: Option<&AttachEnd>) -> Option<Switch> {
    let why = match end? {
        AttachEnd::Ended(code) => format!("ended (exit {code})"),
        AttachEnd::Lost(reason) => format!("lost ({reason})"),
        AttachEnd::Quit => return None,
    };
    let top = stack_top()?;
    let left = current().unwrap_or_else(|| "session".to_string());
    *PENDING_NOTICE.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!(
        "session {} {why} \u{2014} back to {}",
        display_target(&left),
        display_target(&top)
    ));
    Some(Switch {
        target: top,
        previous: Previous::Keep,
        pop: true,
    })
}

/// A target for a notice: a local `:session` names this machine, like the
/// picker's rows and the default title do.
fn display_target(target: &str) -> String {
    match target.strip_prefix(':') {
        Some(rest) => format!("{}:{rest}", crate::remote::mux::hostname()),
        None => target.to_string(),
    }
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
/// With the session's RFC 0013 §5 foreground process, when the daemon has
/// reported one on the frame (`CAP_SESSION_ACTIVITY`, #193), the title is
/// `host:session · process` — so an auto-named session reads as what it is
/// running, `flac:ff9fe216 · clown`, not just an id.
pub fn default_title_with(process: Option<&str>) -> Option<String> {
    let cur = current()?;
    let (dest, session) = cur.rsplit_once(':')?;
    let host = if dest.is_empty() {
        crate::remote::mux::hostname()
    } else {
        short_host(dest.rsplit_once('@').map_or(dest, |(_, h)| h))
    };
    let mut title = format!("{host}:{}", short_session(session));
    if let Some(p) = process.map(str::trim).filter(|p| !p.is_empty()) {
        title.push_str(" \u{b7} ");
        title.push_str(p);
    }
    Some(title)
}

/// A `[group/]name` for the title: an auto-generated UUID name (what clown
/// and other spawners hand `posh start`, and the FDR 0011 `:+` form) is cut
/// to its first 8 hex digits, git-abbreviation style — `flac:ff9fe216`
/// rather than a 36-character id — while any other name is kept whole.
/// Until the RFC 0013 §5 activity label rides frames (it reaches only the
/// unattached listing today), the name is all an attached viewport knows.
fn short_session(scoped: &str) -> String {
    let (group, name) = match scoped.rsplit_once('/') {
        Some((g, n)) => (Some(g), n),
        None => (None, scoped),
    };
    let is_uuid = name.len() == 36
        && name
            .char_indices()
            .all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() });
    let name = if is_uuid { &name[..8] } else { name };
    match group {
        Some(g) => format!("{g}/{name}"),
        None => name.to_string(),
    }
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
        assert_eq!(default_title_with(None).as_deref(), Some("box:dev"));
        set_current(&target_for(Some("box"), Some("grp"), "dev"));
        assert_eq!(default_title_with(None).as_deref(), Some("box:grp/dev"));
        set_current(&target_for(Some("[fe80::1]"), None, "dev"));
        assert_eq!(default_title_with(None).as_deref(), Some("[fe80::1]:dev"));
        set_current(&target_for(Some("10.0.0.7"), None, "dev"));
        assert_eq!(default_title_with(None).as_deref(), Some("10.0.0.7:dev"));
        // A local attach names this machine.
        set_current(&target_for(None, None, "dev"));
        let local = default_title_with(None).unwrap();
        assert!(local.ends_with(":dev") && local.len() > 4, "{local}");
        // An auto-generated UUID name is abbreviated, group kept; a look-alike
        // that is not a UUID (wrong length / a non-hex digit) stays whole.
        set_current(&target_for(Some("box"), None, "ff9fe216-9652-4e23-805c-6f4dd5ce7eca"));
        assert_eq!(default_title_with(None).as_deref(), Some("box:ff9fe216"));
        set_current(&target_for(Some("box"), Some("grp"), "ff9fe216-9652-4e23-805c-6f4dd5ce7eca"));
        assert_eq!(default_title_with(None).as_deref(), Some("box:grp/ff9fe216"));
        set_current(&target_for(Some("box"), None, "ff9fe216-9652-4e23-805c-6f4dd5ce7ecz"));
        assert_eq!(default_title_with(None).as_deref(), Some("box:ff9fe216-9652-4e23-805c-6f4dd5ce7ecz"));
        // With the daemon's foreground process known, it is appended (#193).
        set_current(&target_for(Some("box"), None, "ff9fe216-9652-4e23-805c-6f4dd5ce7eca"));
        assert_eq!(default_title_with(Some("clown")).as_deref(), Some("box:ff9fe216 \u{b7} clown"));
        assert_eq!(default_title_with(Some("  ")).as_deref(), Some("box:ff9fe216"));
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
            Some(Switch { target: "box:dev".into(), previous: Previous::Kill, pop: false })
        );
        assert_eq!(take_switch(), None);
    }

    /// Stacked switching: a push records the session being left, *Back*
    /// targets the top (recorded as a pop), and an empty stack records
    /// nothing. The stack itself is the front door's to push and pop.
    #[test]
    fn back_pops_the_stack_and_an_empty_stack_records_nothing() {
        let _g = switch_test_guard();
        while stack_pop().is_some() {}
        assert_eq!(title(), TITLE);
        assert_eq!(request_pop(Previous::Keep), None, "nothing to go back to");
        assert_eq!(take_switch(), None);
        stack_push(":s-1");
        stack_push("box:dev");
        assert_eq!(stack_depth(), 2);
        assert_eq!(title(), "sessions \u{b7} 2 to go back");
        assert_eq!(stack_top().as_deref(), Some("box:dev"));
        assert_eq!(request_pop(Previous::Kill).as_deref(), Some("box:dev"));
        assert_eq!(
            take_switch(),
            Some(Switch { target: "box:dev".into(), previous: Previous::Kill, pop: true })
        );
        // Recording a pop does not pop: the front door does, when it re-dials.
        assert_eq!(stack_depth(), 2);
        assert_eq!(stack_pop().as_deref(), Some("box:dev"));
        assert_eq!(stack_top().as_deref(), Some(":s-1"));
        stack_pop();
        // The back question re-issues session.pop with a previous, no target.
        let cmds = back_commands(":s-1");
        let arr = cmds.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0]["action"]["method"], "session.pop");
        assert!(arr[0]["action"]["params"].get("target").is_none());
        assert_eq!(arr[1]["action"]["params"]["previous"], "kill");
        assert!(arr[0]["name"].as_str().unwrap().starts_with("Back, keep :s-1"));
    }

    /// The automatic pop: a top session that ENDED or was LOST returns the
    /// viewport to the stack's top as a *Back, keep*, with a notice for the
    /// new attach's banner; a quit pops nothing, and neither does an empty
    /// stack. The noted end is one-shot, like the switch.
    #[test]
    fn an_ended_or_lost_top_session_pops_back_with_a_notice() {
        let _g = switch_test_guard();
        while stack_pop().is_some() {}
        take_pending_notice();
        set_current("box:dev");
        note_attach_end(AttachEnd::Ended(0));
        assert_eq!(take_attach_end(), Some(AttachEnd::Ended(0)));
        assert_eq!(take_attach_end(), None, "one-shot");
        // No stack: nothing to pop, whatever the end.
        assert_eq!(auto_pop(Some(&AttachEnd::Ended(0))), None);
        assert_eq!(take_pending_notice(), None);
        stack_push(":s-2");
        // A quit never pops.
        assert_eq!(auto_pop(Some(&AttachEnd::Quit)), None);
        assert_eq!(auto_pop(None), None);
        assert_eq!(take_pending_notice(), None);
        // An ended session pops back (the front door pops the entry on re-dial).
        assert_eq!(
            auto_pop(Some(&AttachEnd::Ended(1))),
            Some(Switch { target: ":s-2".into(), previous: Previous::Keep, pop: true })
        );
        let notice = take_pending_notice().unwrap();
        assert!(notice.starts_with("session box:dev ended (exit 1) \u{2014} back to "), "{notice}");
        assert!(notice.ends_with(":s-2"), "a local target names this machine: {notice}");
        assert_eq!(take_pending_notice(), None, "one-shot");
        assert_eq!(
            auto_pop(Some(&AttachEnd::Lost("mux channel closed".into()))),
            Some(Switch { target: ":s-2".into(), previous: Previous::Keep, pop: true })
        );
        assert!(take_pending_notice().unwrap().contains("lost (mux channel closed)"));
        stack_pop();
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
