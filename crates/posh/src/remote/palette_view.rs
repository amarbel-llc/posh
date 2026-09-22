//! Presentation of the FDR 0016 session stack for the RFC 0005 renderer —
//! the ONLY module that turns a [`picker::StackView`] into a heading string
//! or a palette row. Schema (`picker`) and presentation (here) are split on
//! purpose (design 2026-09-21 §3): the palette redesign edits this file, and
//! no stack mutation, wire, or client-loop code refers to a row label or a
//! title string.

use serde_json::{json, Value};

use crate::picker::{self, StackView};

/// Budget for a heading: posh-palette's panel is 46 columns with 2 of
/// padding and word-wraps beyond it.
const TITLE_BUDGET: usize = 42;

/// The Commands palette heading when the client has no prefix of its own.
const COMMANDS: &str = "Commands";

/// Character width of a heading fragment (the strings are ASCII-ish plus
/// `·` / `…` / `—`, each one column).
fn width(s: &str) -> usize {
    s.chars().count()
}

/// A target spelled the way the default title spells it — `short_host` :
/// `short_session` (`box:ff9fe216` for `me@box.example.com:ff9fe216-…`,
/// this machine's hostname for a local `:session`) — so the heading, the
/// *Back* row, and the dialog titles all use ONE spelling. A string that is
/// not a target is kept as is.
fn abbreviated(target: &str) -> String {
    match picker::short_target(target) {
        Some((host, session)) => format!("{host}:{session}"),
        None => target.to_string(),
    }
}

/// [`abbreviated`], with the session half cut to at most `max` characters
/// (a trailing `…` inside the count): a UUID name is already 8 digits, but a
/// long hand-picked `group/name` would otherwise push a heading past its
/// budget no matter how the rest is trimmed. The host half is left alone —
/// the heading drops it entirely when it has to.
pub(crate) fn short_back_target(target: &str, max: usize) -> String {
    match picker::short_target(target) {
        Some((host, session)) => format!("{host}:{}", clamp_tail(&session, max)),
        None => clamp_tail(target, max),
    }
}

/// `s` cut to `max` characters, ending in `…` when anything was cut.
fn clamp_tail(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}\u{2026}")
}

/// The Commands palette heading: the client's own prefix (the remote's
/// `rtt … · echo: …`, or `Commands` when the client has none) plus, with a
/// stack, ` · back: <top> [+N]` — N being the entries UNDER the top. The top
/// is abbreviated like the default title (`short_host`:`short_session`),
/// its session half bounded so that `Commands · back: :<session>[ +N]`
/// always fits ([`short_back_target`]); if the whole line still exceeds the
/// budget the host is dropped from the back target (`:session`), then the
/// prefix is truncated with `…` — the stack part is the reason the user
/// opened it.
pub fn commands_title(view: &StackView, prefix: Option<&str>) -> String {
    let prefix = prefix.unwrap_or(COMMANDS);
    let Some(top) = view.top.as_ref() else {
        return prefix.to_string();
    };
    let rest = match view.depth.saturating_sub(1) {
        0 => String::new(),
        n => format!(" +{n}"),
    };
    // The hostless suffix must leave room for at least `Commands`.
    let session_max = TITLE_BUDGET
        .saturating_sub(width(COMMANDS) + width(" \u{b7} back: :") + width(&rest))
        .max(1);
    let target = short_back_target(&top.target, session_max);
    let full = format!("{prefix} \u{b7} back: {target}{rest}");
    if width(&full) <= TITLE_BUDGET {
        return full;
    }
    // The session half: `short_target` split at the LAST `:`, so it has none.
    let session = target.rsplit_once(':').map_or(target.as_str(), |(_, s)| s);
    let suffix = format!(" \u{b7} back: :{session}{rest}");
    let hostless = format!("{prefix}{suffix}");
    if width(&hostless) <= TITLE_BUDGET {
        return hostless;
    }
    // Keep as much of the prefix as fits ahead of a trailing ellipsis.
    let room = TITLE_BUDGET.saturating_sub(width(&suffix)).saturating_sub(1);
    let head: String = prefix.chars().take(room).collect();
    format!("{}\u{2026}{suffix}", head.trim_end())
}

