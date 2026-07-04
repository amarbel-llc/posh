//! The styled `posh list` table: rounded borders, a state-colored STATUS
//! dot with a legend footer — the `sc list` look — rendered only when
//! stdout is a TTY. The plain, `--short`, and `--json` outputs are
//! unchanged (scripts, pipes, and the remote/completion probes rely on
//! them), so this is purely the interactive default.

use super::SessionEntry;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "1";
const DIM: &str = "2";
const GREEN: &str = "32";
const CYAN: &str = "36";
const RED: &str = "31";

/// A dim column separator, constant so per-row lines don't re-allocate it.
const DIM_BAR: &str = "\x1b[2m\u{2502}\x1b[0m";

fn paint(code: &str, s: &str) -> String {
    format!("\x1b[{code}m{s}{RESET}")
}

/// Display width of ANSI-free text, wcwidth per char (CJK/emoji are two
/// columns, combining marks zero) so wide chars in paths and commands
/// keep the grid aligned.
fn width(s: &str) -> usize {
    s.chars().map(|c| posh_term::wcwidth(c) as usize).sum()
}

/// Truncates to `max` columns with a trailing ellipsis when cut; returns
/// the (possibly cut) text and its display width.
fn truncate(s: &str, max: usize) -> (String, usize) {
    let full = width(s);
    if full <= max {
        return (s.to_string(), full);
    }
    let keep = max.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = posh_term::wcwidth(c) as usize;
        if w + cw > keep {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    (out, w + 1)
}

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

/// The session's resolved display state, keyed to the STATUS dot color.
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

    fn color(self) -> &'static str {
        match self {
            State::Attached => GREEN,
            State::Detached => CYAN,
            State::Stale => RED,
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

/// One rendered STATUS cell: prestyled text plus its display width (the
/// styled text cannot be measured with `len()`).
struct StatusCell {
    text: String,
    width: usize,
}

/// The STATUS cell: a state-colored dot plus an optional dim marker
/// (`(current)` for the session this client is running inside). State is
/// carried by the dot's color alone — the legend footer is the key.
fn status_cell(state: State, current: bool) -> StatusCell {
    let mut text = paint(state.color(), "\u{25cf}");
    let mut w = 1;
    if current {
        text.push(' ');
        text.push_str(&paint(DIM, "(current)"));
        w += 1 + width("(current)");
    }
    StatusCell { text, width: w }
}

/// The dim key decoding the STATUS dot colors, rendered once in the
/// table's merged footer row.
fn legend() -> (String, usize) {
    let mut text = String::new();
    let mut w = 0;
    for (i, state) in [State::Attached, State::Detached, State::Stale]
        .into_iter()
        .enumerate()
    {
        if i > 0 {
            text.push_str(&paint(DIM, " \u{b7} "));
            w += 3;
        }
        text.push_str(&paint(state.color(), "\u{25cf}"));
        text.push_str(&paint(DIM, &format!(" {}", state.label())));
        w += 2 + width(state.label());
    }
    (text, w)
}

/// One table row, content unstyled so columns can be measured and
/// truncated before styling. `dim_cmd` marks the CMD column as secondary
/// text (the stale-socket error message rides there).
struct Row {
    name: String,
    status: StatusCell,
    pid: String,
    clients: String,
    dir: String,
    cmd: String,
    dim_cmd: bool,
}

fn row(s: &SessionEntry, current: Option<&str>, home: Option<&str>) -> Row {
    let state = State::of(s);
    if let Some(err) = &s.error {
        return Row {
            name: s.name.clone(),
            status: status_cell(state, false),
            pid: String::new(),
            clients: String::new(),
            dir: String::new(),
            cmd: format!("{err} (cleaning up)"),
            dim_cmd: true,
        };
    }
    Row {
        name: s.name.clone(),
        status: status_cell(state, current == Some(s.name.as_str())),
        pid: s.pid.map(|p| p.to_string()).unwrap_or_default(),
        clients: s.clients.map(|c| c.to_string()).unwrap_or_default(),
        dir: abbrev_home(s.cwd.as_deref().unwrap_or(""), home),
        cmd: s.cmd.clone().unwrap_or_default(),
        dim_cmd: false,
    }
}

const HEADERS: [&str; 6] = ["NAME", "STATUS", "PID", "CLIENTS", "STARTED IN", "CMD"];
/// 0-based indexes of the flexible columns, shrunk (CMD first) when the
/// table overflows the terminal; every other column is content-sized.
const CMD_COL: usize = 5;
const DIR_COL: usize = 4;
/// Narrowest a flexible column shrinks to before we give up and overflow.
const MIN_FLEX: usize = 8;

/// Content widths per column: the widest cell, seeded with the header
/// widths (computed once by the caller, reused for the header line).
fn column_widths(rows: &[Row], mut w: [usize; 6]) -> [usize; 6] {
    for r in rows {
        w[0] = w[0].max(width(&r.name));
        w[1] = w[1].max(r.status.width);
        w[2] = w[2].max(width(&r.pid));
        w[3] = w[3].max(width(&r.clients));
        w[4] = w[4].max(width(&r.dir));
        w[5] = w[5].max(width(&r.cmd));
    }
    w
}

/// Total rendered width for the given column widths: one leading border
/// plus `content + 2 padding + 1 border` per column.
fn table_width(cols: &[usize; 6]) -> usize {
    1 + cols.iter().map(|w| w + 3).sum::<usize>()
}

/// Shrinks the flexible columns (CMD, then STARTED IN) until the table
/// fits `term_cols`. 0 means unknown width: keep the content-sized layout.
fn fit(mut cols: [usize; 6], term_cols: usize) -> [usize; 6] {
    if term_cols == 0 {
        return cols;
    }
    for col in [CMD_COL, DIR_COL] {
        let over = table_width(&cols).saturating_sub(term_cols);
        if over == 0 {
            return cols;
        }
        cols[col] = cols[col].saturating_sub(over).max(MIN_FLEX.min(cols[col]));
    }
    cols
}

/// A horizontal border line, e.g. `rule('╭','┬','╮', ..)` for the top.
fn rule(left: char, mid: char, right: char, cols: &[usize; 6]) -> String {
    let mut s = String::new();
    s.push(left);
    for (i, w) in cols.iter().enumerate() {
        if i > 0 {
            s.push(mid);
        }
        for _ in 0..w + 2 {
            s.push('\u{2500}');
        }
    }
    s.push(right);
    paint(DIM, &s)
}

/// One body/header line: prestyled cell texts framed by dim `│` borders.
/// `widths` are the (possibly shrunk) column widths; `cell_widths` the
/// actual display widths of the passed texts.
fn line(cells: &[String; 6], cell_widths: &[usize; 6], cols: &[usize; 6]) -> String {
    let mut s = String::new();
    for i in 0..6 {
        s.push_str(DIM_BAR);
        s.push(' ');
        s.push_str(&cells[i]);
        for _ in 0..cols[i].saturating_sub(cell_widths[i]) + 1 {
            s.push(' ');
        }
    }
    s.push_str(DIM_BAR);
    s
}

/// Renders the full table with the merged legend footer. Pure: `term_cols`
/// is stdout's column count (0 = unknown, don't shrink), `home` abbreviates
/// STARTED IN. The caller has already checked stdout is a TTY.
pub(super) fn render(
    sessions: &[SessionEntry],
    current: Option<&str>,
    term_cols: usize,
    home: Option<&str>,
) -> String {
    let rows: Vec<Row> = sessions.iter().map(|s| row(s, current, home)).collect();
    let header_widths = HEADERS.map(width);
    let cols = fit(column_widths(&rows, header_widths), term_cols);

    let mut out = String::new();
    out.push_str(&rule('\u{256d}', '\u{252c}', '\u{256e}', &cols));
    out.push('\n');

    let headers = HEADERS.map(|h| paint(BOLD, h));
    out.push_str(&line(&headers, &header_widths, &cols));
    out.push('\n');
    out.push_str(&rule('\u{251c}', '\u{253c}', '\u{2524}', &cols));
    out.push('\n');

    for r in &rows {
        let (dir, dir_width) = truncate(&r.dir, cols[DIR_COL]);
        let (cmd, cmd_width) = truncate(&r.cmd, cols[CMD_COL]);
        let widths = [
            width(&r.name),
            r.status.width,
            width(&r.pid),
            width(&r.clients),
            dir_width,
            cmd_width,
        ];
        let cmd = if r.dim_cmd { paint(DIM, &cmd) } else { cmd };
        let cells = [
            r.name.clone(),
            r.status.text.clone(),
            r.pid.clone(),
            r.clients.clone(),
            dir,
            cmd,
        ];
        out.push_str(&line(&cells, &widths, &cols));
        out.push('\n');
    }

    // The legend footer merges the columns: a ┴ connector row, the
    // centered key, then a flat bottom border. When the table is too
    // narrow to frame the key, close the table normally and append the
    // key as a plain dim line instead.
    let total = table_width(&cols);
    let inner = total - 2;
    let (key, key_width) = legend();
    if key_width + 2 <= inner {
        out.push_str(&rule('\u{251c}', '\u{2534}', '\u{2524}', &cols));
        out.push('\n');
        let pad = inner - key_width;
        let (left, right) = (pad / 2, pad - pad / 2);
        out.push_str(&format!(
            "{DIM_BAR}{}{key}{}{DIM_BAR}\n",
            " ".repeat(left),
            " ".repeat(right)
        ));
        let mut bottom = String::from('\u{2570}');
        for _ in 0..inner {
            bottom.push('\u{2500}');
        }
        bottom.push('\u{256f}');
        out.push_str(&paint(DIM, &bottom));
        out.push('\n');
    } else {
        out.push_str(&rule('\u{2570}', '\u{2534}', '\u{256f}', &cols));
        out.push('\n');
        out.push_str(&key);
        out.push('\n');
    }
    out
}

/// The empty-group message. Single source of truth for the wording: the
/// plain path prints it bare, [`render_empty`] dims it — the two stay
/// grep-compatible by construction.
pub(super) fn empty_message(socket_dir: &std::path::Path) -> String {
    format!("no sessions found in {}", socket_dir.display())
}

/// The dim TTY variant of [`empty_message`].
pub(super) fn render_empty(socket_dir: &std::path::Path) -> String {
    format!("{}\n", paint(DIM, &empty_message(socket_dir)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn entry(name: &str, clients: u64) -> SessionEntry {
        SessionEntry {
            name: name.to_string(),
            pid: Some(4242),
            clients: Some(clients),
            error: None,
            cmd: Some("fish".to_string()),
            cwd: Some("/home/u/eng".to_string()),
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
        }
    }

    #[test]
    fn table_has_headers_borders_and_legend() {
        let out = render(&[entry("dev", 1)], None, 0, None);
        let plain = strip_ansi(&out);
        for h in HEADERS {
            assert!(plain.contains(h), "missing header {h}");
        }
        assert!(plain.contains('\u{256d}') && plain.contains('\u{256f}'));
        // The legend footer merges columns (┴ connector) and carries the key.
        assert!(plain.contains('\u{2534}'));
        for label in ["attached", "detached", "stale"] {
            assert!(plain.contains(label), "legend missing {label}");
        }
    }

    #[test]
    fn every_line_has_equal_display_width() {
        let sessions = [entry("dev", 1), entry("longer-name", 0), stale("old")];
        let out = render(&sessions, Some("dev"), 0, Some("/home/u"));
        let plain = strip_ansi(&out);
        let mut lines = plain.lines().filter(|l| !l.is_empty());
        let first = lines.next().unwrap().chars().count();
        for l in lines {
            assert_eq!(l.chars().count(), first, "misaligned line: {l:?}");
        }
    }

    #[test]
    fn state_colors_attached_detached_stale() {
        let out = render(&[entry("a", 2), entry("d", 0), stale("s")], None, 0, None);
        // One green dot for attached, cyan for detached, red for stale —
        // plus one of each in the legend.
        assert_eq!(out.matches("\x1b[32m\u{25cf}").count(), 2);
        assert_eq!(out.matches("\x1b[36m\u{25cf}").count(), 2);
        assert_eq!(out.matches("\x1b[31m\u{25cf}").count(), 2);
    }

    #[test]
    fn current_session_is_marked() {
        let out = render(&[entry("dev", 1), entry("other", 0)], Some("dev"), 0, None);
        assert_eq!(strip_ansi(&out).matches("(current)").count(), 1);
    }

    #[test]
    fn stale_row_carries_error_in_cmd_column() {
        let out = render(&[stale("old")], None, 0, None);
        assert!(strip_ansi(&out).contains("connection refused (cleaning up)"));
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
    fn narrow_terminal_shrinks_cmd_then_dir() {
        let long = || {
            let mut e = entry("dev", 1);
            e.cmd = Some("a-very-long-create-command --with flags".to_string());
            e.cwd = Some("/deeply/nested/working/directory/path".to_string());
            e
        };
        let wide = strip_ansi(&render(&[long()], None, 0, None));
        let wide_w = wide.lines().next().unwrap().chars().count();
        let narrow = strip_ansi(&render(&[long()], None, 60, None));
        let narrow_w = narrow.lines().next().unwrap().chars().count();
        assert!(narrow_w < wide_w, "narrow ({narrow_w}) not < wide ({wide_w})");
        assert!(narrow.contains('\u{2026}'), "truncation ellipsis missing");
        // All lines still align after shrinking.
        for l in narrow.lines().filter(|l| !l.is_empty()) {
            assert_eq!(l.chars().count(), narrow_w, "misaligned: {l:?}");
        }
    }

    #[test]
    fn wide_chars_keep_grid_aligned() {
        // CJK in a path is two columns per char; the grid must stay
        // aligned when measured in display columns (wcwidth), which a
        // chars-as-columns approximation would get wrong.
        let mut e = entry("dev", 1);
        e.cwd = Some("/home/u/\u{6587}\u{6863}/\u{9879}\u{76ee}".to_string());
        let out = render(&[e, entry("other", 0)], None, 0, None);
        let plain = strip_ansi(&out);
        let mut lines = plain.lines().filter(|l| !l.is_empty());
        let first = width(lines.next().unwrap());
        for l in lines {
            assert_eq!(width(l), first, "misaligned line: {l:?}");
        }
    }

    #[test]
    fn empty_message_matches_plain_wording() {
        let out = render_empty(std::path::Path::new("/run/posh/default"));
        assert!(strip_ansi(&out).contains("no sessions found in /run/posh/default"));
    }
}
