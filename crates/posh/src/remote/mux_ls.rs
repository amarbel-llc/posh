//! `posh mux ls` as a mesa table (RFC 0003 List-Table NDJSON, rendered by
//! the same `mesa` binary `posh list` uses): one row per endpoint this host
//! DIALS (the per-destination mux daemons under `<base>/mux/`, role
//! `client`) and one per peer it SERVES (the mux-peer status sockets under
//! `<base>/agent/`, RFC 0013 §4, role `served`).
//!
//! The rows are parsed from the daemons' own status one-liners — the
//! `mux <key>: self=… state=… …` line a daemon answers `Status` with, and
//! the `posh <ver> (<sha>) pid=… …` line a served endpoint writes to its
//! status socket — rather than from a second wire shape, so an older daemon
//! that reports fewer fields degrades to blank cells, and `mux ls --raw`
//! (the verbatim lines) stays the single source of truth for a soak grep.

use serde_json::{json, Value};

use crate::remote::mux;
use crate::session::mesa;
use crate::util::Result;

/// Which side of a mux connection this host is on for the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A per-destination daemon this host runs toward a remote.
    Client,
    /// A remote host's endpoint served by this machine (its agent peer).
    Served,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Role::Client => "client",
            Role::Served => "served",
        }
    }
}

/// The STATUS dot: a probe verdict, not a connection state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Live,
    /// The socket exists but nothing answers (a daemon that died without
    /// unlinking) — or the served peer's status read failed.
    Stale,
    /// A pre-upgrade daemon still serving its old protocol generation.
    OldGeneration,
}

impl Health {
    fn sev(self) -> &'static str {
        match self {
            Health::Live => "ok",
            Health::Stale => "error",
            Health::OldGeneration => "accent",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Health::Live => "live",
            Health::Stale => "stale",
            Health::OldGeneration => "old-generation",
        }
    }
}

/// One table row: the endpoint's key, role, probe verdict, and the status
/// line's fields (`k=v`, in reported order) — plus, for a non-live row, the
/// verdict's reason text.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub role: Role,
    pub health: Health,
    pub note: String,
    /// A served peer's build, reported as the line's prefix (`posh 1.2.3
    /// (sha)`) rather than a `self=` field.
    pub build: String,
    pub fields: Vec<(String, String)>,
}

impl Entry {
    fn field(&self, name: &str) -> &str {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map_or("", |(_, v)| v.as_str())
    }
}

/// Splits a status line's body into its leading free text and its `k=v`
/// pairs. A whitespace token without `=` extends the previous value (so
/// `self=1.2.3 (abc)` keeps its parenthesized sha) or, before any pair,
/// the prefix.
fn parse_kv(body: &str) -> (String, Vec<(String, String)>) {
    let mut prefix = String::new();
    let mut fields: Vec<(String, String)> = Vec::new();
    for tok in body.split_whitespace() {
        match tok.split_once('=') {
            Some((k, v)) if !k.is_empty() => fields.push((k.to_string(), v.to_string())),
            _ => match fields.last_mut() {
                Some((_, v)) => {
                    v.push(' ');
                    v.push_str(tok);
                }
                None => {
                    if !prefix.is_empty() {
                        prefix.push(' ');
                    }
                    prefix.push_str(tok);
                }
            },
        }
    }
    (prefix, fields)
}

/// Parses one `mux <key>: <body>` line of [`mux::mux_ls`] output.
fn client_entry(line: &str) -> Option<Entry> {
    let rest = line.strip_prefix("mux ")?;
    let (key, body) = rest.split_once(": ")?;
    let (prefix, fields) = parse_kv(body);
    let (health, note) = if body.starts_with("stale") {
        (Health::Stale, body.to_string())
    } else if body.starts_with("old-generation") {
        (Health::OldGeneration, body.to_string())
    } else {
        (Health::Live, String::new())
    };
    Some(Entry {
        key: key.to_string(),
        role: Role::Client,
        health,
        note,
        build: if health == Health::Live { prefix } else { String::new() },
        fields,
    })
}

/// Parses one `endpoint <name>: <body>` line of [`mux::endpoint_status_ls`]
/// output: the body's prefix is the served endpoint's build (`posh 1.2.3
/// (sha)`).
fn served_entry(line: &str) -> Option<Entry> {
    let rest = line.strip_prefix("endpoint ")?;
    let (key, body) = rest.split_once(": ")?;
    let (prefix, fields) = parse_kv(body);
    let (health, note) = if body.starts_with("stale") {
        (Health::Stale, body.to_string())
    } else {
        (Health::Live, String::new())
    };
    Some(Entry {
        key: key.to_string(),
        role: Role::Served,
        health,
        note,
        build: prefix.strip_prefix("posh ").unwrap_or(&prefix).to_string(),
        fields,
    })
}

