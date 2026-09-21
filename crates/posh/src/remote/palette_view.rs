//! Presentation of the FDR 0016 session stack for the RFC 0005 renderer —
//! the ONLY module that turns a [`picker::StackView`] into a heading string
//! or a palette row. Schema (`picker`) and presentation (here) are split on
//! purpose (design 2026-09-21 §3): the palette redesign edits this file, and
//! no stack mutation, wire, or client-loop code refers to a row label or a
//! title string.

use serde_json::{json, Value};

use crate::picker::{self, Previous, StackView};

/// Budget for a heading: posh-palette's panel is 46 columns with 2 of
/// padding and word-wraps beyond it.
const TITLE_BUDGET: usize = 42;

/// The Commands palette heading when the client has no prefix of its own.
const COMMANDS: &str = "Commands";

/// The Commands palette heading: the client's own prefix (the remote's
/// `rtt … · echo: …`, or `Commands` when the client has none) plus, with a
/// stack, ` · back: <top> [+N]` — N being the entries UNDER the top. The top
/// is abbreviated like the default title (`short_host`:`short_session`); if
/// the whole line still exceeds the budget the host is dropped from the back
/// target (`:session`), then the prefix is truncated with `…` — the stack
/// part is the reason the user opened it.
// Wired into both clients' `open_palette` by the next change (plan Task 13).
#[allow(dead_code)]
pub fn commands_title(view: &StackView, prefix: Option<&str>) -> String {
    let prefix = prefix.unwrap_or(COMMANDS);
    let Some(top) = view.top.as_ref() else {
        return prefix.to_string();
    };
    let (host, session) = picker::short_target(&top.target)
        .unwrap_or_else(|| (String::new(), top.target.clone()));
    let rest = match view.depth.saturating_sub(1) {
        0 => String::new(),
        n => format!(" +{n}"),
    };
    let width = |s: &str| s.chars().count();
    let full = format!("{prefix} \u{b7} back: {host}:{session}{rest}");
    if width(&full) <= TITLE_BUDGET {
        return full;
    }
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

/// `Back to <top>` → `session.pop`, or None without a stack.
// Wired into both clients' `palette_commands` by the next change (plan Task 13).
#[allow(dead_code)]
pub fn back_row(view: &StackView) -> Option<Value> {
    let top = view.top.as_ref()?;
    Some(json!({ "name": format!("Back to {}", top.target), "action": { "method": "session.pop" } }))
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

/// The second step after a picker row is chosen from INSIDE a session: the
/// palette asking what to do with the session being left. Each command
/// re-issues `session.switch` with the same `target` and a `previous`.
/// Keeping it is a PUSH: the session goes on the stack and *Back* returns
/// to it (FDR 0016, stacked switching).
pub fn leave_commands(target: &str) -> Value {
    leave_commands_for("session.switch", Some(target))
}

/// The leave question for *Back* (`session.pop`): the same three fates for
/// the session being left, re-issued as `session.pop` with a `previous`
/// (the target is the stack's top, never named by the renderer).
pub fn back_commands() -> Value {
    leave_commands_for("session.pop", None)
}

/// The three fates (+ cancel) for the session being LEFT, each re-issuing
/// `method` with a `previous`. The session being left is named ONCE by the
/// CALLER in the dialog title (RFC 0005 `title`), NOT repeated in every answer,
/// so the answers read as fates of "it".
fn leave_commands_for(method: &str, target: Option<&str>) -> Value {
    let cmd = |name: &str, previous: Previous| {
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
        cmd("Keep it running", Previous::Keep),
        cmd("Kill it (kept if other viewports are attached)", Previous::Kill),
        cmd("Kill it even with other viewports attached", Previous::ForceKill),
        { "name": "Cancel" },
    ])
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

    #[test]
    fn back_row_is_present_only_with_a_top() {
        assert_eq!(back_row(&view(None, 0)), None);
        let one = view(Some(entry("box:dev", SessionKind::Named)), 1);
        assert_eq!(
            back_row(&one),
            Some(json!({ "name": "Back to box:dev", "action": { "method": "session.pop" } }))
        );
    }

    #[test]
    fn picker_title_counts_the_stack() {
        assert_eq!(picker_title(&view(None, 0)), "sessions");
        assert_eq!(picker_title(&view(Some(entry(":a", SessionKind::Named)), 1)), "sessions \u{b7} 1 to go back");
        assert_eq!(picker_title(&view(Some(entry(":a", SessionKind::Named)), 2)), "sessions \u{b7} 2 to go back");
    }

    /// The back question re-issues session.pop with a previous, no target.
    /// The session being left is named in the dialog title (the caller), NOT
    /// in every answer — so the answers are generic fates of "it".
    #[test]
    fn back_commands_reissue_pop_with_a_previous_and_no_target() {
        let cmds = back_commands();
        let arr = cmds.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0]["action"]["method"], "session.pop");
        assert!(arr[0]["action"]["params"].get("target").is_none());
        assert_eq!(arr[1]["action"]["params"]["previous"], "kill");
        assert_eq!(arr[0]["name"], "Keep it running");
    }

    /// The leave step: three `session.switch` re-issues carrying the chosen
    /// target and a `previous`, plus a no-op cancel; `previous` parses back,
    /// and an unknown spelling is rejected (absent = keep). The session being
    /// left is named in the dialog title (the caller), not in the answers, so
    /// each answer is a generic fate of "it".
    #[test]
    fn leave_commands_carry_target_and_previous() {
        let cmds = leave_commands("box:dev");
        let arr = cmds.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        let names = ["Keep it running", "Kill it", "Kill it"];
        for (i, want) in ["keep", "kill", "force-kill"].iter().enumerate() {
            assert_eq!(arr[i]["action"]["method"], "session.switch");
            assert_eq!(arr[i]["action"]["params"]["target"], "box:dev");
            assert_eq!(arr[i]["action"]["params"]["previous"], *want);
            assert_eq!(Previous::parse(Some(want)).map(Previous::as_str), Some(*want));
            // The answer names the fate, never the leaving session id.
            let name = arr[i]["name"].as_str().unwrap();
            assert!(name.starts_with(names[i]), "{name:?}");
            assert!(!name.contains(':'), "answer must not repeat a session id: {name:?}");
        }
        assert!(arr[3]["action"].is_null(), "Cancel is a no-op entry");
        assert_eq!(Previous::parse(None), Some(Previous::Keep));
        assert_eq!(Previous::parse(Some("nuke")), None);
    }
}
