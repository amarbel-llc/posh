//! Emits the RFC 0003 "List-Table NDJSON" protocol
//! (purse-first `docs/rfcs/0003-list-table-ndjson-protocol.md`) for `posh
//! list` and pipes it to the `mesa` renderer binary. mesa decides by probing
//! its inherited stdout whether to draw a styled table or print plain
//! TAB-separated lines, but from ONE column set — so posh checks the tty too
//! and sends each audience its own table (posh#215): a terminal gets four
//! merged, decorated columns (porcelain); a pipe keeps one field per column
//! (plumbing). purse-first#192 is about giving mesa's API that split.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use posh_proto::caps::SessionKind;
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

/// The STATUS-dot legend (RFC 0003 §6), shared by both tables.
fn legend() -> Value {
    json!([
        {"sev": State::Attached.sev(), "glyph": "\u{25cf}", "label": State::Attached.label()},
        {"sev": State::Detached.sev(), "glyph": "\u{25cf}", "label": State::Detached.label()},
        {"sev": State::Stale.sev(), "glyph": "\u{25cf}", "label": State::Stale.label()},
    ])
}

/// The terminal table's header (posh#215): NAME, STATUS, CWD, ACTIVITY.
/// ACTIVITY shrinks first, CWD after; NAME/STATUS are `pin`.
fn styled_header(socket_dir: &Path) -> Value {
    json!({
        "columns": [
            {"name": "NAME", "role": "pin"},
            {"name": "STATUS", "role": "pin"},
            {"name": "CWD", "role": "flex", "shrink": 1, "min": 8},
            {"name": "ACTIVITY", "role": "flex", "shrink": 0, "min": 8},
        ],
        "legend": legend(),
        "empty": empty_message(socket_dir),
    })
}

/// One terminal-table row. NAME carries a dim ` (anon)` for an anonymous
/// session; STATUS the dot, the client count while attached, and `(current)`;
/// CWD where the session is, plus a dim `← <start>` only when it moved;
/// ACTIVITY the label plus a dim `· echo <summary>` when a client reports one.
/// A stale session's error takes the ACTIVITY cell, as in the plain table.
fn styled_row(s: &SessionEntry, current: Option<&str>, home: Option<&str>) -> Value {
    let muted = |t: String| json!({"text": t, "sev": "muted"});
    let name = if s.kind == Some(SessionKind::Anonymous) {
        json!({"spans": [{"text": s.name}, muted(" (anon)".into())]})
    } else {
        json!(s.name)
    };
    let state = State::of(s);
    if let Some(err) = &s.error {
        return json!({"cells": [name, status_cell(state, None, false), "", {"spans": [muted(format!("{err} (cleaning up)"))]}]});
    }
    let attached = s.clients.filter(|_| state == State::Attached);
    let stale_build = s.build.as_deref().filter(|_| s.stale);
    let status = status_cell_with_build(state, attached, current == Some(s.name.as_str()), stale_build);
    let start = s.cwd.as_deref().map(|d| abbrev_home(d, home));
    let now = s.cwd_now.as_deref().map(|d| abbrev_home(d, home));
    let cwd = match (now, start) {
        (Some(now), Some(start)) if now != start => {
            json!({"spans": [{"text": now}, muted(format!("  \u{2190} {start}"))]})
        }
        (Some(dir), _) | (None, Some(dir)) => json!(dir),
        (None, None) => json!(""),
    };
    let label = s.activity.clone().or_else(|| s.cmd.clone()).unwrap_or_default();
    // No client attached: nothing to add.
    let activity = match s.echo.as_deref().filter(|e| *e != super::ECHO_NO_CLIENT) {
        Some(echo) if label.is_empty() => json!({"spans": [muted(format!("echo {echo}"))]}),
        Some(echo) => json!({"spans": [{"text": label}, muted(format!(" \u{b7} echo {echo}"))]}),
        None => json!(label),
    };
    json!({"cells": [name, status, cwd, activity]})
}

/// The sessions a table shows: on a terminal, system sessions only when asked
/// (`--include-system-sessions`); on a pipe (plumbing), every one.
fn shown(sessions: &[SessionEntry], styled: bool, include_system: bool) -> Vec<&SessionEntry> {
    sessions
        .iter()
        .filter(|s| !styled || include_system || s.kind != Some(SessionKind::System))
        .collect()
}

