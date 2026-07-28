//! The per-destination local mux endpoint (M1, agent-only): destination
//! keys, the hardened `<base>/mux/` socket directory, and the client id the
//! remote side names in the FDR 0014 election.
//!
//! Design: docs/plans/2026-07-28-connection-mux-endpoint-design.md ("Keying
//! and placement", "Remote side"). This file currently carries only Task 1 of
//! docs/plans/2026-07-28-mux-endpoint-m1-impl.md — the pure helpers; the
//! daemon + IPC (Task 3) and client integration (Task 4) consume them.

use std::path::{Path, PathBuf};

use crate::remote::datagram::Family;
use crate::util::{self, Result};

/// Canonicalized, filesystem-safe destination key: `user@host` + address
/// family + port range (#54), rendered as a slug safe to embed in
/// `mux/<key>.sock`. The host is case-folded; an explicit user is prefixed
/// `user@`-style with the joiner rendered slug-safe; the family suffix
/// appears only for an explicit `-4`/`-6`, the port-range suffix only when a
/// non-default range was given — so the common invocation stays a bare
/// hostname slug.
#[allow(dead_code)] // consumed by the mux daemon + client integration (M1 Tasks 3-4, docs/plans/2026-07-28-mux-endpoint-m1-impl.md)
pub fn dest_key(user: Option<&str>, host: &str, family: Family, port_range: Option<&str>) -> String {
    let mut key = String::new();
    if let Some(user) = user {
        // `user@host` with the `@` joiner rendered slug-safe (its mapped
        // byte). The default (no user) carries no marker, so the common key
        // stays the bare host slug.
        key.push_str(&sanitize_id(user));
        key.push('-');
    }
    key.push_str(&sanitize_id(&host.to_lowercase()));
    match family {
        Family::Auto => {}
        Family::Inet => key.push_str("-4"),
        Family::Inet6 => key.push_str("-6"),
    }
    if let Some(range) = port_range {
        key.push('-');
        key.push_str(&sanitize_id(range));
    }
    key
}

/// `<base>/mux/` under the same base-dir resolution as session sockets
/// (`POSH_DIR > XDG_RUNTIME_DIR/posh > TMPDIR/posh-{uid} > /tmp/posh-{uid}`),
/// created 0700 and hardened with the shared #7 check (self-owned,
/// symlink-rejecting) exactly like `<base>/agent/`.
#[allow(dead_code)] // consumed by the mux daemon (M1 Task 3, docs/plans/2026-07-28-mux-endpoint-m1-impl.md)
pub fn mux_dir() -> Result<PathBuf> {
    let env = |k: &str| std::env::var(k).ok();
    let base = crate::session::resolve_socket_base(
        env("POSH_DIR").as_deref(),
        env("XDG_RUNTIME_DIR").as_deref(),
        env("TMPDIR").as_deref(),
        util::uid(),
    );
    mux_dir_at(&base)
}

/// `<base>/mux/<key>.sock` — the endpoint socket for a destination key.
#[allow(dead_code)] // consumed by the mux daemon + client integration (M1 Tasks 3-4, docs/plans/2026-07-28-mux-endpoint-m1-impl.md)
pub fn mux_socket_path(key: &str) -> Result<PathBuf> {
    Ok(mux_socket_path_in(&mux_dir()?, key))
}

/// The join behind [`mux_socket_path`], pure so tests pin the path shape
/// without touching the env-resolved base.
fn mux_socket_path_in(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.sock"))
}

/// The client id the remote `posh-server agent` binds
/// `agent/mux-<client-id>.sock` under and the FDR 0014 election names: the
/// sanitized local hostname, overridable via `POSH_CLIENT_ID` for the
/// shared-hostname pathological case (design doc "Remote side"). The override
/// is sanitized too — the id lands in remote socket names either way.
#[allow(dead_code)] // consumed by the mux daemon + posh-server agent (M1 Tasks 2-3, docs/plans/2026-07-28-mux-endpoint-m1-impl.md)
pub fn client_id() -> String {
    match std::env::var("POSH_CLIENT_ID") {
        Ok(id) if !id.is_empty() => sanitize_id(&id),
        _ => sanitize_id(&hostname()),
    }
}

/// Maps every byte outside `[A-Za-z0-9._-]` to `-`. The one sanitizer behind
/// both [`dest_key`] and [`client_id`], pure so it is testable without env
/// mutation.
fn sanitize_id(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => b as char,
            _ => '-',
        })
        .collect()
}

