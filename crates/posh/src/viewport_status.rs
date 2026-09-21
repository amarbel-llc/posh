//! RFC 0014 §6: the viewport is the daemon for its own history. One
//! `<base>/viewports/<pid>.status.sock` (+ `.status.pid`) per front-door
//! process, answering connect → response → EOF like a session daemon's
//! status socket (§4.1). The response is rebuilt by [`ViewportStatus::refresh`]
//! whenever the stack, the current attach, or an overlay changes (the
//! front door's re-attach loop calls it; the overlay helpers call
//! [`refresh_now`] through the hook the bind registers); the accept thread
//! serves the latest snapshot. `POSH_VIEWPORT_STATUS=0|off|false|no` skips
//! the bind (diagnostic only; nothing depends on it). Best-effort
//! throughout: a bind failure is a warn line, never a failed attach.
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

/// The text the overlay helpers refresh without owning the handle: the
/// bound socket's shared response, registered by [`bind_in`], cleared on
/// drop. One per process — a front door binds once.
static HOOK: Mutex<Option<Arc<Mutex<String>>>> = Mutex::new(None);

/// Rebuild the bound socket's response from the picker state; a no-op when
/// no socket is bound (a `posh list`, a test, the env opt-out).
pub fn refresh_now() {
    let hook = HOOK.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(text) = hook {
        render_into(&text);
    }
}

fn render_into(text: &Mutex<String>) {
    *text.lock().unwrap_or_else(|e| e.into_inner()) = render(&picker::snapshot(), std::process::id());
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
    /// Bind this process's socket under [`dir`], unless the env opts out.
    pub fn bind() -> Option<ViewportStatus> {
        bind_in(&dir(), enabled_by_env())
    }

    /// Rebuild the response from the picker state (after a stack push / pop
    /// or an attach's return).
    pub fn refresh(&self) {
        render_into(&self.text);
    }

    #[cfg(test)]
    fn text(&self) -> String {
        self.text.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// [`ViewportStatus::bind`] against an explicit base and gate, so tests
/// touch neither the env nor the real socket dir. Reaps dead siblings first;
/// mirrors `server.rs::bind_remote_status_socket` (pidfile first, a stale
/// socket removed, 0700 dir) and degrades to `None` with a warn line.
pub(crate) fn bind_in(dir: &Path, enabled: bool) -> Option<ViewportStatus> {
    if !enabled {
        return None;
    }
    reap_dead_in(dir);
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
    *HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&text));
    let status = ViewportStatus { sock, pidfile, text, thread, stop };
    status.refresh();
    Some(status)
}

/// 0700 dir, pidfile first, a stale socket removed, then the nonblocking bind.
fn bind_listener(dir: &Path, pid: u32) -> std::io::Result<UnixListener> {
    let sock = sock_path(dir, pid);
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true).mode(0o700);
    b.create(dir)?;
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
        {
            let mut hook = HOOK.lock().unwrap_or_else(|e| e.into_inner());
            if hook.as_ref().is_some_and(|h| Arc::ptr_eq(h, &self.text)) {
                *hook = None;
            }
        }
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
/// deterministic.
pub(crate) fn render(snap: &Snapshot, pid: u32) -> String {
    let mut out = String::new();
    match &snap.current {
        Some((target, kind, anon)) => out.push_str(&format!(
            "viewport pid={pid} current={target} kind={} anonymous_create={}\n",
            kind.as_str(),
            u8::from(*anon)
        )),
        None => out.push_str(&format!("viewport pid={pid} current=- kind=unknown anonymous_create=0\n")),
    }
    for (i, e) in snap.stack.iter().enumerate() {
        out.push_str(&format!("stack depth={} target={} kind={}\n", i + 1, e.target, e.kind.as_str()));
    }
    for o in &snap.overlays {
        out.push_str(&format!("overlay kind={} over={}\n", o.kind, o.over.as_deref().unwrap_or("-")));
    }
    out
}

/// `posh status --viewport [pid]`: print one viewport's response, or (no
/// pid) reap the dead and list every live viewport pid, one per line — the
/// asking process excluded (the front door binds for every invocation).
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
            for pid in registered_pids(&dir).into_iter().filter(|p| *p != std::process::id()) {
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
            stack: vec![StackEntry { target: "box:dev".into(), kind: SessionKind::Named }],
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
                StackEntry { target: ":s-1".into(), kind: SessionKind::Unknown },
                StackEntry { target: "box:dev".into(), kind: SessionKind::Anonymous },
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
    }

    #[test]
    fn disabled_binds_nothing_and_creates_nothing() {
        let dir = tmp("off");
        assert!(bind_in(&dir, false).is_none());
        assert!(!dir.exists());
    }

    /// The socket answers the rendered text and an overlay change reaches it
    /// through the hook; dropping removes both files and unregisters.
    #[test]
    fn bound_socket_serves_the_rendered_text_and_drop_cleans_up() {
        let _g = picker::switch_test_guard();
        let dir = tmp("bind");
        let pid = std::process::id();
        let s = bind_in(&dir, true)
            .unwrap_or_else(|| panic!("bind under {}: {:?}", dir.display(), bind_listener(&dir, pid).err()));
        let sock = sock_path(&dir, pid);
        let pidfile = pid_path(&dir, pid);
        assert_eq!(std::fs::read_to_string(&pidfile).unwrap(), pid.to_string());
        let served = session::read_status_socket(&sock).unwrap();
        assert_eq!(served, s.text());
        assert!(served.starts_with(&format!("viewport pid={pid} ")), "{served}");
        // The overlay helpers refresh without the handle.
        picker::set_current(":vs-test");
        picker::overlay_open("picker");
        let served = session::read_status_socket(&sock).unwrap();
        assert!(served.contains("overlay kind=picker over=:vs-test\n"), "{served}");
        picker::overlay_close("picker");
        assert!(!session::read_status_socket(&sock).unwrap().contains("over=:vs-test"));
        // An explicit refresh picks up a picker change made without one.
        picker::stack_push_current();
        assert!(!s.text().contains("stack depth=1 target=:vs-test"));
        s.refresh();
        assert!(s.text().contains("stack depth=1 target=:vs-test kind=unknown\n"), "{}", s.text());
        picker::stack_pop();
        drop(s);
        assert!(!sock.exists() && !pidfile.exists());
        assert!(HOOK.lock().unwrap().is_none());
        refresh_now(); // a no-op once unbound
        let _ = std::fs::remove_dir_all(&dir);
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