/// The plain (piped) table's header record: one field per column, stable
/// for scripts (RFC 0003 §2/§6/§7.4). ACTIVITY/ECHO/STARTED IN/CWD are `flex`
/// and shrink in that order (lowest `shrink` first) down to a floor of 8
/// columns before mesa ellipsizes; NAME/STATUS/PID/CLIENTS/KIND are `pin`.
fn header(socket_dir: &Path) -> Value {
    json!({
        "columns": [
            {"name": "NAME", "role": "pin"},
            {"name": "STATUS", "role": "pin"},
            {"name": "PID", "role": "pin"},
            {"name": "CLIENTS", "role": "pin"},
            {"name": "KIND", "role": "pin"},
            {"name": "CWD", "role": "flex", "shrink": 3, "min": 8},
            {"name": "STARTED IN", "role": "flex", "shrink": 2, "min": 8},
            {"name": "ACTIVITY", "role": "flex", "shrink": 0, "min": 8},
            {"name": "ECHO", "role": "flex", "shrink": 1, "min": 8},
            // posh#206: appended so the first nine keep their positions.
            {"name": "BUILD", "role": "pin"},
        ],
        "legend": legend(),
        "empty": empty_message(socket_dir),
    })
}

/// The STATUS cell: a state-colored dot, the attached client count when
/// given, and a dim `(current)` for the session this client runs inside.
/// State is carried by the dot's severity alone — the legend is the key.
fn status_cell(state: State, clients: Option<u64>, current: bool) -> Value {
    status_cell_with_build(state, clients, current, None)
}

/// [`status_cell`] plus, for a daemon on another build than this posh
/// (posh#206), a dim `old build <sha>`: the terminal table shows the anomaly
/// only, never the build of a current daemon.
fn status_cell_with_build(state: State, clients: Option<u64>, current: bool, stale_build: Option<&str>) -> Value {
    let mut spans = vec![json!({"text": "\u{25cf}", "sev": state.sev()})];
    if let Some(n) = clients {
        spans.push(json!({"text": format!(" {n}")}));
    }
    if let Some(build) = stale_build {
        // `<version>+<sha>`: the sha is what tells two builds apart.
        let sha = build.rsplit_once('+').map_or(build, |(_, sha)| sha);
        // Not "stale": the legend already uses that for a dead socket.
        spans.push(json!({"text": format!(" old build {sha}"), "sev": "muted"}));
    }
    if current {
        spans.push(json!({"text": " (current)", "sev": "muted"}));
    }
    json!({ "spans": spans })
}

/// One ROW record. A stale session (probe failed) puts its error message in
/// the ACTIVITY cell (dimmed) and leaves the other data cells blank; the
/// STATUS dot still carries the `error` severity.
fn row(s: &SessionEntry, current: Option<&str>, home: Option<&str>) -> Value {
    let state = State::of(s);
    if let Some(err) = &s.error {
        return json!({"cells": [
            s.name,
            status_cell(state, None, false),
            "",
            "",
            "",
            "",
            "",
            {"spans": [{"text": format!("{err} (cleaning up)"), "sev": "muted"}]},
            "",
            "",
        ]});
    }
    json!({"cells": [
        s.name,
        status_cell(state, None, current == Some(s.name.as_str())),
        s.pid.map(|p| p.to_string()).unwrap_or_default(),
        s.clients.map(|c| c.to_string()).unwrap_or_default(),
        s.kind.map(|k| k.as_str().to_string()).unwrap_or_default(),
        abbrev_home(s.cwd_now.as_deref().unwrap_or(""), home),
        abbrev_home(s.cwd.as_deref().unwrap_or(""), home),
        s.activity.clone().or_else(|| s.cmd.clone()).unwrap_or_default(),
        s.echo.clone().unwrap_or_default(),
        s.build.clone().unwrap_or_default(),
    ]})
}

