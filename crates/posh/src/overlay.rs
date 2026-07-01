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
pub(crate) fn escape_command() -> Option<Vec<String>> {
    std::env::var("POSH_ESCAPE_CMD")
        .ok()
        .filter(|s| !s.trim().is_empty())
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
