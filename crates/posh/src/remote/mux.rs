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