/// Builds the full NDJSON stream (one header record, then one row record per
/// session) that `render` pipes to `mesa`: the terminal table when `styled`,
/// else the plain one. Pure and separately testable from the child process.
fn build_ndjson<'a>(
    sessions: impl IntoIterator<Item = &'a SessionEntry>,
    current: Option<&str>,
    home: Option<&str>,
    socket_dir: &Path,
    styled: bool,
) -> String {
    let head = if styled { styled_header(socket_dir) } else { header(socket_dir) };
    let mut out = head.to_string();
    out.push('\n');
    for s in sessions {
        let r = if styled { styled_row(s, current, home) } else { row(s, current, home) };
        out.push_str(&r.to_string());
        out.push('\n');
    }
    out
}

/// Renders `sessions` by spawning `mesa`, writing the RFC 0003 NDJSON stream
/// to its stdin, and letting its stdout inherit posh's own. posh makes the
/// same tty check mesa does to pick the table shape (see the module doc). A
/// `mesa` binary missing from PATH is reported as a clear error rather than
/// silently producing no output (the nix package wraps it onto PATH).
pub(super) fn render(
    sessions: &[SessionEntry],
    current: Option<&str>,
    home: Option<&str>,
    socket_dir: &Path,
    include_system: bool,
) -> Result<()> {
    let styled = crate::util::is_tty(libc::STDOUT_FILENO);
    pipe(&build_ndjson(shown(sessions, styled, include_system), current, home, socket_dir, styled))
}

