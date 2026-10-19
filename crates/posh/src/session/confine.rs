//! Session confinement (FDR 0022): which processes may attach to a session.
//!
//! The session socket has no client identity of its own (any process that can
//! reach its 0700 directory is a full client: it can write the PTY and kill the
//! session). With confinement ON the daemon asks the KERNEL who connected
//! (`SO_PEERCRED`, then the peer's cgroup from `/proc`) and refuses a peer that
//! sits in an agent scope — a cgroup the operator named — before it becomes a
//! client at all. A process cannot lie about, or leave, a non-delegated cgroup,
//! which is why this keys on the cgroup rather than a secret an agent running
//! as the same user could read.
//!
//! Default OFF: with `POSH_CONFINE` unset the daemon never calls in here and
//! behaves exactly as before. The refusal is whole-connection (no input, no
//! screen, no kill) and is counted, so the observed side sees it in
//! `posh status`. A connection verdict is a [`Verdict`], not a bool, so a
//! later read-only observer attach (posh#224) can add an `Observe` outcome
//! without reworking the call site.

use std::os::fd::RawFd;

/// `POSH_CONFINE=1` turns the check on for sessions the process creates.
pub const ENV_ON: &str = "POSH_CONFINE";
/// Colon-separated cgroup path prefixes that are agent scope: a peer inside
/// one is refused.
pub const ENV_DENY: &str = "POSH_CONFINE_DENY_CGROUP";
/// Colon-separated cgroup path prefixes that are the ONLY places a peer may
/// come from. Stronger than the deny list (a same-uid agent that can reach the
/// user manager can start a unit anywhere outside the deny prefix); optional.
pub const ENV_ALLOW: &str = "POSH_CONFINE_ALLOW_CGROUP";

/// What the kernel told us about a connecting peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peer {
    /// The peer's cgroup-v2 path (`0::<path>`), e.g. `/user.slice/…/x.service`.
    Cgroup(String),
    /// Pid not obtainable, process already gone, not cgroup v2, or not
    /// Linux: the peer cannot be placed, so it cannot be trusted.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny(String),
}

/// An enabled confinement policy plus the refusals it has made.
#[derive(Debug, Clone, Default)]
pub struct Confine {
    deny: Vec<String>,
    allow: Vec<String>,
    refused: u64,
    last_refused: Option<String>,
}

impl Confine {
    /// `Some` only when `POSH_CONFINE=1`.
    pub fn from_env() -> Option<Confine> {
        let env = |k: &str| std::env::var(k).ok();
        Confine::from_parts(
            env(ENV_ON).as_deref(),
            env(ENV_DENY).as_deref(),
            env(ENV_ALLOW).as_deref(),
        )
    }

    pub fn from_parts(on: Option<&str>, deny: Option<&str>, allow: Option<&str>) -> Option<Confine> {
        if on != Some("1") {
            return None;
        }
        Some(Confine {
            deny: prefixes(deny),
            allow: prefixes(allow),
            ..Confine::default()
        })
    }

    /// Decide on a peer. Pure; the I/O is in [`peer_of`].
    pub fn judge(&self, peer: &Peer) -> Verdict {
        let Peer::Cgroup(cg) = peer else {
            return Verdict::Deny("peer cgroup unknown".into());
        };
        if let Some(p) = self.deny.iter().find(|p| under(cg, p)) {
            return Verdict::Deny(format!("peer in agent scope {p}"));
        }
        if !self.allow.is_empty() && !self.allow.iter().any(|p| under(cg, p)) {
            return Verdict::Deny(format!("peer cgroup {cg} outside the allowed scopes"));
        }
        Verdict::Allow
    }

    /// Judge the peer on `fd` (the accepted socket) and keep the books.
    pub fn admit(&mut self, fd: RawFd) -> Verdict {
        let peer = peer_of(fd);
        let v = self.judge(&peer);
        if let Verdict::Deny(why) = &v {
            self.refused += 1;
            self.last_refused = Some(why.clone());
        }
        v
    }

    /// The optional `confine*=` tail of the RFC 0014 §4.2 session line.
    pub fn status_field(&self) -> String {
        let mut s = format!(
            "confine=on confine_deny={} confine_allow={} confine_refused={}",
            self.deny.len(),
            self.allow.len(),
            self.refused
        );
        if let Some(why) = &self.last_refused {
            s.push_str(&format!(" confine_last={why:?}"));
        }
        s
    }
}

fn prefixes(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or("")
        .split(':')
        .map(|p| p.trim().trim_end_matches('/'))
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Segment-aware prefix: `/a/b` covers `/a/b` and `/a/b/c`, never `/a/bc`.
fn under(cgroup: &str, prefix: &str) -> bool {
    let cg = cgroup.trim_end_matches('/');
    cg == prefix || cg.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))
}

/// Pull the unified-hierarchy path out of a `/proc/<pid>/cgroup` body.
fn parse_cgroup_v2(body: &str) -> Option<String> {
    body.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().to_string())
}