/// [`mux_dir`] under an explicit base (the seam tests use with a tempdir):
/// validates the base like `agent/` does, creates `mux/` 0700, validates the
/// leaf private + self-owned. Reuses `session::validate_session_dir` — the
/// same hardening helper `AgentEndpoint::build` uses; no duplicate checks.
fn mux_dir_at(base: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let uid = util::uid();
    // The base itself must be a real, self-owned dir (no symlink redirect);
    // it may be group-readable like any /tmp intermediate. github #7.
    crate::session::validate_session_dir(base, uid, false)?;
    let dir = base.join("mux");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    // The leaf that holds the mux sockets must be private + self-owned —
    // reject an attacker-planted dir or a symlink. github #7.
    crate::session::validate_session_dir(&dir, uid, true)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// The refcount/linger state machine (M1 Task 3 of
// docs/plans/2026-07-28-mux-endpoint-m1-impl.md). Pure and virtual-time
// (`now: u64` ms), so ref/unref/linger transitions are unit-tested without a
// daemon, a socket, or a clock.

/// Default linger after the last session unref (design doc "Lifecycle",
/// decided 2026-07-28): 60 s, ControlMaster-ish, conservative until rekey
/// (posh#145) lands.
pub const DEFAULT_LINGER_MS: u64 = 60_000;

/// `POSH_MUX_PERSIST` in SECONDS (the `POSH_SERVER_*_TMOUT` / ssh
/// ControlPersist convention), converted to the internal ms clock. `0`
/// disables lingering; unset/unparsable falls back to the 60 s default.
#[allow(dead_code)] // consumed by run_daemon (M1 Task 3) + client integration (Task 4)
pub fn linger_ms_from_env() -> u64 {
    parse_linger_ms(std::env::var("POSH_MUX_PERSIST").ok().as_deref())
}

/// The pure predicate behind [`linger_ms_from_env`], testable without env
/// mutation.
fn parse_linger_ms(value: Option<&str>) -> u64 {
    match value.map(str::trim).and_then(|v| v.parse::<u64>().ok()) {
        Some(secs) => secs.saturating_mul(1000),
        None => DEFAULT_LINGER_MS,
    }
}

/// The FDR 0014 M1 policy machine: the count of live local session
/// invocations holding a `MuxSessionRef` gates agent serviceability
/// (`refs > 0`), and unref-to-zero arms the linger clock — the endpoint keeps
/// the connection (agent service OFF) for `linger_ms`, then
/// [`should_exit`](Self::should_exit) signals shutdown. Construction arms the
/// same clock, so a daemon whose spawner dies before its first ref exits
/// instead of idling forever; the normal first ref cancels it.
pub struct MuxState {
    refs: usize,
    linger_ms: u64,
    /// `Some(deadline)` exactly while `refs == 0` (the linger window).
    linger_deadline: Option<u64>,
}

#[allow(dead_code)] // consumed by the mux daemon loop (M1 Task 3, docs/plans/2026-07-28-mux-endpoint-m1-impl.md)
impl MuxState {
    pub fn new(linger_ms: u64, now: u64) -> MuxState {
        MuxState {
            refs: 0,
            linger_ms,
            linger_deadline: Some(now.saturating_add(linger_ms)),
        }
    }

    /// A `MuxSessionRef` landed: agent service on, linger cancelled.
    pub fn add_ref(&mut self) {
        self.refs += 1;
        self.linger_deadline = None;
    }

    /// A ref dropped (explicitly or by its IPC connection closing). Reaching
    /// zero turns agent service off and starts the linger window from `now`.
    pub fn unref(&mut self, now: u64) {
        self.refs = self.refs.saturating_sub(1);
        if self.refs == 0 {
            self.linger_deadline = Some(now.saturating_add(self.linger_ms));
        }
    }

    pub fn refs(&self) -> usize {
        self.refs
    }

    /// The FDR 0014 M1 gate: agent channels are serviced iff a session ref is
    /// held. Enforced client-side — the side whose agent is exposed.
    pub fn serviceable(&self) -> bool {
        self.refs > 0
    }

    /// Whether the linger clock is armed (refs == 0, window not yet checked).
    pub fn lingering(&self) -> bool {
        self.linger_deadline.is_some()
    }

    /// The shutdown signal: unreferenced and the linger window has elapsed.
    /// With `linger_ms == 0` this is true the moment the last ref drops.
    pub fn should_exit(&self, now: u64) -> bool {
        self.linger_deadline.is_some_and(|d| now >= d)
    }

    /// The next wall-clock moment the daemon loop must wake for (the linger
    /// expiry), for folding into its poll deadline. `None` while referenced.
    pub fn next_deadline(&self) -> Option<u64> {
        self.linger_deadline
    }
}

/// The local hostname via gethostname(2); `"unknown"` when the call fails or
/// reports an empty name, so [`client_id`] never yields an empty id.
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into a valid,
    // exclusively held buffer; no pointers escape the call.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc != 0 {
        return "unknown".to_string();
    }
    // POSIX leaves NUL-termination unspecified on truncation; force one so
    // the scan below is bounded either way.
    buf[buf.len() - 1] = 0;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
    let name = String::from_utf8_lossy(&buf[..end]);
    if name.is_empty() {
        "unknown".to_string()
    } else {
        name.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- MuxState: the refcount/linger machine (M1 Task 3.1), virtual time ---

    #[test]
    fn refs_gate_serviceability_exactly() {
        let mut st = MuxState::new(60_000, 0);
        assert!(!st.serviceable(), "no refs at spawn: agent service off");
        st.add_ref();
        assert!(st.serviceable(), "first ref turns agent service on");
        st.add_ref();
        assert!(st.serviceable());
        st.unref(10);
        assert!(st.serviceable(), "one of two refs dropped: still serviceable");
        st.unref(20);
        assert!(!st.serviceable(), "last ref dropped: service off at once");
        st.add_ref();
        assert!(st.serviceable(), "re-ref re-enables");
    }

    #[test]
    fn unref_to_zero_starts_linger_and_expiry_signals_shutdown() {
        let mut st = MuxState::new(60_000, 0);
        st.add_ref();
        assert!(!st.should_exit(u64::MAX), "a held ref never expires");
        assert_eq!(st.next_deadline(), None, "no linger clock while referenced");
        st.unref(1_000);
        assert_eq!(st.next_deadline(), Some(61_000));
        assert!(!st.should_exit(60_999), "inside the linger window");
        assert!(st.should_exit(61_000), "linger expiry is the shutdown signal");
    }

    #[test]
    fn re_ref_during_linger_cancels_it() {
        let mut st = MuxState::new(60_000, 0);
        st.add_ref();
        st.unref(1_000);
        assert!(st.lingering());
        st.add_ref();
        assert!(!st.lingering(), "a fresh ref cancels the linger clock");
        assert!(!st.should_exit(u64::MAX));
        // The next unref restarts the window from ITS moment, not the old one.
        st.unref(500_000);
        assert!(!st.should_exit(559_999));
        assert!(st.should_exit(560_000));
    }

    #[test]
    fn zero_linger_exits_immediately_on_unref() {
        let mut st = MuxState::new(0, 7);
        assert!(st.should_exit(7), "unreferenced at spawn with no linger");
        st.add_ref();
        assert!(!st.should_exit(u64::MAX));
        st.unref(42);
        assert!(st.should_exit(42), "POSH_MUX_PERSIST=0: no linger window at all");
    }

    #[test]
    fn spawn_starts_an_orphan_linger_clock() {
        // A daemon whose spawner dies before ever connecting must not idle
        // forever: the linger clock is armed from construction, and the first
        // ref (the spawner's, in the normal path) cancels it.
        let st = MuxState::new(60_000, 100);
        assert!(!st.should_exit(60_099));
        assert!(st.should_exit(60_100));
    }

    #[test]
    fn ref_unref_cycles_track_refs() {
        let mut st = MuxState::new(60_000, 0);
        for round in 0..3u64 {
            st.add_ref();
            st.add_ref();
            st.unref(round);
            st.unref(round);
            assert!(!st.serviceable());
            assert!(st.lingering(), "round {round}: linger armed after each cycle");
        }
        // Unref below zero saturates rather than wrapping.
        st.unref(99);
        assert_eq!(st.refs(), 0);
    }

    #[test]
    fn linger_env_parses_seconds_with_default() {
        // POSH_MUX_PERSIST is seconds (the POSH_SERVER_*_TMOUT / ssh
        // ControlPersist convention); internal time is ms. Tested via the pure
        // predicate, never the process environment.
        assert_eq!(parse_linger_ms(None), DEFAULT_LINGER_MS);
        assert_eq!(parse_linger_ms(Some("")), DEFAULT_LINGER_MS);
        assert_eq!(parse_linger_ms(Some("junk")), DEFAULT_LINGER_MS);
        assert_eq!(parse_linger_ms(Some("-3")), DEFAULT_LINGER_MS);
        assert_eq!(parse_linger_ms(Some("0")), 0);
        assert_eq!(parse_linger_ms(Some("5")), 5_000);
        assert_eq!(parse_linger_ms(Some(" 120 ")), 120_000);
    }

    /// A private 0700 base dir with a SHORT path (the `agent.rs` `temp_base`
    /// pattern): the scratch `$TMPDIR` is too deep for sun_path, so anchor at
    /// `/tmp` like the production `/tmp/posh-<uid>` fallback.
    fn temp_base() -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = PathBuf::from(format!("/tmp/posh-mux-{}-{}", std::process::id(), n));
        std::fs::remove_dir_all(&base).ok();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)
            .unwrap();
        base
    }

    #[test]
    fn dest_key_is_stable_and_case_folds_host() {
        assert_eq!(
            dest_key(None, "Example.COM", Family::Auto, None),
            dest_key(None, "example.com", Family::Auto, None),
        );
        // Same inputs twice ⇒ byte-identical key.
        assert_eq!(
            dest_key(Some("me"), "example.com", Family::Inet, Some("60000:61000")),
            dest_key(Some("me"), "example.com", Family::Inet, Some("60000:61000")),
        );
    }

    #[test]
    fn dest_key_distinguishes_user_from_default() {
        assert_ne!(
            dest_key(Some("root"), "example.com", Family::Auto, None),
            dest_key(None, "example.com", Family::Auto, None),
        );
    }

    #[test]
    fn dest_key_distinguishes_family() {
        let auto = dest_key(None, "example.com", Family::Auto, None);
        let v4 = dest_key(None, "example.com", Family::Inet, None);
        let v6 = dest_key(None, "example.com", Family::Inet6, None);
        assert_ne!(auto, v4);
        assert_ne!(auto, v6);
        assert_ne!(v4, v6);
    }

    #[test]
    fn dest_key_distinguishes_port_ranges() {
        let none = dest_key(None, "example.com", Family::Auto, None);
        let a = dest_key(None, "example.com", Family::Auto, Some("60000:61000"));
        let b = dest_key(None, "example.com", Family::Auto, Some("62000:62099"));
        assert_ne!(none, a);
        assert_ne!(a, b);
    }

    #[test]
    fn hostile_host_yields_safe_slug_without_traversal() {
        let key = dest_key(Some("ev il"), "../..//etc passwd\x01ü", Family::Auto, None);
        assert!(
            key.bytes().all(
                |b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-'
            ),
            "unsafe byte survived: {key:?}"
        );
        assert!(!key.contains('/'), "path separator survived: {key:?}");
        // The key is a single path component: joining it never escapes mux/.
        let joined = Path::new("/base/mux").join(format!("{key}.sock"));
        assert!(joined.starts_with("/base/mux"));
    }

    #[test]
    fn sanitize_id_keeps_safe_bytes_and_maps_the_rest() {
        assert_eq!(sanitize_id("host-1.example_X"), "host-1.example_X");
        assert_eq!(sanitize_id("a b/c:d@e"), "a-b-c-d-e");
        // Multi-byte UTF-8: every byte outside the safe set maps to '-'.
        assert_eq!(sanitize_id("ü"), "--");
        assert_eq!(sanitize_id(""), "");
    }

    #[test]
    fn mux_dir_at_creates_private_hardened_dir() {
        use std::os::unix::fs::MetadataExt;
        let base = temp_base();
        let dir = mux_dir_at(&base).unwrap();
        assert_eq!(dir, base.join("mux"));
        let mode = std::fs::metadata(&dir).unwrap().mode();
        assert_eq!(mode & 0o777, 0o700, "mux/ must be private, got {mode:o}");
        // Idempotent: a second call on the existing dir succeeds.
        assert_eq!(mux_dir_at(&base).unwrap(), dir);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn mux_dir_at_rejects_symlinked_mux_dir() {
        // A pre-planted symlink at <base>/mux must be refused by the shared
        // #7 hardening rather than followed — same contract as agent/.
        let base = temp_base();
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, base.join("mux")).unwrap();
        assert!(mux_dir_at(&base).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn mux_socket_path_lands_under_mux_dir_with_sock_suffix() {
        let base = temp_base();
        let dir = mux_dir_at(&base).unwrap();
        // The pub fn resolves the base from env; the join it performs is the
        // pure seam pinned here.
        let path = mux_socket_path_in(&dir, "example.com-4");
        assert_eq!(path, base.join("mux").join("example.com-4.sock"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn hostname_is_nonempty_and_client_id_shape_is_safe() {
        let h = hostname();
        assert!(!h.is_empty());
        // client_id() is hostname (or override) through sanitize_id; the
        // sanitized form of whatever it returns must be itself.
        let id = client_id();
        assert!(!id.is_empty());
        assert_eq!(sanitize_id(&id), id, "client_id must already be sanitized");
    }
}
