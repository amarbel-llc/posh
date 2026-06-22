//! On-demand transport-state dump (SIGUSR2).
//!
//! When the roaming server or client receives `SIGUSR2`, its event loop calls
//! into here on the next iteration to append a one-line snapshot of its live
//! transport state to the diagnostic sink. This is the introspection hook for a
//! *running* wedged session: unlike the `POSH_DEBUG_LOG` periodic stats (which
//! must be armed at startup), the dump works on a process that is already up,
//! and carries the peer-liveness fields — peer address, last-heard/last-send
//! ages, acked-vs-current frame — that distinguish a roam the server has not
//! re-pinned from asymmetric packet loss or a dead client.
//!
//! The dump runs in normal loop context (after `util::take_flag`), NOT in the
//! signal handler, so the formatting and file I/O here are unrestricted.

use std::net::SocketAddr;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

use crate::session::resolve_socket_base;
use crate::util;

/// Server-side transport snapshot, copied out of `server_loop`'s locals when the
/// SIGUSR2 flag is taken.
pub struct ServerState {
    pub peer_active: bool,
    pub has_remote: bool,
    pub remote: Option<SocketAddr>,
    pub last_heard_age_ms: u64,
    /// None until the first frame is sent (`last_send == 0`).
    pub last_send_age_ms: Option<u64>,
    pub current_num: u64,
    pub acked_num: u64,
    pub outstanding: usize,
    pub srtt: f64,
    pub rto: u64,
    pub send_interval: u64,
    pub bytes_rx: u64,
    pub bytes_tx: u64,
    pub term_gen: u64,
    pub pty_open: bool,
}

impl ServerState {
    pub fn format(&self) -> String {
        format!(
            "role=server pid={} peer_active={} has_remote={} remote={} \
             last_heard_age_ms={} last_send_age_ms={} current_num={} acked_num={} \
             unacked={} outstanding={} srtt={:.0}ms rto={}ms send_interval={}ms \
             bytes_rx={} bytes_tx={} term_gen={} pty_open={}",
            std::process::id(),
            self.peer_active as u8,
            self.has_remote as u8,
            fmt_addr(self.remote),
            self.last_heard_age_ms,
            fmt_age(self.last_send_age_ms),
            self.current_num,
            self.acked_num,
            self.current_num.saturating_sub(self.acked_num),
            self.outstanding,
            self.srtt,
            self.rto,
            self.send_interval,
            self.bytes_rx,
            self.bytes_tx,
            self.term_gen,
            self.pty_open as u8,
        )
    }

    /// Append this snapshot to the diagnostic sink.
    pub fn dump(&self) {
        dump("server", &self.format());
    }
}

/// Client-side transport snapshot, copied out of `drive_client`'s `ClientState`
/// when the SIGUSR2 flag is taken.
pub struct ClientState {
    pub remote: Option<SocketAddr>,
    /// None until the first message is sent (`last_send == 0`).
    pub last_send_age_ms: Option<u64>,
    pub applied_num: u64,
    pub outbox_base: u64,
    pub outbox_pending: usize,
    pub scrollback_len: usize,
    pub srtt: f64,
    pub rto: u64,
    pub send_interval: u64,
    pub bytes_rx: u64,
    pub bytes_tx: u64,
    pub predict_active: bool,
    pub predict_shown: u64,
    pub predict_epoch_lag: u64,
    pub term_gen: u64,
    pub rows: u16,
    pub cols: u16,
    pub echo_on: bool,
}

impl ClientState {
    pub fn format(&self) -> String {
        format!(
            "role=client pid={} remote={} last_send_age_ms={} applied_num={} \
             outbox_base={} outbox_pending={} scrollback_len={} srtt={:.0}ms rto={}ms \
             send_interval={}ms bytes_rx={} bytes_tx={} predict(active={} shown={} epoch_lag={}) \
             term_gen={} rows={} cols={} echo_on={}",
            std::process::id(),
            fmt_addr(self.remote),
            fmt_age(self.last_send_age_ms),
            self.applied_num,
            self.outbox_base,
            self.outbox_pending,
            self.scrollback_len,
            self.srtt,
            self.rto,
            self.send_interval,
            self.bytes_rx,
            self.bytes_tx,
            self.predict_active as u8,
            self.predict_shown,
            self.predict_epoch_lag,
            self.term_gen,
            self.rows,
            self.cols,
            self.echo_on as u8,
        )
    }

    /// Append this snapshot to the diagnostic sink.
    pub fn dump(&self) {
        dump("client", &self.format());
    }
}

