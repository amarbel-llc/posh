//! Shared escape-to-shell overlay (FDR 0008): a transient second PTY running
//! the configured escape command in the session's cwd, with its own terminal
//! model. Used by BOTH the roaming remote server (`remote::server`) and the
//! session daemon (`session::daemon`): while an overlay is present it is the
//! broadcast source and the input sink; the live session keeps running
//! underneath (its model still advances, just unbroadcast) and is repainted
//! when the overlay's shell exits. Extracted from `remote::server` so the daemon
//! reuses it verbatim (FDR 0011 Phase 2.4b) rather than copy-pasting.

use posh_term::Terminal;

use crate::pty;
use crate::util;

/// A transient escape-to-shell overlay: the overlay PTY child plus its own
/// terminal model. While it is `Some` on a session/server loop, it owns the
/// broadcast source and input sink.
pub(crate) struct Overlay {
    pub(crate) child: pty::PtyChild,
    pub(crate) term: Terminal,
}

/// `$POSH_ESCAPE_CMD` parsed into argv (whitespace-split; `sc exec` and most
/// commands need nothing fancier). `None` (unset/blank) means spawn `$SHELL` as
/// a login shell — the same default as the session shell.
///
/// Unit tests get a plain `sh` instead: a test must not run whatever the
/// developer configured. posh#203 was exactly that — an ambient
/// `POSH_ESCAPE_CMD='sc exec'` made the overlay a wrapper whose inner shell,
/// under load, missed the test's `exit` and never ended.
pub(crate) fn escape_command() -> Option<Vec<String>> {
    #[cfg(test)]
    return Some(vec!["sh".to_string()]);
    #[cfg(not(test))]
    parse_escape_command(std::env::var("POSH_ESCAPE_CMD").ok().as_deref())
}

fn parse_escape_command(raw: Option<&str>) -> Option<Vec<String>> {
    raw.filter(|s| !s.trim().is_empty())
        .map(|s| s.split_whitespace().map(str::to_string).collect())
}

/// Tear down an active escape overlay: hang up its shell's process group, reap
/// it, and close the master fd. No-op when there is no overlay.
pub(crate) fn close_overlay(overlay: &mut Option<Overlay>) {
    if let Some(o) = overlay.take() {
        util::kill_pgroup(o.child.pid, libc::SIGHUP);
        let _ = util::try_reap(o.child.pid);
        util::close_fd(o.child.master);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_command_is_whitespace_split_and_blank_means_the_login_shell() {
        let argv = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(parse_escape_command(Some("sc exec")), argv(&["sc", "exec"]));
        assert_eq!(parse_escape_command(Some("  nvim   -R  ")), argv(&["nvim", "-R"]));
        assert_eq!(parse_escape_command(Some("   ")), None);
        assert_eq!(parse_escape_command(None), None);
    }

    /// posh#203: tests never run the developer's configured command.
    #[test]
    fn unit_tests_ignore_the_ambient_escape_command() {
        assert_eq!(escape_command(), Some(vec!["sh".to_string()]));
    }
}
