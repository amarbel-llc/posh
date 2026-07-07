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
    /// ms since we last decoded a frame from the server -- transport liveness. A
    /// large value with `bytes_rx` flat means the server went silent / the path
    /// is down, distinct from an apply-stall where frames keep arriving.
    pub last_heard_age_ms: u64,
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
    /// Negotiated frame-sync codec label (#wedge): "dumpdiff" | "morph".
    pub codec: &'static str,
    /// The server's window/icon title (OSC 0/2), mirrored from the client model,
    /// so a debugger can map a pid/socket back to its session by hand.
    pub title: String,
    /// Client apply-path histogram + last received frame (#wedge): a climbing
    /// `basemis` with a frozen `term_gen` is the apply-stall fingerprint.
    pub apply: crate::remote::stats::ApplySnapshot,
    /// Transport-liveness gauges (#false-disconnect): the frame-arrival timing
    /// behind the "Last contact" banner. A nonzero `frame_gaps_late` with a
    /// healthy `retransmits` and steady `heartbeats_rx` means the banner tripped
    /// on an arrival gap, not on a genuinely dead peer.
    pub link: crate::remote::stats::LinkSnapshot,
    /// Whether the "Last contact" banner is currently showing (server_late): the
    /// client's live disconnect verdict, paired with `last_heard_age_ms`.
    pub server_late: bool,
    /// Latest server transport state from CAP_DIAG (#6): the far side of a wedge,
    /// otherwise un-SIGUSR2-able on a remote server. `None` until the server
    /// reports (only in a debug posture, when the client advertised CAP_DIAG).
    pub server_diag: Option<crate::remote::caps::ServerDiag>,
}