fn fmt_addr(addr: Option<SocketAddr>) -> String {
    addr.map(|a| a.to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn fmt_age(age: Option<u64>) -> String {
    age.map(|a| a.to_string())
        .unwrap_or_else(|| "never".to_string())
}

/// The default per-pid sink when `POSH_DEBUG_LOG` is unset: the same socket-dir
/// scheme the sessions (and `just debug-posh-sockets`) already use, so the dump
/// file is discoverable beside them.
fn default_dump_path(role: &str) -> PathBuf {
    let posh_dir = std::env::var("POSH_DIR").ok();
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    let tmpdir = std::env::var("TMPDIR").ok();
    let base = resolve_socket_base(
        posh_dir.as_deref(),
        xdg.as_deref(),
        tmpdir.as_deref(),
        util::uid(),
    );
    // The base may not exist for a bare roaming server (no local session ever
    // created it); make it private (0700) like the session dirs. Best-effort —
    // log_init reports any real open failure.
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&base);
    base.join(format!("posh-{role}-{}.log", std::process::id()))
}

/// Enable debug logging at runtime to the default per-pid sink (if not already
/// active), returning the sink path. The runtime counterpart of `POSH_DEBUG_LOG`
/// for the `Ctrl-^ d` toggle; mirrors `dump`'s lazy-open + path scheme.
pub fn enable_logging(role: &str) -> PathBuf {
    let path = default_dump_path(role);
    if !util::log_active() {
        let _ = util::log_init(&path);
    }
    path
}

/// Append `body` to the diagnostic sink under the `dump` level. Reuses the
/// `POSH_DEBUG_LOG` sink when armed; otherwise lazily opens the per-pid default
/// so SIGUSR2 works even when periodic logging was never enabled — the whole
/// point of the on-demand dump.
pub fn dump(role: &str, body: &str) {
    if !util::log_active() {
        let _ = util::log_init(&default_dump_path(role));
    }
    util::log_write("dump", body);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_state() -> ServerState {
        ServerState {
            peer_active: true,
            has_remote: true,
            remote: Some("100.85.205.39:60006".parse().unwrap()),
            last_heard_age_ms: 1234,
            last_send_age_ms: Some(57),
            current_num: 42,
            acked_num: 40,
            outstanding: 2,
            srtt: 114.0,
            rto: 350,
            send_interval: 57,
            bytes_rx: 8192,
            bytes_tx: 4096,
            term_gen: 99,
            pty_open: true,
        }
    }

    #[test]
    fn server_format_carries_wedge_fields() {
        let line = server_state().format();
        for key in [
            "role=server",
            "peer_active=1",
            "has_remote=1",
            "remote=100.85.205.39:60006",
            "last_heard_age_ms=1234",
            "last_send_age_ms=57",
            "current_num=42",
            "acked_num=40",
            "unacked=2",
            "outstanding=2",
            "srtt=114ms",
            "rto=350ms",
            "pty_open=1",
        ] {
            assert!(line.contains(key), "missing {key:?} in:\n{line}");
        }
    }

    #[test]
    fn server_format_renders_absent_peer_and_no_send() {
        let line = ServerState {
            peer_active: false,
            has_remote: false,
            remote: None,
            last_send_age_ms: None,
            ..server_state()
        }
        .format();
        assert!(line.contains("remote=none"), "{line}");
        assert!(line.contains("last_send_age_ms=never"), "{line}");
        assert!(line.contains("peer_active=0"), "{line}");
    }

    #[test]
    fn client_format_carries_transport_fields() {
        let line = ClientState {
            remote: Some("100.85.205.39:60006".parse().unwrap()),
            last_send_age_ms: Some(20),
            applied_num: 41,
            outbox_base: 7,
            outbox_pending: 3,
            scrollback_len: 500,
            srtt: 114.0,
            rto: 350,
            send_interval: 57,
            bytes_rx: 4096,
            bytes_tx: 8192,
            predict_active: true,
            predict_shown: 5,
            predict_epoch_lag: 1,
            term_gen: 88,
            rows: 40,
            cols: 120,
            echo_on: true,
        }
        .format();
        for key in [
            "role=client",
            "remote=100.85.205.39:60006",
            "last_send_age_ms=20",
            "applied_num=41",
            "outbox_base=7",
            "outbox_pending=3",
            "scrollback_len=500",
            "predict(active=1 shown=5 epoch_lag=1)",
            "rows=40",
            "cols=120",
            "echo_on=1",
        ] {
            assert!(line.contains(key), "missing {key:?} in:\n{line}");
        }
    }
}
