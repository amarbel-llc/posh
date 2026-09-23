//! RFC 0014 §6: the viewport is the daemon for its own history. One
//! `<base>/viewports/<pid>.status.sock` (+ `.status.pid`) per front-door
//! process, answering connect → response → EOF like a session daemon's
//! status socket (§4.1). Bound on the process's FIRST attach
//! ([`ensure_bound`] from `picker::set_current` — design §2; a listing never
//! binds), held in a static until [`shutdown`] on the way out. The response
//! is rebuilt by [`refresh_now`] whenever the stack, the current attach, or
//! an overlay changes (the front door's re-attach loop and the overlay
//! helpers call it); the accept thread serves the latest snapshot.
//! `POSH_VIEWPORT_STATUS=0|off|false|no` skips the bind (diagnostic only;
//! nothing depends on it). Best-effort throughout: a bind failure is a warn
//! line, never a failed attach.
//!
//! Response grammar (design 2026-09-21 §2):
//!
//! ```text
//! viewport pid=<pid> current=<target|-> kind=<kind> anonymous_create=<0|1>
//! stack depth=<n> target=<target> kind=<kind>        one per entry, bottom first
//! overlay kind=<palette|picker|leave> over=<target|->  one per live overlay
//! ```

use std::io::Write as _;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::picker::{self, Snapshot};
use crate::session::{self, STATUS_SOCK_SUFFIX};
use crate::util::{self, Error, Result};

/// The liveness record beside the socket: written before the bind, its
/// name is the pid [`reap_dead_in`] probes.
const PID_SUFFIX: &str = ".status.pid";

/// How long the accept thread waits for a connection before checking the
/// stop flag — the bound on how long a drop can take.
const ACCEPT_TICK_MS: i32 = 50;

/// `<base>/viewports`: the per-viewport status sockets.
pub fn dir() -> PathBuf {
    session::socket_base_from_env().join("viewports")
}

fn sock_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}{STATUS_SOCK_SUFFIX}"))
}

fn pid_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}{PID_SUFFIX}"))
}

fn enabled_by_env() -> bool {
    match std::env::var("POSH_VIEWPORT_STATUS") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"),
        Err(_) => true,
    }
}

/// The process's one bound socket (design §2: the front door binds on its
/// FIRST attach — `picker::set_current`, which every attach entry point
/// calls and no listing does). `None` until then, after [`shutdown`], under
/// the env opt-out, or when the bind failed.
static HANDLE: Mutex<Option<ViewportStatus>> = Mutex::new(None);

/// Under test the base is an explicit override, else there is NO bind: the
/// many tests that call `set_current` must never touch the real socket dir.
#[cfg(test)]
static TEST_BASE: Mutex<Option<PathBuf>> = Mutex::new(None);

fn bind_base() -> Option<PathBuf> {
    #[cfg(test)]
    {
        TEST_BASE.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    #[cfg(not(test))]
    {
        Some(dir())
    }
}

/// Bind this process's socket if it is not bound yet (idempotent; honors
/// the env gate). Called by `picker::set_current` — the first attach.
pub fn ensure_bound() {
    let mut handle = HANDLE.lock().unwrap_or_else(|e| e.into_inner());
    if handle.is_some() {
        return;
    }
    if let Some(base) = bind_base() {
        *handle = bind_in(&base, enabled_by_env());
    }
}

/// Unbind: stop serving and remove the socket + pidfile. The handle is a
/// static, so nothing drops it at exit — the front door calls this on
/// every way out (before a `process::exit`, and after `run()` returns).
pub fn shutdown() {
    HANDLE.lock().unwrap_or_else(|e| e.into_inner()).take();
}

/// Rebuild the bound socket's response from the picker state; a no-op when
/// no socket is bound (a `posh list`, a test, the env opt-out).
pub fn refresh_now() {
    if let Some(s) = HANDLE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        s.refresh();
    }
}

