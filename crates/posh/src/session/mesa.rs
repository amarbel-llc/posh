//! Emits the RFC 0003 "List-Table NDJSON" protocol
//! (purse-first `docs/rfcs/0003-list-table-ndjson-protocol.md`) for `posh
//! list` and pipes it to the `mesa` renderer binary. mesa auto-detects
//! whether ITS (inherited) stdout is a terminal, so posh needs no tty branch
//! for this path: styled/bordered on a terminal, plain TAB-separated on a
//! pipe — both driven by the same NDJSON stream built here.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use super::SessionEntry;
use crate::util::{Error, Result};

/// Abbreviates a leading $HOME to `~` (display only).
fn abbrev_home(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|h| !h.is_empty() && *h != "/") else {
        return path.to_string();
    };
    let Some(rest) = path.strip_prefix(home) else {
        return path.to_string();
    };
    if rest.is_empty() {
        return "~".to_string();
    }
    if rest.starts_with('/') {
        return format!("~{rest}");
    }
    path.to_string()
}

/// The session's resolved display state, keyed to the STATUS dot severity.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// At least one client attached.
    Attached,
    /// Daemon alive, no clients.
    Detached,
    /// The probe failed (stale socket, being cleaned up).
    Stale,
}

impl State {
    fn of(s: &SessionEntry) -> State {
        if s.error.is_some() {
            State::Stale
        } else if s.clients.unwrap_or(0) > 0 {
            State::Attached
        } else {
            State::Detached
        }
    }

    /// RFC 0003 §5 severity name mesa colors the STATUS dot by.
    fn sev(self) -> &'static str {
        match self {
            State::Attached => "ok",
            State::Detached => "accent",
            State::Stale => "error",
        }
    }

    fn label(self) -> &'static str {
        match self {
            State::Attached => "attached",
            State::Detached => "detached",
            State::Stale => "stale",
        }
    }
}

/// The header record: columns, the STATUS-dot legend, and the empty-table
/// message (RFC 0003 §2/§6/§7.4). ACTIVITY/ECHO/STARTED IN are `flex` and
/// shrink in that order (lowest `shrink` first) down to a floor of 8 columns
/// before mesa ellipsizes; NAME/STATUS/PID/CLIENTS are `pin`, sized to
/// content.
fn header(socket_dir: &Path) -> Value {
    json!({
        "columns": [
            {"name": "NAME", "role": "pin"},
            {"name": "STATUS", "role": "pin"},
            {"name": "PID", "role": "pin"},
            {"name": "CLIENTS", "role": "pin"},
            {"name": "STARTED IN", "role": "flex", "shrink": 2, "min": 8},
            {"name": "ACTIVITY", "role": "flex", "shrink": 0, "min": 8},
            {"name": "ECHO", "role": "flex", "shrink": 1, "min": 8},
        ],
        "legend": [
            {"sev": State::Attached.sev(), "glyph": "\u{25cf}", "label": State::Attached.label()},
            {"sev": State::Detached.sev(), "glyph": "\u{25cf}", "label": State::Detached.label()},
            {"sev": State::Stale.sev(), "glyph": "\u{25cf}", "label": State::Stale.label()},
        ],
        "empty": empty_message(socket_dir),
    })
}

/// The STATUS cell: a state-colored dot plus an optional dim marker
/// (`(current)` for the session this client is running inside). State is
/// carried by the dot's severity alone — the legend footer is the key.
fn status_cell(state: State, current: bool) -> Value {
    if current {
        json!({"spans": [
            {"text": "\u{25cf}", "sev": state.sev()},
            {"text": " (current)", "sev": "muted"},
        ]})
    } else {
        json!({"spans": [{"text": "\u{25cf}", "sev": state.sev()}]})
    }
}

/// One ROW record. A stale session (probe failed) puts its error message in
/// the ACTIVITY cell (dimmed) and leaves the other data cells blank; the
/// STATUS dot still carries the `error` severity.
fn row(s: &SessionEntry, current: Option<&str>, home: Option<&str>) -> Value {
    let state = State::of(s);
    if let Some(err) = &s.error {
        return json!({"cells": [
            s.name,
            status_cell(state, false),
            "",
            "",
            "",
            {"spans": [{"text": format!("{err} (cleaning up)"), "sev": "muted"}]},
            "",
        ]});
    }
    json!({"cells": [
        s.name,
        status_cell(state, current == Some(s.name.as_str())),
        s.pid.map(|p| p.to_string()).unwrap_or_default(),
        s.clients.map(|c| c.to_string()).unwrap_or_default(),
        abbrev_home(s.cwd.as_deref().unwrap_or(""), home),
        s.activity.clone().or_else(|| s.cmd.clone()).unwrap_or_default(),
        s.echo.clone().unwrap_or_default(),
    ]})
}

/// Builds the full NDJSON stream (one header record, then one row record per
/// session) that `render` pipes to `mesa`. Pure and separately testable from
/// the child-process plumbing.
fn build_ndjson(sessions: &[SessionEntry], current: Option<&str>, home: Option<&str>, socket_dir: &Path) -> String {
    let mut out = header(socket_dir).to_string();
    out.push('\n');
    for s in sessions {
        out.push_str(&row(s, current, home).to_string());
        out.push('\n');
    }
    out
}