/// `Back to <top>` → `session.pop`, or None without a stack. Both clients
/// put it FIRST in the Commands palette while there is a top, so `Ctrl-^`
/// Enter is "go back". The top is spelled as the heading spells it
/// ([`abbreviated`]; rows wrap, so no width clamp).
pub fn back_row(view: &StackView) -> Option<Value> {
    let top = view.top.as_ref()?;
    Some(json!({ "name": format!("Back to {}", abbreviated(&top.target)), "action": { "method": "session.pop" } }))
}

/// The notice for a `session.pop` with nothing stacked.
pub fn no_back_notice() -> &'static str {
    "nothing to go back to"
}

/// The picker heading: [`picker::TITLE`], plus how deep the session stack
/// is when a *Back* would return somewhere (`sessions · 2 to go back`).
pub fn picker_title(view: &StackView) -> String {
    match view.depth {
        0 => picker::TITLE.to_string(),
        1 => format!("{} \u{b7} 1 to go back", picker::TITLE),
        n => format!("{} \u{b7} {n} to go back", picker::TITLE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::StackEntry;
    use posh_proto::caps::SessionKind;

    fn entry(t: &str, k: SessionKind) -> StackEntry {
        StackEntry { target: t.into(), kind: k }
    }

    fn view(top: Option<StackEntry>, depth: usize) -> StackView {
        StackView { top, depth, current: None }
    }

    const RTT: &str = "rtt 12ms \u{b7} echo: always";

    /// Without a stack the heading is the client's prefix (or `Commands`);
    /// with one it names the top — a local `:session` on this machine, a
    /// remote's host cut to its first label, a UUID name to 8 digits — and
    /// counts the entries under it.
    #[test]
    fn commands_title_names_the_top_and_counts_the_rest() {
        let none = view(None, 0);
        assert_eq!(commands_title(&none, Some(RTT)), RTT);
        assert_eq!(commands_title(&none, None), "Commands");
        let one = view(Some(entry(":s-1", SessionKind::Anonymous)), 1);
        let t = commands_title(&one, None);
        assert!(t.starts_with("Commands \u{b7} back: ") && t.ends_with(":s-1"), "{t}");
        assert!(!t.starts_with("Commands \u{b7} back: :"), "a local target names this machine: {t}");
        let many = view(
            Some(entry("me@box.example.com:grp/ff9fe216-9652-4e23-805c-6f4dd5ce7eca", SessionKind::Named)),
            3,
        );
        assert_eq!(commands_title(&many, None), "Commands \u{b7} back: box:grp/ff9fe216 +2");
        // Exactly at the budget: nothing is dropped.
        let at = view(Some(entry("me@box.example.com:dev", SessionKind::Named)), 3);
        let t = commands_title(&at, Some(RTT));
        assert_eq!(t, "rtt 12ms \u{b7} echo: always \u{b7} back: box:dev +2");
        assert_eq!(t.chars().count(), TITLE_BUDGET);
    }

    /// Over the budget, the host is dropped from the back target first
    /// (`:session` still says which session); only if that is not enough is
    /// the prefix cut with an ellipsis — the back part always survives whole.
    #[test]
    fn commands_title_drops_the_host_then_truncates_the_prefix() {
        let long_host = view(
            Some(entry("someone@very-long-hostname.internal.example.com:dev", SessionKind::Named)),
            1,
        );
        assert_eq!(commands_title(&long_host, Some(RTT)), "rtt 12ms \u{b7} echo: always \u{b7} back: :dev");
        let uuid = view(
            Some(entry("someone@very-long-hostname.internal.example.com:grp/ff9fe216-9652-4e23-805c-6f4dd5ce7eca", SessionKind::Named)),
            12,
        );
        let t = commands_title(&uuid, Some("rtt 1234ms \u{b7} echo: optimistic (auto)"));
        assert_eq!(t, "rtt 1234ms \u{b7} ec\u{2026} \u{b7} back: :grp/ff9fe216 +11");
        assert_eq!(t.chars().count(), TITLE_BUDGET);
    }

    /// A hand-picked name is never shortened by `short_session`, so a long
    /// one (or a `group/name`) is clamped at its tail with `…` before the
    /// stages run — else the suffix alone would overflow and no prefix
    /// truncation could save it. `Commands` always survives whole.
    #[test]
    fn commands_title_clamps_a_long_session_name() {
        let long = "a-deliberately-long-hand-picked-session-name-of-sixty-chars-";
        assert_eq!(long.chars().count(), 60);
        let v = view(Some(entry(&format!("me@box.example.com:{long}"), SessionKind::Named)), 1);
        let t = commands_title(&v, None);
        assert!(t.chars().count() <= TITLE_BUDGET, "{t} is {} cols", t.chars().count());
        assert!(t.starts_with("Commands \u{b7} back: :a-deliberately"), "{t}");
        assert!(t.ends_with('\u{2026}'), "{t}");
        let grouped = view(Some(entry(&format!("me@box.example.com:grp/{long}"), SessionKind::Named)), 4);
        let t = commands_title(&grouped, Some(RTT));
        assert!(t.chars().count() <= TITLE_BUDGET, "{t} is {} cols", t.chars().count());
        assert!(t.contains(" \u{b7} back: :grp/a-"), "{t}");
        assert!(t.ends_with("\u{2026} +3"), "the count survives, the name's tail does not: {t}");
        // The bound itself: the session half is cut to `max` with the ellipsis inside it.
        assert_eq!(short_back_target("me@box.example.com:dev", 3), "box:dev");
        assert_eq!(short_back_target("me@box.example.com:devel", 3), "box:de\u{2026}");
        assert_eq!(short_back_target("not a target", 3), "no\u{2026}");
    }

    /// The plan's budget case: a long host + a UUID name + the rtt/echo
    /// prefix must land at or under 42 columns (chars; the strings are
    /// ASCII-ish plus '·').
    #[test]
    fn commands_title_stays_inside_the_renderer_budget() {
        let v = view(
            Some(entry("someone@very-long-hostname.internal.example.com:grp/ff9fe216-9652-4e23-805c-6f4dd5ce7eca", SessionKind::Named)),
            12,
        );
        let t = commands_title(&v, Some("rtt 1234ms \u{b7} echo: optimistic (auto)"));
        assert!(t.chars().count() <= TITLE_BUDGET, "{t} is {} cols", t.chars().count());
        assert!(t.ends_with(" \u{b7} back: :grp/ff9fe216 +11"), "{t}");
    }

    /// The row names the top as the heading does: a short name whole, a
    /// UUID name by its first 8 digits, the host by its first label.
    #[test]
    fn back_row_is_present_only_with_a_top() {
        assert_eq!(back_row(&view(None, 0)), None);
        let one = view(Some(entry("box:dev", SessionKind::Named)), 1);
        assert_eq!(
            back_row(&one),
            Some(json!({ "name": "Back to box:dev", "action": { "method": "session.pop" } }))
        );
        let uuid = view(
            Some(entry("me@box.example.com:ff9fe216-9652-4e23-805c-6f4dd5ce7eca", SessionKind::Anonymous)),
            1,
        );
        assert_eq!(back_row(&uuid).unwrap()["name"], "Back to box:ff9fe216");
    }

    #[test]
    fn a_pop_with_nothing_stacked_says_so() {
        assert_eq!(no_back_notice(), "nothing to go back to");
    }

    #[test]
    fn picker_title_counts_the_stack() {
        assert_eq!(picker_title(&view(None, 0)), "sessions");
        assert_eq!(picker_title(&view(Some(entry(":a", SessionKind::Named)), 1)), "sessions \u{b7} 1 to go back");
        assert_eq!(picker_title(&view(Some(entry(":a", SessionKind::Named)), 2)), "sessions \u{b7} 2 to go back");
    }

    /// *Back* is a bare `session.pop`: no target (the client resolves it from
    /// the stack) and no `previous` — a transition never kills, so the row IS
    /// the whole answer.
    #[test]
    fn back_row_is_a_bare_pop_with_no_params() {
        let row = back_row(&view(Some(entry(":prev", SessionKind::Named)), 1)).unwrap();
        assert_eq!(row["action"]["method"], "session.pop");
        assert!(row["action"].get("params").is_none(), "{row}");
    }
}
