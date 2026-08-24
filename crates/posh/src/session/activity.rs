//! The session **activity label** (RFC 0013 §5): a short human-readable string
//! identifying what a session is running, so a session can be selected by what
//! it is *doing* rather than by its (possibly auto-generated) name — the label
//! in `posh list`, the picker (FDR 0011), and `ph` completion (FDR 0015).
//!
//! The label combines two fields the session daemon can observe: the pty's
//! foreground-process command ([`crate::pty::foreground_command`]) and the
//! terminal title the shell/app set via OSC 0/2 ([`posh_term::Terminal::title`]).
//! Per RFC 0013 §5 the rendered label is `title · process` when a title is set,
//! else the process alone; both empty renders as an empty string (the caller
//! shows `unknown`).

const SEP: &str = " · ";

/// Compose the activity label from the foreground-process command and the
/// terminal title. See the module docs for the rule.
pub(crate) fn compose(process: Option<&str>, title: &str) -> String {
    let title = title.trim();
    let process = process.map(str::trim).filter(|p| !p.is_empty());
    match (title.is_empty(), process) {
        (false, Some(p)) => format!("{title}{SEP}{p}"),
        (false, None) => title.to_string(),
        (true, Some(p)) => p.to_string(),
        (true, None) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::compose;

    #[test]
    fn title_and_process_combined() {
        assert_eq!(compose(Some("vim"), "~/notes"), "~/notes · vim");
    }

    #[test]
    fn process_only_when_no_title() {
        assert_eq!(compose(Some("bash"), ""), "bash");
        assert_eq!(compose(Some("bash"), "   "), "bash");
    }

    #[test]
    fn title_only_when_no_process() {
        assert_eq!(compose(None, "deploy headscale"), "deploy headscale");
        assert_eq!(compose(Some("  "), "deploy"), "deploy");
    }

    #[test]
    fn empty_when_neither() {
        assert_eq!(compose(None, ""), "");
        assert_eq!(compose(Some("  "), "  "), "");
    }
}