/// The bound socket: dropping it stops the accept thread and removes the
/// socket and its pidfile.
pub struct ViewportStatus {
    sock: PathBuf,
    pidfile: PathBuf,
    text: Arc<Mutex<String>>,
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl ViewportStatus {
    /// Rebuild the response from the picker state.
    fn refresh(&self) {
        *self.text.lock().unwrap_or_else(|e| e.into_inner()) = render(&picker::snapshot(), std::process::id());
    }
}

#[cfg(test)]
static BINDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The bind against an explicit base and gate, so tests touch neither the
/// env nor the real socket dir. Reaps dead siblings first; mirrors
/// `server.rs::bind_remote_status_socket` (pidfile first, a stale socket
/// removed, 0700 dir) and degrades to `None` with a warn line.
fn bind_in(dir: &Path, enabled: bool) -> Option<ViewportStatus> {
    if !enabled {
        return None;
    }
    let pid = std::process::id();
    let sock = sock_path(dir, pid);
    let pidfile = pid_path(dir, pid);
    let listener = match bind_listener(dir, pid) {
        Ok(l) => l,
        Err(e) => {
            util::log_write("warn", &format!("viewport status socket unavailable {}: {e}", sock.display()));
            let _ = std::fs::remove_file(&pidfile);
            return None;
        }
    };
    let text = Arc::new(Mutex::new(String::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (text, stop) = (Arc::clone(&text), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("viewport-status".into())
            .spawn(move || serve(listener, &text, &stop))
            .ok()
    };
    if thread.is_none() {
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&pidfile);
        return None;
    }
    #[cfg(test)]
    BINDS.fetch_add(1, Ordering::Relaxed);
    let status = ViewportStatus { sock, pidfile, text, thread, stop };
    status.refresh();
    Some(status)
}

/// The dir hardened like every socket dir (`session::Config::new`,
/// `AgentEndpoint::build_named`; github #7): the base must be a real,
/// self-owned directory and the 0700 leaf private and self-owned — a
/// recursive create would silently trust a symlink or a foreign dir an
/// attacker planted under the world-writable `/tmp` fallback. Only then the
/// dead siblings are reaped, the pidfile written, a stale socket removed,
/// and the listener bound nonblocking.
fn bind_listener(dir: &Path, pid: u32) -> Result<UnixListener> {
    let uid = util::uid();
    if let Some(base) = dir.parent() {
        session::validate_session_dir(base, uid, false)?;
    }
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true).mode(0o700);
    b.create(dir)?;
    session::validate_session_dir(dir, uid, true)?;
    reap_dead_in(dir);
    let sock = sock_path(dir, pid);
    std::fs::write(pid_path(dir, pid), pid.to_string())?;
    let _ = std::fs::remove_file(&sock);
    let l = UnixListener::bind(&sock)?;
    l.set_nonblocking(true)?;
    Ok(l)
}

/// The accept loop: wait up to one tick for a connection, answer it with the
/// current text, hang up; until told to stop.
fn serve(listener: UnixListener, text: &Mutex<String>, stop: &AtomicBool) {
    use std::os::unix::io::AsRawFd;
    while !stop.load(Ordering::Relaxed) {
        let mut fds = [util::pollfd(listener.as_raw_fd(), libc::POLLIN)];
        // SAFETY: fds is a valid, initialized pollfd array for the call.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, ACCEPT_TICK_MS) };
        if rc <= 0 {
            continue;
        }
        if let Ok((mut s, _)) = listener.accept() {
            let answer = text.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
            let _ = s.write_all(answer.as_bytes());
        }
    }
}

impl Drop for ViewportStatus {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The thread wakes every tick; the join is bounded by that.
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.sock);
        let _ = std::fs::remove_file(&self.pidfile);
    }
}

/// Unlink every `<pid>.status.{pid,sock}` pair under `dir` whose pid is
/// gone (the bind and `posh status --viewport` both do this first). A live
/// pid's files are never touched.
pub(crate) fn reap_dead_in(dir: &Path) {
    for pid in registered_pids(dir) {
        if !crate::remote::agent::pid_alive(pid as i32) {
            let _ = std::fs::remove_file(pid_path(dir, pid));
            let _ = std::fs::remove_file(sock_path(dir, pid));
        }
    }
}

/// Every pid with a `.status.pid` record under `dir`, ascending.
fn registered_pids(dir: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            name.to_str()?.strip_suffix(PID_SUFFIX)?.parse().ok()
        })
        .collect();
    pids.sort_unstable();
    pids
}

/// The §6 response, pure: `pid` is a parameter so the golden tests are
/// deterministic. Targets are flattened ([`flat`]): a session name may
/// legally hold a space or a line break (`util::encode_session_name`
/// escapes only `/ \ %` and NUL), and only a line break would break the
/// one-record-per-line grammar — a space inside a value is fine, fields
/// being `key=value` read by key prefix.
pub(crate) fn render(snap: &Snapshot, pid: u32) -> String {
    let mut out = String::new();
    match &snap.current {
        Some((target, kind, anon)) => out.push_str(&format!(
            "viewport pid={pid} current={} kind={} anonymous_create={}\n",
            flat(target),
            kind.as_str(),
            u8::from(*anon)
        )),
        None => out.push_str(&format!("viewport pid={pid} current=- kind=unknown anonymous_create=0\n")),
    }
    for (i, e) in snap.stack.iter().enumerate() {
        out.push_str(&format!("stack depth={} target={} kind={}\n", i + 1, flat(&e.target), e.kind.as_str()));
    }
    for o in &snap.overlays {
        out.push_str(&format!("overlay kind={} over={}\n", o.kind, o.over.as_deref().map_or("-".into(), flat)));
    }
    out
}