/// The table rows from the two raw listings (their "nothing here" sentinels
/// contribute no rows). Pure, so the shape is testable without sockets.
pub fn entries_from_raw(mux_raw: &str, served_raw: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    if mux_raw != mux::MUX_LS_EMPTY {
        out.extend(mux_raw.lines().filter_map(client_entry));
    }
    if served_raw != mux::ENDPOINT_LS_EMPTY {
        out.extend(served_raw.lines().filter_map(served_entry));
    }
    out
}

/// `heard=1234ms` → `1.2s`; blank when unreported.
fn heard_cell(v: &str) -> String {
    let Some(ms) = v.strip_suffix("ms").and_then(|n| n.parse::<u64>().ok()) else {
        return v.to_string();
    };
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// The key under the table (RFC 0003 §6.1 `footer`): what the columns a
/// reader cannot guess mean. Rendered dim on a tty, verbatim on a pipe
/// (after the rows, untabbed); an older `mesa` ignores it.
const FOOTER: [&str; 2] = [
    "SELF: the build the long-lived daemon actually runs (an upgrade on disk does not change it; \
     blank = a pre-self-ident daemon)",
    "REMOTE: the endpoint's build (unknown before RFC 0013, or mid-reconnect) \u{b7} \
     CHANNELS: agent/session \u{b7} --raw: the full status lines",
];

/// The header record: columns, the STATUS-dot legend, the key, and the
/// empty-table message. SELF/REMOTE/PEER are `flex` and shrink in that
/// order (lowest `shrink` first); the counts are `pin`.
fn header() -> Value {
    json!({
        "columns": [
            {"name": "ENDPOINT", "role": "pin"},
            {"name": "ROLE", "role": "pin"},
            {"name": "STATUS", "role": "pin"},
            {"name": "STATE", "role": "pin"},
            {"name": "SELF", "role": "flex", "shrink": 0, "min": 8},
            {"name": "REMOTE", "role": "flex", "shrink": 1, "min": 8},
            {"name": "PEER", "role": "flex", "shrink": 2, "min": 8},
            {"name": "HEARD", "role": "pin"},
            {"name": "CHANNELS", "role": "pin"},
            {"name": "REFS", "role": "pin"},
            {"name": "LINGER", "role": "pin"},
        ],
        "legend": [
            {"sev": Health::Live.sev(), "glyph": "\u{25cf}", "label": Health::Live.label()},
            {"sev": Health::Stale.sev(), "glyph": "\u{25cf}", "label": Health::Stale.label()},
            {"sev": Health::OldGeneration.sev(), "glyph": "\u{25cf}", "label": Health::OldGeneration.label()},
        ],
        "footer": FOOTER,
        "empty": EMPTY,
    })
}

/// The empty-table message (both dirs empty).
pub const EMPTY: &str = "no mux endpoints";

fn dot(health: Health) -> Value {
    json!({"spans": [{"text": "\u{25cf}", "sev": health.sev()}]})
}

/// One ROW record. A non-live row carries the verdict's reason in the
/// STATE cell (dimmed) and leaves the data cells blank.
fn row(e: &Entry) -> Value {
    if e.health != Health::Live {
        return json!({"cells": [
            e.key,
            e.role.label(),
            dot(e.health),
            {"spans": [{"text": e.note, "sev": "muted"}]},
            "", "", "", "", "", "", "",
        ]});
    }
    let (state, slf, remote, refs, linger) = match e.role {
        Role::Client => (
            e.field("state").to_string(),
            e.field("self").to_string(),
            e.field("remote").to_string(),
            e.field("refs").to_string(),
            e.field("linger").to_string(),
        ),
        // A served peer is a process of THIS host serving a remote's
        // agent: its build is the line's prefix; the remote's build is not
        // reported on that line; refs/linger are a client daemon's.
        Role::Served => ("serving".to_string(), e.build.clone(), String::new(), String::new(), String::new()),
    };
    let agent_channels = match e.role {
        Role::Client => e.field("channels"),
        Role::Served => e.field("agent_channels"),
    };
    let peer = match e.field("peer") {
        "none" | "" => String::new(),
        p => p.to_string(),
    };
    json!({"cells": [
        e.key,
        e.role.label(),
        dot(e.health),
        state,
        slf,
        remote,
        peer,
        heard_cell(e.field("heard")),
        format!("{agent_channels}/{}", e.field("session_channels")),
        refs,
        linger,
    ]})
}

/// The full NDJSON stream (header, then one row per entry).
pub fn build_ndjson(entries: &[Entry]) -> String {
    let mut out = header().to_string();
    out.push('\n');
    for e in entries {
        out.push_str(&row(e).to_string());
        out.push('\n');
    }
    out
}

/// `posh mux ls`: probe both dirs and render the table through `mesa`.
pub fn render() -> Result<()> {
    let entries = entries_from_raw(&mux::mux_ls()?, &mux::endpoint_status_ls()?);
    mesa::pipe(&build_ndjson(&entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = "mux box: self=0.9.1 (abc1234) state=connected peer=100.64.0.2:60001 remote=0.9.0 (def5678) heard=1234ms channels=2 session_channels=1 refs=3 linger=off cwnd=8192 cuts=0 streak_hwm=0\n";

    #[test]
    fn kv_parse_keeps_parenthesized_values_and_the_prefix() {
        let (prefix, fields) = parse_kv("posh 9.9.9 (cafef00) pid=7 peer=none heard=5ms");
        assert_eq!(prefix, "posh 9.9.9 (cafef00)");
        assert_eq!(fields[0], ("pid".to_string(), "7".to_string()));
        let (_, fields) = parse_kv("self=0.9.1 (abc1234) state=connected");
        assert_eq!(fields[0], ("self".to_string(), "0.9.1 (abc1234)".to_string()));
        assert_eq!(fields[1], ("state".to_string(), "connected".to_string()));
    }

    #[test]
    fn empty_sentinels_yield_no_rows() {
        assert!(entries_from_raw(mux::MUX_LS_EMPTY, mux::ENDPOINT_LS_EMPTY).is_empty());
        let records: Vec<Value> = build_ndjson(&[])
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["empty"], EMPTY);
        assert_eq!(records[0]["columns"].as_array().unwrap().len(), 11);
        assert_eq!(records[0]["legend"].as_array().unwrap().len(), 3);
        // The key rides the header as §6.1 footer lines (bare strings =
        // muted prose), one per FOOTER entry.
        let footer = records[0]["footer"].as_array().unwrap();
        assert_eq!(footer.len(), FOOTER.len());
        assert!(footer[0].as_str().unwrap().starts_with("SELF:"));
    }

    #[test]
    fn client_rows_carry_state_builds_peer_heard_and_counts() {
        let raw = format!(
            "mux dead: stale (connection refused)\n{LIVE}mux old: old-generation daemon (stamp 3, ours 4)\n"
        );
        let entries = entries_from_raw(&raw, mux::ENDPOINT_LS_EMPTY);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].health, Health::Stale);
        assert_eq!(entries[2].health, Health::OldGeneration);
        let live = &entries[1];
        assert_eq!(live.role, Role::Client);
        assert_eq!(live.field("self"), "0.9.1 (abc1234)");
        let r = row(live);
        let cells = r["cells"].as_array().unwrap();
        assert_eq!(cells[0], "box");
        assert_eq!(cells[1], "client");
        assert_eq!(cells[2]["spans"][0]["sev"], "ok");
        assert_eq!(cells[3], "connected");
        assert_eq!(cells[4], "0.9.1 (abc1234)");
        assert_eq!(cells[5], "0.9.0 (def5678)");
        assert_eq!(cells[6], "100.64.0.2:60001");
        assert_eq!(cells[7], "1.2s");
        assert_eq!(cells[8], "2/1");
        assert_eq!(cells[9], "3");
        assert_eq!(cells[10], "off");
        // The stale row: reason in STATE (dimmed), data cells blank.
        let r = row(&entries[0]);
        let cells = r["cells"].as_array().unwrap();
        assert_eq!(cells[2]["spans"][0]["sev"], "error");
        assert_eq!(cells[3]["spans"][0]["text"], "stale (connection refused)");
        assert_eq!(cells[4], "");
    }

    #[test]
    fn served_rows_take_the_build_from_the_prefix() {
        let raw = "endpoint mux-alpha: posh 9.9.9 (cafef00) pid=7 peer=100.64.0.9:5000 heard=90000ms agent_channels=1 opened_total=4 owns_agent_sock=true session_channels=0\n\
                   endpoint mux-beta: stale (connection refused)\n";
        let entries = entries_from_raw(mux::MUX_LS_EMPTY, raw);
        assert_eq!(entries.len(), 2);
        let r = row(&entries[0]);
        let cells = r["cells"].as_array().unwrap();
        assert_eq!(cells[0], "mux-alpha");
        assert_eq!(cells[1], "served");
        assert_eq!(cells[3], "serving");
        assert_eq!(cells[4], "9.9.9 (cafef00)");
        assert_eq!(cells[5], "");
        assert_eq!(cells[7], "1m30s");
        assert_eq!(cells[8], "1/0");
        assert_eq!(entries[1].health, Health::Stale);
    }

    #[test]
    fn an_older_daemon_reporting_fewer_fields_degrades_to_blank_cells() {
        let raw = "mux old: state=connected peer=none heard=5ms channels=0 refs=1 linger=armed\n";
        let entries = entries_from_raw(raw, mux::ENDPOINT_LS_EMPTY);
        let r = row(&entries[0]);
        let cells = r["cells"].as_array().unwrap();
        assert_eq!(cells[4], "", "no self= reported");
        assert_eq!(cells[5], "", "no remote= reported");
        assert_eq!(cells[6], "", "peer=none reads blank");
        assert_eq!(cells[8], "0/", "session_channels unreported");
    }
}