/// Pipes a complete RFC 0003 NDJSON stream to `mesa` — the one child-process
/// seam every posh table (`list`, `mux ls`) renders through.
pub(crate) fn pipe(ndjson: &str) -> Result<()> {
    let mut child = Command::new("mesa")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::Msg(
                    "mesa binary not found on PATH: posh's tables render through mesa \
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
            cwd_now: Some("/home/u/eng/posh".to_string()),
            activity: Some("nvim".to_string()),
            echo: Some("optimistic auto-escalated 412ms".to_string()),
            kind: Some(SessionKind::Named),
            ..Default::default()
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
            cwd_now: None,
            activity: None,
            echo: None,
            kind: None,
            ..Default::default()
        }
    }

    fn parse_lines(ndjson: &str) -> Vec<Value> {
        ndjson.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn the_plain_header_has_ten_columns_legend_and_empty() {
        let records = parse_lines(&build_ndjson(&[], None, None, Path::new("/run/posh/default"), false));
        assert_eq!(records.len(), 1);
        let header = &records[0];
        assert_eq!(header["columns"].as_array().unwrap().len(), 10);
        assert_eq!(header["legend"].as_array().unwrap().len(), 3);
        assert_eq!(header["empty"], "no sessions found in /run/posh/default");
    }

    #[test]
    fn flex_columns_shrink_activity_then_echo_then_started_in_then_cwd() {
        let records = parse_lines(&build_ndjson(&[], None, None, Path::new("/x"), false));
        let cols = records[0]["columns"].as_array().unwrap();
        let shrink_of = |name: &str| {
            cols.iter().find(|c| c["name"] == name).unwrap()["shrink"]
                .as_u64()
                .unwrap()
        };
        assert!(shrink_of("ACTIVITY") < shrink_of("ECHO"));
        assert!(shrink_of("ECHO") < shrink_of("STARTED IN"));
        assert!(shrink_of("STARTED IN") < shrink_of("CWD"), "where it is outlasts where it began");
    }

    /// ADR 0008: CWD (where the session is now) sits before STARTED IN,
    /// home-abbreviated like it; a pre-cascade daemon leaves it blank.
    #[test]
    fn cwd_column_shows_where_the_session_is_now() {
        let records = parse_lines(&build_ndjson(&[entry("dev", 1)], None, Some("/home/u"), Path::new("/x"), false));
        let cols = records[0]["columns"].as_array().unwrap();
        let at = |name: &str| cols.iter().position(|c| c["name"] == name).unwrap();
        assert_eq!(at("CWD") + 1, at("STARTED IN"));
        assert_eq!(records[1]["cells"][at("CWD")], "~/eng/posh");
        assert_eq!(records[1]["cells"][at("STARTED IN")], "~/eng");
        let mut old = entry("old", 0);
        old.cwd_now = None;
        let records = parse_lines(&build_ndjson(&[old], None, Some("/home/u"), Path::new("/x"), false));
        assert_eq!(records[1]["cells"][at("CWD")], "");
    }

    #[test]
    fn row_count_matches_sessions() {
        let sessions = [entry("dev", 1), entry("other", 0)];
        let records = parse_lines(&build_ndjson(&sessions, None, None, Path::new("/x"), false));
        assert_eq!(records.len(), 3); // header + 2 rows
    }

    #[test]
    fn status_severities_by_state() {
        let sessions = [entry("a", 2), entry("d", 0), stale("s")];
        let records = parse_lines(&build_ndjson(&sessions, None, None, Path::new("/x"), false));
        let sev = |i: usize| records[i]["cells"][1]["spans"][0]["sev"].clone();
        assert_eq!(sev(1), "ok"); // attached
        assert_eq!(sev(2), "accent"); // detached
        assert_eq!(sev(3), "error"); // stale
    }

    #[test]
    fn current_session_is_marked() {
        let sessions = [entry("dev", 1), entry("other", 0)];
        let records = parse_lines(&build_ndjson(&sessions, Some("dev"), None, Path::new("/x"), false));
        let spans = records[1]["cells"][1]["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1]["text"], " (current)");
        // The other row is not marked.
        assert_eq!(records[2]["cells"][1]["spans"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn stale_row_carries_error_in_activity_cell_other_cells_blank() {
        let records = parse_lines(&build_ndjson(&[stale("old")], None, None, Path::new("/x"), false));
        let cells = records[1]["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 10);
        assert_eq!(cells[9], ""); // BUILD
        assert_eq!(cells[0], "old"); // NAME
        assert_eq!(cells[2], ""); // PID
        assert_eq!(cells[3], ""); // CLIENTS
        assert_eq!(cells[4], ""); // KIND
        assert_eq!(cells[5], ""); // CWD
        assert_eq!(cells[6], ""); // STARTED IN
        assert_eq!(cells[7]["spans"][0]["text"], "connection refused (cleaning up)");
        assert_eq!(cells[8], ""); // ECHO
    }

    #[test]
    fn header_has_a_kind_column_and_rows_fill_it() {
        let mut s = entry("dev", 1);
        s.kind = Some(SessionKind::Anonymous);
        let records = parse_lines(&build_ndjson(&[s], None, None, Path::new("/x"), false));
        let cols = records[0]["columns"].as_array().unwrap();
        let kind_col = cols.iter().position(|c| c["name"] == "KIND").unwrap();
        assert_eq!(kind_col, 4, "KIND sits after CLIENTS");
        let cells = records[1]["cells"].as_array().unwrap();
        assert_eq!(cells.len(), cols.len());
        assert_eq!(cells[kind_col], "anonymous");
        // An unknown kind leaves the cell blank.
        let mut u = entry("old", 0);
        u.kind = None;
        let records = parse_lines(&build_ndjson(&[u], None, None, Path::new("/x"), false));
        assert_eq!(records[1]["cells"][kind_col], "");
    }

    // ---- posh#215: the styled (TTY) table merges columns; plain keeps nine ----

    fn styled(sessions: &[SessionEntry], current: Option<&str>) -> Vec<Value> {
        parse_lines(&build_ndjson(sessions, current, Some("/home/u"), Path::new("/x"), true))
    }

    fn text(cell: &Value) -> String {
        match cell {
            Value::String(s) => s.clone(),
            v => v["spans"].as_array().unwrap().iter().map(|s| s["text"].as_str().unwrap()).collect(),
        }
    }

    #[test]
    fn the_styled_table_has_four_columns() {
        let records = styled(&[], None);
        let names: Vec<&str> =
            records[0]["columns"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["NAME", "STATUS", "CWD", "ACTIVITY"]);
    }

    #[test]
    fn an_anonymous_name_carries_a_dim_marker_and_a_named_one_none() {
        let mut anon = entry("s-3", 0);
        anon.kind = Some(SessionKind::Anonymous);
        let records = styled(&[entry("dev", 0), anon], None);
        assert_eq!(records[1]["cells"][0], "dev");
        let spans = records[2]["cells"][0]["spans"].as_array().unwrap();
        assert_eq!(spans[0]["text"], "s-3");
        assert_eq!((spans[1]["text"].as_str(), spans[1]["sev"].as_str()), (Some(" (anon)"), Some("muted")));
    }

    #[test]
    fn status_shows_the_client_count_only_while_attached() {
        let records = styled(&[entry("a", 2), entry("d", 0)], Some("a"));
        assert_eq!(text(&records[1]["cells"][1]), "\u{25cf} 2 (current)");
        assert_eq!(text(&records[2]["cells"][1]), "\u{25cf}");
    }

    #[test]
    fn cwd_shows_the_start_dir_only_when_the_session_moved() {
        let mut stayed = entry("s", 0);
        stayed.cwd_now = stayed.cwd.clone();
        let mut old = entry("o", 0);
        old.cwd_now = None; // pre-cascade daemon: the start dir alone
        let records = styled(&[entry("m", 0), stayed, old], None);
        assert_eq!(text(&records[1]["cells"][2]), "~/eng/posh  \u{2190} ~/eng");
        assert_eq!(records[2]["cells"][2], "~/eng");
        assert_eq!(records[3]["cells"][2], "~/eng");
    }

    #[test]
    fn activity_carries_the_echo_summary_when_one_is_reported() {
        let mut quiet = entry("q", 0);
        quiet.echo = None;
        let records = styled(&[entry("e", 1), quiet], None);
        assert_eq!(text(&records[1]["cells"][3]), "nvim \u{b7} echo optimistic auto-escalated 412ms");
        assert_eq!(records[2]["cells"][3], "nvim");
    }

    #[test]
    fn a_stale_styled_row_puts_its_error_under_activity() {
        let cells = styled(&[stale("old")], None)[1]["cells"].as_array().unwrap().clone();
        assert_eq!(cells.len(), 4);
        assert_eq!(cells[2], "");
        assert_eq!(text(&cells[3]), "connection refused (cleaning up)");
    }

    #[test]
    fn system_sessions_are_shown_only_when_asked_and_always_on_a_pipe() {
        let mut sys = entry("svc", 0);
        sys.kind = Some(SessionKind::System);
        let all = [entry("dev", 0), sys];
        let names = |v: Vec<&SessionEntry>| v.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(shown(&all, true, false)), ["dev"]);
        assert_eq!(names(shown(&all, true, true)), ["dev", "svc"]);
        assert_eq!(names(shown(&all, false, false)), ["dev", "svc"], "plumbing keeps every session");
    }

    /// posh#206: a daemon on another build is marked in STATUS (the anomaly
    /// only); a current one shows nothing extra.
    #[test]
    fn a_stale_daemons_status_names_its_build() {
        let mut old = entry("old", 0);
        (old.build, old.stale) = (Some("0.4.1+0ld1234".into()), true);
        let mut cur = entry("cur", 0);
        cur.build = Some("0.4.2+new".into());
        let records = styled(&[old, cur], None);
        assert_eq!(text(&records[1]["cells"][1]), "\u{25cf} old build 0ld1234");
        assert_eq!(text(&records[2]["cells"][1]), "\u{25cf}");
    }

    /// posh#206: the plain table appends BUILD (always, when known) as a
    /// tenth column, so the first nine keep their positions.
    #[test]
    fn the_plain_table_appends_a_build_column() {
        let mut s = entry("dev", 0);
        s.build = Some("0.4.2+abc".into());
        let records = parse_lines(&build_ndjson(&[s], None, None, Path::new("/x"), false));
        let cols = records[0]["columns"].as_array().unwrap();
        assert_eq!(cols.last().unwrap()["name"], "BUILD");
        assert_eq!(records[1]["cells"][cols.len() - 1], "0.4.2+abc");
    }

    /// An echo summary of `-` means no client is attached: nothing to add.
    #[test]
    fn a_detached_sessions_activity_has_no_echo_suffix() {
        let mut s = entry("d", 0);
        s.echo = Some("-".into());
        assert_eq!(styled(&[s], None)[1]["cells"][3], "nvim");
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