/// A target as one line: tab / newline / carriage return become a space
/// (the `completion_summary` flattening).
fn flat(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// `posh status --viewport [pid]`: print one viewport's response, or (no
/// pid) reap the dead and list every live viewport pid, one per line (a
/// listing process never binds, so it never lists itself).
pub fn cmd_status(pid: Option<u32>) -> Result<()> {
    let dir = dir();
    match pid {
        Some(pid) => {
            let text = session::read_status_socket(&sock_path(&dir, pid)).map_err(|e| {
                Error::Msg(format!("viewport {pid}: status socket {e} — no such viewport, or it predates RFC 0014 §6"))
            })?;
            print!("{text}");
        }
        None => {
            reap_dead_in(&dir);
            for pid in registered_pids(&dir) {
                println!("{pid}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::{Overlay, StackEntry};
    use posh_proto::caps::SessionKind;

    /// A literal `/tmp` base, like the other socket tests: a devshell
    /// `$TMPDIR` can push a socket path past `SUN_LEN`.
    fn tmp(tag: &str) -> PathBuf {
        let d = PathBuf::from(format!("/tmp/posh-vs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn render_golden() {
        let empty = Snapshot { current: None, stack: vec![], overlays: vec![] };
        assert_eq!(render(&empty, 42), "viewport pid=42 current=- kind=unknown anonymous_create=0\n");

        let one = Snapshot {
            current: Some((":ff9fe216-9652-4e23-805c-6f4dd5ce7eca".into(), SessionKind::Anonymous, true)),
            stack: vec![StackEntry { target: "box:dev".into(), kind: SessionKind::Named, activity: String::new() }],
            overlays: vec![],
        };
        assert_eq!(
            render(&one, 7),
            "viewport pid=7 current=:ff9fe216-9652-4e23-805c-6f4dd5ce7eca kind=anonymous anonymous_create=1\n\
             stack depth=1 target=box:dev kind=named\n"
        );

        let two = Snapshot {
            current: Some(("box:work/s-3".into(), SessionKind::Named, false)),
            stack: vec![
                StackEntry { target: ":s-1".into(), kind: SessionKind::Unknown, activity: String::new() },
                StackEntry { target: "box:dev".into(), kind: SessionKind::Anonymous, activity: String::new() },
            ],
            overlays: vec![
                Overlay { kind: "palette", over: Some("box:work/s-3".into()) },
                Overlay { kind: "picker", over: None },
            ],
        };
        assert_eq!(
            render(&two, 9),
            "viewport pid=9 current=box:work/s-3 kind=named anonymous_create=0\n\
             stack depth=1 target=:s-1 kind=unknown\n\
             stack depth=2 target=box:dev kind=anonymous\n\
             overlay kind=palette over=box:work/s-3\n\
             overlay kind=picker over=-\n"
        );

        // A line break inside a session name never breaks a record; a space
        // is kept as is.
        let odd = Snapshot {
            current: Some((":two\nlines".into(), SessionKind::Named, false)),
            stack: vec![StackEntry { target: "box:a b\r\nc\td".into(), kind: SessionKind::Named, activity: String::new() }],
            overlays: vec![Overlay { kind: "palette", over: Some(":two\nlines".into()) }],
        };
        assert_eq!(
            render(&odd, 3),
            "viewport pid=3 current=:two lines kind=named anonymous_create=0\n\
             stack depth=1 target=box:a b  c d kind=named\n\
             overlay kind=palette over=:two lines\n"
        );
    }

    /// github #7: a symlink planted at the leaf path is refused, so the
    /// bind degrades to None and writes nothing through it.
    #[test]
    fn a_symlinked_viewports_dir_is_refused() {
        let base = tmp("symlink");
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let leaf = base.join("viewports");
        std::os::unix::fs::symlink(&elsewhere, &leaf).unwrap();
        assert!(bind_in(&leaf, true).is_none());
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none(), "nothing written through the link");
        assert!(session::validate_session_dir(&leaf, util::uid(), true).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn disabled_binds_nothing_and_creates_nothing() {
        let dir = tmp("off").join("viewports");
        assert!(bind_in(&dir, false).is_none());
        assert!(!dir.exists());
    }

    /// The first attach (`set_current`) binds once under the test base; a
    /// second attach does not rebind. The socket answers the rendered text,
    /// an overlay change reaches it on its own, an explicit `refresh_now`
    /// picks up a stack change, and `shutdown` removes both files. Other
    /// tests may call `entered` concurrently: with the base set they hit
    /// the idempotent path, with it cleared they bind nothing.
    #[test]
    fn first_attach_binds_once_and_shutdown_cleans_up() {
        let _g = picker::switch_test_guard();
        picker::reset_for_test();
        let base = tmp("bind");
        let dir = base.join("viewports");
        let pid = std::process::id();
        shutdown();
        *TEST_BASE.lock().unwrap() = Some(dir.clone());
        let binds = BINDS.load(Ordering::Relaxed);
        picker::entered(":vs-test");
        assert!(
            HANDLE.lock().unwrap().is_some(),
            "bind under {}: {:?}",
            dir.display(),
            bind_listener(&dir, pid).err()
        );
        assert_eq!(BINDS.load(Ordering::Relaxed), binds + 1);
        let sock = sock_path(&dir, pid);
        let pidfile = pid_path(&dir, pid);
        assert_eq!(std::fs::read_to_string(&pidfile).unwrap(), pid.to_string());
        let served = session::read_status_socket(&sock).unwrap();
        assert!(
            served.starts_with(&format!("viewport pid={pid} current=:vs-test kind=unknown anonymous_create=0\n")),
            "{served}"
        );
        // A second attach (an in-place re-home) does not rebind but the
        // `current=` line follows it; the kind arriving on a frame updates
        // `kind=` without the front door's loop. Entering also PUSHES what
        // it left, so the stack grows as the test walks.
        picker::entered(":vs-rehomed");
        assert_eq!(BINDS.load(Ordering::Relaxed), binds + 1, "a second attach does not rebind");
        let served = session::read_status_socket(&sock).unwrap();
        assert!(served.contains(" current=:vs-rehomed kind=unknown "), "{served}");
        assert!(served.contains("stack depth=1 target=:vs-test kind=unknown\n"), "{served}");
        picker::set_current_kind(SessionKind::Named);
        let served = session::read_status_socket(&sock).unwrap();
        assert!(served.contains(" current=:vs-rehomed kind=named "), "{served}");
        picker::entered(":vs-test");
        assert!(session::read_status_socket(&sock).unwrap().contains(" current=:vs-test kind=unknown "));
        // The overlay helpers refresh on their own. The `leave` kind is the
        // one no concurrent renderer test opens (they record palette / picker
        // over whatever `current` is), so its line is this test's alone.
        picker::overlay_open("leave");
        let served = session::read_status_socket(&sock).unwrap();
        assert!(served.contains("overlay kind=leave over=:vs-test\n"), "{served}");
        picker::overlay_close("leave");
        assert!(!session::read_status_socket(&sock).unwrap().contains("kind=leave"));
        // The stack the walk above built is reported bottom first.
        refresh_now();
        let served = session::read_status_socket(&sock).unwrap();
        assert!(served.contains("stack depth=1 target=:vs-test kind=unknown\n"), "{served}");
        assert!(served.contains("stack depth=2 target=:vs-rehomed kind=named\n"), "{served}");
        picker::reset_for_test();
        *TEST_BASE.lock().unwrap() = None;
        shutdown();
        assert!(!sock.exists() && !pidfile.exists());
        assert!(HANDLE.lock().unwrap().is_none());
        refresh_now(); // a no-op once unbound
        picker::entered(":vs-after");
        assert!(HANDLE.lock().unwrap().is_none(), "no base under test: no bind");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Reaping unlinks a pair whose pid is gone and keeps a live pid's.
    #[test]
    fn reap_removes_dead_pairs_and_keeps_live_ones() {
        let dir = tmp("reap");
        std::fs::create_dir_all(&dir).unwrap();
        let dead = std::process::Command::new("true").spawn().and_then(|mut c| c.wait().map(|_| c.id()));
        let Ok(dead) = dead else { return }; // no `true` on PATH: nothing to test
        let live = std::process::id();
        for pid in [dead, live] {
            std::fs::write(pid_path(&dir, pid), pid.to_string()).unwrap();
            std::fs::write(sock_path(&dir, pid), "").unwrap();
        }
        assert_eq!(registered_pids(&dir), { let mut v = vec![dead, live]; v.sort_unstable(); v });
        reap_dead_in(&dir);
        assert!(!pid_path(&dir, dead).exists() && !sock_path(&dir, dead).exists());
        assert!(pid_path(&dir, live).exists() && sock_path(&dir, live).exists());
        assert_eq!(registered_pids(&dir), vec![live]);
        // A missing dir reaps nothing and lists nothing.
        let _ = std::fs::remove_dir_all(&dir);
        reap_dead_in(&dir);
        assert!(registered_pids(&dir).is_empty());
    }
}