/// Who is on the other end of the accepted unix socket `fd`.
#[cfg(target_os = "linux")]
pub fn peer_of(fd: RawFd) -> Peer {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred`/`len` are valid for the call and `len` is its size.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 || cred.pid <= 0 {
        return Peer::Unknown;
    }
    std::fs::read_to_string(format!("/proc/{}/cgroup", cred.pid))
        .ok()
        .and_then(|b| parse_cgroup_v2(&b))
        .map_or(Peer::Unknown, Peer::Cgroup)
}

// macOS gap (posh#214): no cgroups; LOCAL_PEERPID would give the pid, but
// there is no scope to place it in. Confinement ON therefore refuses every
// peer there, which is the safe answer for an unsupported platform.
#[cfg(not(target_os = "linux"))]
pub fn peer_of(_fd: RawFd) -> Peer {
    Peer::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = "/user.slice/user-1001.slice/user@1001.service/clown.slice/clown-agent.slice";

    fn policy(deny: &str, allow: &str) -> Confine {
        Confine::from_parts(Some("1"), Some(deny), Some(allow)).unwrap()
    }

    #[test]
    fn off_unless_exactly_one() {
        assert!(Confine::from_parts(None, Some(AGENT), None).is_none());
        assert!(Confine::from_parts(Some("0"), Some(AGENT), None).is_none());
        assert!(Confine::from_parts(Some("true"), Some(AGENT), None).is_none());
        assert!(Confine::from_parts(Some("1"), None, None).is_some());
    }

    #[test]
    fn a_peer_inside_the_agent_scope_is_refused() {
        let c = policy(AGENT, "");
        let inside = Peer::Cgroup(format!("{AGENT}/run-1.service"));
        assert!(matches!(c.judge(&inside), Verdict::Deny(_)));
        assert!(matches!(c.judge(&Peer::Cgroup(AGENT.into())), Verdict::Deny(_)));
    }

    #[test]
    fn a_peer_outside_the_scope_is_allowed() {
        let c = policy(AGENT, "");
        let op = Peer::Cgroup("/user.slice/user-1001.slice/session-3.scope".into());
        assert_eq!(c.judge(&op), Verdict::Allow);
    }

    #[test]
    fn prefix_match_is_by_segment() {
        let c = policy("/a/b", "");
        assert_eq!(c.judge(&Peer::Cgroup("/a/bc".into())), Verdict::Allow);
        assert!(matches!(c.judge(&Peer::Cgroup("/a/b/c".into())), Verdict::Deny(_)));
        // A trailing slash on the configured prefix means the same thing.
        let c = policy("/a/b/", "");
        assert!(matches!(c.judge(&Peer::Cgroup("/a/b".into())), Verdict::Deny(_)));
    }

    #[test]
    fn an_unplaceable_peer_is_refused() {
        // Fail closed even with no deny list: a peer that cannot be placed
        // (already exited, foreign pid namespace, no cgroup v2) is not trusted.
        let c = policy("", "");
        assert!(matches!(c.judge(&Peer::Unknown), Verdict::Deny(_)));
    }

    #[test]
    fn the_allow_list_is_exclusive() {
        let c = policy("", "/user.slice/user-1001.slice/session-3.scope");
        assert_eq!(
            c.judge(&Peer::Cgroup("/user.slice/user-1001.slice/session-3.scope".into())),
            Verdict::Allow
        );
        // A unit the agent started via the user manager lands outside the
        // deny prefix; the allow list is what still refuses it.
        assert!(matches!(
            c.judge(&Peer::Cgroup("/user.slice/user-1001.slice/user@1001.service/app.slice/x.service".into())),
            Verdict::Deny(_)
        ));
    }

    #[test]
    fn deny_wins_over_allow() {
        let c = policy("/a/agent", "/a");
        assert!(matches!(c.judge(&Peer::Cgroup("/a/agent/x".into())), Verdict::Deny(_)));
        assert_eq!(c.judge(&Peer::Cgroup("/a/op".into())), Verdict::Allow);
    }

    #[test]
    fn parses_the_unified_line_among_v1_lines() {
        assert_eq!(parse_cgroup_v2("0::/x/y.service\n").as_deref(), Some("/x/y.service"));
        assert_eq!(parse_cgroup_v2("12:cpu:/x\n0::/z\n").as_deref(), Some("/z"));
        assert_eq!(parse_cgroup_v2("12:cpu:/x\n"), None);
    }

    #[test]
    fn refusals_are_counted_and_reported() {
        let mut c = policy(AGENT, "");
        assert_eq!(c.status_field(), "confine=on confine_deny=1 confine_allow=0 confine_refused=0");
        // `admit` on a non-socket fd cannot place the peer: refused, counted.
        let v = c.admit(-1);
        assert!(matches!(v, Verdict::Deny(_)));
        let f = c.status_field();
        assert!(
            f.starts_with("confine=on confine_deny=1 confine_allow=0 confine_refused=1 confine_last=\""),
            "{f}"
        );
    }

    /// The real kernel path: a socketpair peer is this very process, so its
    /// cgroup is ours. Denying our own cgroup must refuse; denying another
    /// must allow. (Skips where /proc/<pid>/cgroup is not cgroup v2.)
    #[cfg(target_os = "linux")]
    #[test]
    fn peer_of_reads_the_real_cgroup() {
        use std::os::fd::AsRawFd;
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let Peer::Cgroup(mine) = peer_of(a.as_raw_fd()) else { return };
        let mut deny_me = policy(&mine, "");
        assert!(matches!(deny_me.admit(a.as_raw_fd()), Verdict::Deny(_)));
        let mut deny_other = policy("/nonexistent-scope", "");
        assert_eq!(deny_other.admit(a.as_raw_fd()), Verdict::Allow);
    }
}