impl ClientState {
    pub fn format(&self) -> String {
        format!(
            "role=client pid={} remote={} last_send_age_ms={} last_heard_age_ms={} applied_num={} \
             outbox_base={} outbox_pending={} scrollback_len={} srtt={:.0}ms rto={}ms \
             send_interval={}ms bytes_rx={} bytes_tx={} predict(active={} shown={} epoch_lag={}) \
             term_gen={} rows={} cols={} echo_on={} codec={} title={:?} \
             apply(adv={} stale={} dup={} basemis={} bsum_mis={} reack={} nochange={} sb_rx={}) \
             last_rx(num={} base={} body={}) srv={} \
             link(late={} gap_max={}ms late_gaps={} rx_total={} heartbeats={} retransmits={})",
            std::process::id(),
            fmt_addr(self.remote),
            fmt_age(self.last_send_age_ms),
            self.last_heard_age_ms,
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
            self.codec,
            &self.title,
            self.apply.advanced,
            self.apply.stale,
            self.apply.dup,
            self.apply.basemis,
            self.apply.base_sum_mismatch,
            self.apply.reack,
            self.apply.nochange,
            self.apply.scrollback_rx,
            self.apply.last_rx_num,
            self.apply.last_rx_base,
            self.apply.last_rx_body.as_str(),
            fmt_server_diag(self.server_diag.as_ref()),
            self.server_late as u8,
            self.link.frame_gap_ms_max,
            self.link.frame_gaps_late,
            self.link.frames_total,
            self.link.heartbeats_rx,
            self.link.retransmits,
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

/// The server-side transport piggyback (#6) for the client dump. `none` until
/// the server reports; otherwise the far side's frame/ack/outstanding/gen/pty,
/// to compare against the client's own `applied_num`/`term_gen` at a glance —
/// e.g. server `num` climbing while client `applied_num` is stuck is a clear
/// apply-stall, vs. both frozen pointing at the server or the path.
fn fmt_server_diag(d: Option<&crate::remote::caps::ServerDiag>) -> String {
    match d {
        None => "none".to_string(),
        Some(d) => format!(
            "(pid={} num={} acked={} gen={} out={} pty={})",
            d.pid, d.current_num, d.acked_num, d.term_gen, d.outstanding, d.pty_open as u8,
        ),
    }
}

/// The session socket dir, lazily created private (0700) like the session dirs.
/// Per-pid dump and forensic files live here beside the sockets, so they are
/// discoverable by `just debug-posh-sockets` / `debug-posh-forensics`.
fn sink_base() -> PathBuf {
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
    // created it). Best-effort — the caller's open reports any real failure.
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&base);
    base
}

/// The default per-pid sink when `POSH_DEBUG_LOG` is unset: the same socket-dir
/// scheme the sessions (and `just debug-posh-sockets`) already use, so the dump
/// file is discoverable beside them.
fn default_dump_path(role: &str) -> PathBuf {
    sink_base().join(format!("posh-{role}-{}.log", std::process::id()))
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

/// Byte-level forensics for an apply-stall (#90/#94), captured when the client
/// hits `ReackAndWait`. The transport-layer origin of a base divergence is not
/// reproducible at unit level (the per-frame byte invariant holds in isolation),
/// so the bytes from a live wedge are the only way to root-cause it.
pub struct ForensicReport {
    pub pid: u32,
    pub applied_num: u64,
    pub rx_num: u64,
    pub rx_base: u64,
    pub body: crate::remote::stats::FrameKind,
    pub applied_len: usize,
    pub body_len: usize,
    /// Decoded prefix/suffix from a `Diff` body header; None for other bodies
    /// or a body too short to carry the 8-byte header.
    pub prefix_suffix: Option<(u32, u32)>,
}

impl ForensicReport {
    /// One-line classification of why apply_diff failed (or would mis-apply).
    pub fn verdict(&self) -> String {
        use crate::remote::stats::FrameKind;
        if self.body != FrameKind::Diff {
            return format!(
                "{} body (base={}) -- no prefix/suffix diff to decode",
                self.body.as_str(),
                self.rx_base,
            );
        }
        let Some((p, s)) = self.prefix_suffix else {
            return format!("DIFF_TRUNCATED (diff_len={} < 8 header bytes)", self.body_len);
        };
        let ps = p as usize + s as usize;
        if ps > self.applied_len {
            format!(
                "SHORT_BASE: prefix+suffix={ps} > applied_len={} -- apply_diff returns None, \
                 the #90 apply-stall wedge",
                self.applied_len,
            )
        } else {
            format!(
                "LEN_OK: prefix+suffix={ps} <= applied_len={} -- apply_diff returns Some; if the \
                 base content diverged this is silent corruption (#94)",
                self.applied_len,
            )
        }
    }

    pub fn format(&self) -> String {
        let (p, s) = match self.prefix_suffix {
            Some((p, s)) => (p.to_string(), s.to_string()),
            None => ("-".to_string(), "-".to_string()),
        };
        format!(
            "role=client pid={} applied_num={} last_rx(num={} base={} body={}) \
             applied_len={} body_len={} prefix={p} suffix={s}\nverdict: {}\n",
            self.pid,
            self.applied_num,
            self.rx_num,
            self.rx_base,
            self.body.as_str(),
            self.applied_len,
            self.body_len,
            self.verdict(),
        )
    }
}

/// Write a forensic bundle for a pending apply-stall reack to the sink dir:
/// `posh-forensic-client-<pid>-<rx_num>.{txt,applied,diff}`, returning the
/// `.txt` path. `reack` is `(rx_num, rx_base, body_kind, body_bytes)` as stashed
/// by the client's `ReackAndWait` arm. Lazily creates the sink dir like `dump`,
/// so it works even when periodic logging was never armed -- the whole point,
/// since past wedges happened with no logging in place.
pub fn capture_forensics(
    applied_num: u64,
    applied_data: &[u8],
    reack: &(u64, u64, crate::remote::stats::FrameKind, Vec<u8>),
) -> Option<PathBuf> {
    use crate::remote::stats::FrameKind;
    let (rx_num, rx_base, body, body_bytes) = (reack.0, reack.1, reack.2, &reack.3);
    let prefix_suffix = (body == FrameKind::Diff && body_bytes.len() >= 8).then(|| {
        let p = u32::from_le_bytes(body_bytes[0..4].try_into().unwrap());
        let s = u32::from_le_bytes(body_bytes[4..8].try_into().unwrap());
        (p, s)
    });
    let report = ForensicReport {
        pid: std::process::id(),
        applied_num,
        rx_num,
        rx_base,
        body,
        applied_len: applied_data.len(),
        body_len: body_bytes.len(),
        prefix_suffix,
    };
    let base = sink_base();
    let stem = format!("posh-forensic-client-{}-{}", report.pid, rx_num);
    let txt = base.join(format!("{stem}.txt"));
    // Private (0600) writes, #118: `.applied`/`.diff` are raw screen bytes —
    // whatever was on the terminal at capture time — and must not be born
    // world-readable under a permissive umask.
    util::write_private(&txt, report.format().as_bytes()).ok()?;
    let _ = util::write_private(&base.join(format!("{stem}.applied")), applied_data);
    let _ = util::write_private(&base.join(format!("{stem}.diff")), body_bytes);
    Some(txt)
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
            last_heard_age_ms: 1234,
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
            codec: "morph",
            title: "user@host: ~/work".to_string(),
            apply: crate::remote::stats::ApplySnapshot {
                basemis: 7,
                last_rx_num: 41,
                last_rx_base: 40,
                last_rx_body: crate::remote::stats::FrameKind::Diff,
                ..Default::default()
            },
            server_diag: Some(crate::remote::caps::ServerDiag {
                current_num: 43,
                acked_num: 41,
                term_gen: 90,
                outstanding: 2,
                pty_open: true,
                pid: 4242,
                agent: None,
            }),
            link: crate::remote::stats::LinkSnapshot {
                frames_total: 120,
                heartbeats_rx: 30,
                frame_gap_ms_max: 8000,
                frame_gaps_late: 2,
                retransmits: 4,
                ..Default::default()
            },
            server_late: true,
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
            "codec=morph",
            "last_heard_age_ms=1234",
            "title=\"user@host: ~/work\"",
            "apply(adv=0 stale=0 dup=0 basemis=7 bsum_mis=0 reack=0 nochange=0 sb_rx=0)",
            "last_rx(num=41 base=40 body=diff)",
            "srv=(pid=4242 num=43 acked=41 gen=90 out=2 pty=1)",
            "link(late=1 gap_max=8000ms late_gaps=2 rx_total=120 heartbeats=30 retransmits=4)",
        ] {
            assert!(line.contains(key), "missing {key:?} in:\n{line}");
        }
    }

    #[test]
    fn client_format_server_diag_absent_reads_none() {
        let line = ClientState {
            remote: None,
            last_send_age_ms: None,
            last_heard_age_ms: 0,
            applied_num: 0,
            outbox_base: 0,
            outbox_pending: 0,
            scrollback_len: 0,
            srtt: 0.0,
            rto: 0,
            send_interval: 0,
            bytes_rx: 0,
            bytes_tx: 0,
            predict_active: false,
            predict_shown: 0,
            predict_epoch_lag: 0,
            term_gen: 0,
            rows: 24,
            cols: 80,
            echo_on: false,
            codec: "dumpdiff",
            title: String::new(),
            apply: crate::remote::stats::ApplySnapshot::default(),
            link: crate::remote::stats::LinkSnapshot::default(),
            server_late: false,
            server_diag: None,
        }
        .format();
        assert!(line.contains("srv=none"), "{line}");
    }

    use crate::remote::stats::FrameKind;

    fn forensic(
        body: FrameKind,
        applied_len: usize,
        prefix_suffix: Option<(u32, u32)>,
        body_len: usize,
    ) -> ForensicReport {
        ForensicReport {
            pid: 123,
            applied_num: 5272,
            rx_num: 5276,
            rx_base: 5272,
            body,
            applied_len,
            body_len,
            prefix_suffix,
        }
    }

    #[test]
    fn forensic_verdict_short_base_is_the_wedge() {
        // prefix+suffix (120) > applied_len (100): the #90 apply-stall.
        let r = forensic(FrameKind::Diff, 100, Some((80, 40)), 200);
        assert!(r.verdict().starts_with("SHORT_BASE"), "{}", r.verdict());
        let line = r.format();
        assert!(line.contains("verdict: SHORT_BASE"), "{line}");
        assert!(line.contains("prefix=80 suffix=40"), "{line}");
        assert!(line.contains("last_rx(num=5276 base=5272 body=diff)"), "{line}");
    }

    #[test]
    fn forensic_verdict_len_ok_flags_content_divergence() {
        // prefix+suffix (80) <= applied_len (100): apply_diff returns Some;
        // a divergent base is then silent corruption (#94).
        let r = forensic(FrameKind::Diff, 100, Some((40, 40)), 90);
        assert!(r.verdict().starts_with("LEN_OK"), "{}", r.verdict());
        assert!(r.verdict().contains("#94"), "{}", r.verdict());
    }

    #[test]
    fn forensic_verdict_truncated_and_non_diff() {
        let trunc = forensic(FrameKind::Diff, 100, None, 4);
        assert!(
            trunc.verdict().starts_with("DIFF_TRUNCATED"),
            "{}",
            trunc.verdict(),
        );
        let morph = forensic(FrameKind::Morph, 100, None, 50);
        assert!(morph.verdict().contains("morph"), "{}", morph.verdict());
    }
}