/// Renders `sessions` by spawning `mesa`, writing the RFC 0003 NDJSON stream
/// to its stdin, and letting its stdout inherit posh's own — mesa decides
/// styled vs. plain by probing that inherited stdout itself. A `mesa` binary
/// missing from PATH is reported as a clear error rather than silently
/// producing no output (the nix package wraps it onto PATH; see flake.nix).
pub(super) fn render(
    sessions: &[SessionEntry],
    current: Option<&str>,
    home: Option<&str>,
    socket_dir: &Path,
) -> Result<()> {
    let ndjson = build_ndjson(sessions, current, home, socket_dir);
    let mut child = Command::new("mesa")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::Msg(
                    "mesa binary not found on PATH: posh list renders through mesa \
                     (purse-first); the nix package wraps it onto PATH, so a manual \
                     build needs it available too"
                        .to_string(),
                )
            } else {
                Error::Io(e)
            }
        })?;
    child
        .stdin
        .take()
        .expect("mesa spawned with piped stdin")
        .write_all(ndjson.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        return Err(Error::Msg(format!("mesa exited with {status}")));
    }
    Ok(())
}

/// The empty-group message, carried as the header's `empty` field (RFC 0003
/// §7.4) so mesa renders it dim on a TTY and verbatim on a pipe.
pub(super) fn empty_message(socket_dir: &Path) -> String {
    format!("no sessions found in {}", socket_dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, clients: u64) -> SessionEntry {
        SessionEntry {
            name: name.to_string(),
            pid: Some(4242),
            clients: Some(clients),
            error: None,
            cmd: Some("fish".to_string()),
            cwd: Some("/home/u/eng".to_string()),
            activity: Some("nvim".to_string()),
            echo: Some("optimistic auto-escalated 412ms".to_string()),
        }
    }

    fn stale(name: &str) -> SessionEntry {
        SessionEntry {
            name: name.to_string(),
            pid: None,
            clients: None,
            error: Some("connection refused".to_string()),
            cmd: None,
            cwd: None,
            activity: None,
            echo: None,
        }
    }

    fn parse_lines(ndjson: &str) -> Vec<Value> {
        ndjson.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn header_has_seven_columns_legend_and_empty() {
        let records = parse_lines(&build_ndjson(&[], None, None, Path::new("/run/posh/default")));
        assert_eq!(records.len(), 1);
        let header = &records[0];
        assert_eq!(header["columns"].as_array().unwrap().len(), 7);
        assert_eq!(header["legend"].as_array().unwrap().len(), 3);
        assert_eq!(header["empty"], "no sessions found in /run/posh/default");
    }

    #[test]
    fn flex_columns_shrink_activity_first_then_echo_then_started_in() {
        let records = parse_lines(&build_ndjson(&[], None, None, Path::new("/x")));
        let cols = records[0]["columns"].as_array().unwrap();
        let shrink_of = |name: &str| {
            cols.iter().find(|c| c["name"] == name).unwrap()["shrink"]
                .as_u64()
                .unwrap()
        };
        assert!(shrink_of("ACTIVITY") < shrink_of("ECHO"));
        assert!(shrink_of("ECHO") < shrink_of("STARTED IN"));
    }

    #[test]
    fn row_count_matches_sessions() {
        let sessions = [entry("dev", 1), entry("other", 0)];
        let records = parse_lines(&build_ndjson(&sessions, None, None, Path::new("/x")));
        assert_eq!(records.len(), 3); // header + 2 rows
    }

    #[test]
    fn status_severities_by_state() {
        let sessions = [entry("a", 2), entry("d", 0), stale("s")];
        let records = parse_lines(&build_ndjson(&sessions, None, None, Path::new("/x")));
        let sev = |i: usize| records[i]["cells"][1]["spans"][0]["sev"].clone();
        assert_eq!(sev(1), "ok"); // attached
        assert_eq!(sev(2), "accent"); // detached
        assert_eq!(sev(3), "error"); // stale
    }

    #[test]
    fn current_session_is_marked() {
        let sessions = [entry("dev", 1), entry("other", 0)];
        let records = parse_lines(&build_ndjson(&sessions, Some("dev"), None, Path::new("/x")));
        let spans = records[1]["cells"][1]["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1]["text"], " (current)");
        // The other row is not marked.
        assert_eq!(records[2]["cells"][1]["spans"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn stale_row_carries_error_in_activity_cell_other_cells_blank() {
        let records = parse_lines(&build_ndjson(&[stale("old")], None, None, Path::new("/x")));
        let cells = records[1]["cells"].as_array().unwrap();
        assert_eq!(cells[0], "old"); // NAME
        assert_eq!(cells[2], ""); // PID
        assert_eq!(cells[3], ""); // CLIENTS
        assert_eq!(cells[4], ""); // STARTED IN
        assert_eq!(cells[5]["spans"][0]["text"], "connection refused (cleaning up)");
        assert_eq!(cells[6], ""); // ECHO
    }

    #[test]
    fn home_is_abbreviated() {
        assert_eq!(abbrev_home("/home/u/eng", Some("/home/u")), "~/eng");
        assert_eq!(abbrev_home("/home/u", Some("/home/u")), "~");
        // A sibling like /home/u2 must not match.
        assert_eq!(abbrev_home("/home/u2/x", Some("/home/u")), "/home/u2/x");
        assert_eq!(abbrev_home("/etc", None), "/etc");
    }

    #[test]
    fn empty_message_matches_wording() {
        assert_eq!(
            empty_message(Path::new("/run/posh/default")),
            "no sessions found in /run/posh/default"
        );
    }
}
