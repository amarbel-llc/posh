//! FDR 0016 session picker: the row source shared by the top-level `ph`
//! chooser and the in-session palette's *Switch session…* command, and the
//! switch hand-off between a client that ended with a selection and the front
//! door that re-attaches to it.
//!
//! The renderer draws the rows without knowing what a session is (RFC 0005
//! §3.5); this module is where a row's cells and its `session.switch` target
//! are decided. A target is an RFC 0001 spelling the front door already
//! routes (`crate::ph_parse`), so a selection attaches exactly as typing it
//! would.

use std::sync::Mutex;

use serde_json::Value;

use crate::session::{self, Config};
use crate::util::Result;

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

/// The rows for one host's entries: `dest` is the `[user@]host` the host
/// cell shows and the target carries; `None` is this machine. A trailing
/// `+ create new session…` row routes to `posh start` there (`:+`).
pub fn rows_for(dest: Option<&str>, group: &str, entries: &[session::PickerEntry]) -> Vec<PickerRow> {
    let host_cell = dest.unwrap_or("local").to_string();
    let target = |session: &str| {
        let scoped = if group == "default" {
            session.to_string()
        } else {
            format!("{group}/{session}")
        };
        format!("{}:{scoped}", dest.unwrap_or(""))
    };
    let mut rows: Vec<PickerRow> = entries
        .iter()
        .map(|e| PickerRow {
            target: target(&e.name),
            cells: vec![e.label.clone(), e.name.clone(), host_cell.clone(), e.status.clone()],
        })
        .collect();
    rows.push(PickerRow {
        target: target("+"),
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
        let (user, host) = match dest.rsplit_once('@') {
            Some((u, h)) if !u.is_empty() => (Some(u), h),
            _ => (None, dest.as_str()),
        };
        match crate::remote_list_output(user, host, group, "--json")
            .and_then(|json| session::picker_entries_from_json(&json))
        {
            Ok(entries) => rows.extend(rows_for(Some(&dest), group, &entries)),
            Err(e) => eprintln!("posh: {dest}: {e}"),
        }
    }
    Ok(rows)
}

/// The RFC 0005 §3.5 `rows` array: each row's cells plus a
/// `session.switch {target}` action (§7).
pub fn rows_json(rows: &[PickerRow]) -> Value {
    Value::Array(
        rows.iter()
            .map(|r| {
                serde_json::json!({
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

/// The switch hand-off: a client that dispatched `session.switch` records
/// the target and ends its attach (quit / detach); the front door reads it
/// back once the attach returns and re-attaches to it (FDR 0016 re-dial).
/// Process-global because the attach entry points (mux channel, bootstrap,
/// local socket) each return their own shape — this is the one seam they
/// all pass through.
static SWITCH: Mutex<Option<String>> = Mutex::new(None);

pub fn request_switch(target: &str) {
    *SWITCH.lock().unwrap_or_else(|e| e.into_inner()) = Some(target.to_string());
}

pub fn take_switch() -> Option<String> {
    SWITCH.lock().unwrap_or_else(|e| e.into_inner()).take()
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
    }

    /// The hand-off is a one-shot: the front door takes the recorded target
    /// once, and nothing is pending afterwards.
    #[test]
    fn switch_handoff_is_one_shot() {
        let _g = switch_test_guard();
        assert_eq!(take_switch(), None);
        request_switch("box:dev");
        assert_eq!(take_switch().as_deref(), Some("box:dev"));
        assert_eq!(take_switch(), None);
    }
}
