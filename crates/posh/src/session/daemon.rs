//! Per-session daemon: owns the PTY and broadcasts output to attached
//! clients over a Unix socket (zmx daemonLoop port).

use std::collections::VecDeque;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

use posh_proto::caps::SessionKind;
use posh_term::{ScreenSwitch, Terminal};

use crate::overlay::{close_overlay, escape_command, Overlay};
use crate::pty::{self, PtyChild};
use crate::remote::caps;
use crate::remote::display::Snapshot;
use crate::remote::framesync::FrameProducer;
use crate::remote::history::{HistoryCursor, HistoryStart, SB2_ROWS_PER_BODY};
use crate::remote::introspect;
use crate::remote::sync::{base_checksum, FrameBody, ServerFrame};
use crate::session::ipc::{self, FrameBuffer, SessionInfo, Tag};
use crate::session::{self, Config};
use crate::util::{self, Error, Result};

const SCROLLBACK: usize = 10_000;

/// A `.castx` recorder writing to a boxed sink (a file, in practice). Built
/// when `$POSH_RECORD_FILE` is set (`posh --record FILE`); tees the session's
/// raw PTY output so `poshterity replay` can reproduce the screen deterministically.
type SessionRecorder = poshterity::castx::Recorder<Box<dyn Write>>;

/// Open the recording at `path` (`$POSH_RECORD_FILE`, if set) and write its
/// header. A failure to open/write only logs and disables recording — it must
/// never stop the session from starting.
fn open_recorder(path: Option<std::ffi::OsString>, rows: u16, cols: u16) -> Option<SessionRecorder> {
    let path = path?;
    let file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            util::log_write("warn", &format!("--record: cannot open {path:?}: {e}"));
            return None;
        }
    };
    let writer: Box<dyn Write> = Box::new(std::io::BufWriter::new(file));
    let mut rec = poshterity::castx::Recorder::new(writer);
    let header = poshterity::castx::Header {
        version: 2,
        width: cols,
        height: rows,
        poshterity: Some(poshterity::castx::Poshterity {
            v: 1,
            emu_rev: posh_term::emu_rev(),
        }),
    };
    if let Err(e) = rec.write_header(&header) {
        util::log_write("warn", &format!("--record: cannot write header: {e}"));
        return None;
    }
    Some(rec)
}

/// A client whose unsent backlog grows past this is treated as a stuck
/// reader and dropped, so one wedged terminal can't OOM the daemon and take
/// down every other attached client. github #11.
const MAX_CLIENT_BACKLOG: usize = 16 * 1024 * 1024;

/// Paced delivery (posh#225, RFC 0008 §3.2): the least time between two
/// fresh visible frames to one paced viewport, however promptly it acks.
/// Starts at `server_loop`'s send-interval floor (`datagram::SEND_INTERVAL_MIN`).
/// A tuning value: change it only with a measurement recorded in FDR 0021.
const PACED_FRAME_FLOOR_MS: u64 = 20;
/// The longest a dirty paced viewport waits for the ack of its last fresh
/// frame before it is sent the next anyway — what keeps a lost ack from
/// stalling its screen. Starts at `server_loop`'s send-interval ceiling
/// (`SEND_INTERVAL_MAX`). A tuning value, as above.
const PACED_ACK_WAIT_MS: u64 = 250;
/// How often a paced viewport's `paced ack latency` log line may repeat
/// (posh#225 Stage 3.0): the field series Task 3.4 is tuned from.
const ACK_LOG_INTERVAL_MS: u64 = 10_000;
/// v2 history (posh#225 Stage 3): rows a paced viewport may have in flight
/// before its ack — THE history rate limiter, with the socket's own
/// backpressure (an empty `write_buf`): throughput ≈ one window per round
/// trip. The frame floor does not apply to history (it caps the screen's
/// encode cost; a body is a row copy). The daemon's stand-in for
/// `server_loop`'s SRTT-paced send interval; the static share Task 3.4
/// makes dynamic. A tuning value: change it only with a measurement in FDR 0021.
const HISTORY_WINDOW_ROWS: u64 = 2 * SB2_ROWS_PER_BODY;
/// The v2 resend floor before any ack latency has been measured: TCP's
/// initial RTO. A tuning value, as above.
const HISTORY_RESEND_INITIAL_MS: u64 = 4 * PACED_ACK_WAIT_MS;
/// Resend backoff: the floor doubles per resend without ack progress, at
/// most this many times.
const HISTORY_RESEND_MAX_DOUBLINGS: u32 = 3;

/// Ensures the session exists, forking off a daemon when needed. Returns
/// true when a new session was created. The daemon is a double-forked
/// grandchild that never returns from this function (it exits the process).
/// `kind` is what the creator states the session IS (design 2026-09-21 §1);
/// a freshly created daemon stores it for life and reports it in `Tag::Info`.
/// Like `command`, it is ignored when the session already exists.
pub fn ensure_session(
    cfg: &Config,
    name: &str,
    command: Option<Vec<String>>,
    kind: SessionKind,
) -> Result<bool> {
    let created = ensure_session_in(cfg, name, command, kind, None, None)?;
    if created {
        // A CLI creator gives the grandchild a beat to exist before it
        // connects (the socket is already bound, so a fast connect just
        // queues). Not in `ensure_session_in`: a daemon serving push-cmd
        // must not stall its own poll loop — and every attached viewport.
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(created)
}

/// [`ensure_session`] for a daemon creating a session on a viewport's behalf
/// (RFC 0016 §4): the new daemon starts in `cwd` rather than the creator's
/// own directory, and `seed` pre-marks a push-cmd token as served so a late
/// repeat that reaches the new session is ignored. A CLI creator passes
/// `None` for both — its cwd is the one the new daemon should inherit.
fn ensure_session_in(
    cfg: &Config,
    name: &str,
    command: Option<Vec<String>>,
    kind: SessionKind,
    cwd: Option<&str>,
    seed: Option<u64>,
) -> Result<bool> {
    let path = cfg.socket_path(name)?;
    if session::session_socket_exists(&path) {
        match session::probe_session(&path) {
            Ok(_) => {
                if command.is_some() {
                    util::log_write(
                        "warn",
                        &format!("session already exists, ignoring command session={name}"),
                    );
                }
                return Ok(false);
            }
            Err(_) => {
                // Only reclaim the socket if the daemon is genuinely gone; a
                // slow-but-live daemon means the session already exists, so
                // don't remove its socket and spawn a duplicate. github #15.
                if !session::cleanup_stale_socket(&path) {
                    return Ok(false);
                }
            }
        }
    } else if std::fs::symlink_metadata(&path).is_ok() {
        return Err(Error::Msg(format!(
            "{} exists and is not a socket",
            path.display()
        )));
    }

    // Bind before forking so a racing client can connect (and queue) as soon
    // as the parent returns.
    let listener =
        UnixListener::bind(&path).map_err(|e| Error::Msg(format!("bind {}: {e}", path.display())))?;
    if util::double_fork()? {
        drop(listener);
        return Ok(true);
    }
    // Shed every descriptor the creator held (the mux spawn's rule,
    // `remote/mux.rs`). A daemon outlives its creator, and its creator may
    // be long-lived — the relay, `posh-server mux`, or (push-cmd) another
    // daemon holding client sockets and a PTY master — so an inherited fd
    // would pin that resource open and hide its EOF.
    util::shed_creator(&[listener.as_raw_fd()]);
    // The directory was resolved (and checked to exist) by the caller; if it
    // vanished since, the daemon keeps the creator's and reports that.
    if let Some(dir) = cwd {
        let _ = std::env::set_current_dir(dir);
    }
    daemon_main(cfg, name, listener, command, kind, seed);
}

struct ClientConn {
    stream: UnixStream,
    read_buf: FrameBuffer,
    write_buf: Vec<u8>,
    // Zero means "size not yet reported"; ignored for the shared minimum.
    rows: u16,
    cols: u16,
    // Capabilities the client advertised on its `Tag::Init` (RFC 0001 table,
    // github #100). Read by `is_frame_capable` to decide whether this client
    // gets a `FrameProducer` (and thus `Tag::Frame` output) when the session
    // frame-emission gate is on.
    caps: Vec<caps::Cap>,
    // Per-client visible-frame producer (RFC 0008), `Some` exactly when this
    // client advertised frame support on its Init. While `Some`, the daemon
    // emits posh-proto `ServerFrame`s (`Tag::Frame`) to this client instead of
    // raw `Tag::Output`; each client diffs against its OWN acked base, so a
    // freshly attached client's first frame is a `Full` while an established one
    // gets a `Diff`. `None` (a baseline, non-frame client) ⇒ legacy `Tag::Output`.
    producer: Option<FrameProducer>,
    // Whether this client relays its frames onto a LOSSY link (it advertised
    // `CAP_LOSSY` on Init — the Phase 3 frame relay, RFC 0008 §3). A lossy client
    // is NOT self-acked: `queue_frame`/scrollback skip the immediate
    // `producer.ack`, so the diff base advances only on a forwarded
    // `Tag::FrameAck`, each new frame supersedes the last unacked one, and the
    // relay keeps O(1) retransmit state. It also selects the codec (MorphDelta if
    // `CAP_MORPH`) and stamps `base_sum` (if `CAP_BASE_SUM`) from its caps. A
    // reliable local client never sets this, so `lossy` stays false and its frame
    // stream is byte-identical to today (self-acked, DumpDiff, no base_sum).
    lossy: bool,
    // Local write-buffer coalescing (posh#137). `coalesce` is set from
    // `CAP_COALESCE` on Init (like `lossy`, but independent — a client is one or
    // the other): the local stream client opts in so its diff base advances only
    // on its own `Tag::FrameAck` and the daemon replaces a still-un-sent trailing
    // visible frame in `write_buf` rather than appending a second, bounding a
    // burst below `MAX_CLIENT_BACKLOG` (the spontaneous-detach bug). `coalesce_off`
    // is a runtime toggle (via `FRAME_ACK_COALESCE_OFF`, the command palette): when
    // true the client reverts to today's self-ack+append even though it advertised
    // the cap. `pending_frame_start` is the byte offset in `write_buf` where the
    // last-queued, still-fully-un-sent visible `Tag::Frame` begins — the frame the
    // next visible frame may truncate-and-replace; `None` when there is no clean
    // coalescable trailing frame (any non-visible append clears it, and the drain
    // loop clears/shifts it as bytes go on the wire).
    coalesce: bool,
    coalesce_off: bool,
    pending_frame_start: Option<usize>,
    // Per-client scrollback-sync bookkeeping (RFC 0002 §2/§3), the session-socket
    // analog of the roaming server's per-connection `sb_floor`/`acked_sb_total`.
    // `sb_floor` is the daemon terminal's monotonic scrollback total at which
    // this client's forward-only accumulation (re)started — set when frames are
    // enabled (attach) and again when the SESSION WIDTH changes (§4's reflow:
    // a height change only pushes or pops ring rows without renumbering them,
    // so it is not a boundary; `reset_scrollback_floors_on_reflow`).
    // `acked_sb_total` is the total the
    // client holds; on the reliable socket each scrollback frame is self-acked at
    // once, so it advances immediately (no separate `sb_high` is needed —
    // produced always equals acked here). A scrollback frame is emitted only when
    // the daemon total grows past `acked_sb_total.max(sb_floor)`.
    sb_floor: u64,
    acked_sb_total: u64,
    // Backlog instrumentation (posh#131 sibling — the MAX_CLIENT_BACKLOG drop
    // diagnosis): distinguish a STALLED reader (write_buf grows while the socket
    // never drains) from a BURSTY one (draining, but the app outpaces it).
    // `bytes_drained` is the lifetime total successfully written to the socket;
    // `last_drain_ms` is when the last non-zero drain happened (util::now_ms);
    // `hiwater_mb` throttles the growth breadcrumb to one line per new MiB.
    bytes_drained: u64,
    last_drain_ms: u64,
    hiwater_mb: usize,
    /// The ACTIVE pty's ECHO state as of this loop iteration, stamped onto
    /// every frame this client is sent (FDR 0006: the optimistic-echo gate's
    /// FLAG_ECHO — `server_loop` computes the same per send). Refreshed at
    /// the top of each daemon iteration; 0 until the first refresh, so a
    /// brand-new conn's replay frame errs toward echo-suppressed.
    echo_flag: u8,
    /// FLAG_OVERLAY while the escape-to-shell overlay (FDR 0008) is up, OR'd
    /// into every frame's flags beside `echo_flag`. A roaming client reads it
    /// to clear the "opening shell…" notice (posh#178) — the daemon's own
    /// counterpart to the Arch-A server's FLAG_OVERLAY, which the relay
    /// forwards verbatim (`relay::rewrap`). Refreshed per loop iteration.
    overlay_flag: u8,
    /// RFC 0014 §3: the ORIGINATING client's introspection record — identity
    /// and latest state from the Init table or a later `Tag::ClientCaps`.
    /// `record_at` is when the state was decoded (`util::now_ms`), for the
    /// §4.2 `age=`; `attach_pid` is the pid this connection's own Init
    /// identified as, so a `ClientCaps` identity with a DIFFERENT pid marks
    /// this attachment as a relay and that pid as the origin (`via=relay`).
    record: introspect::ClientRecord,
    record_at: u64,
    attach_pid: Option<u32>,
    /// When this connection last delivered `Tag::Input` (util::now_ms; 0 =
    /// never). The FDR 0012 switch router picks the most-recent-input
    /// attached connection — tmux's current-client heuristic, and
    /// per-viewport by construction (every relay/M2 channel serves one
    /// viewport). RFC 0008 §3.1.
    last_input_ms: u64,
    /// RFC 0013 §5.2 on-frame activity label: `wants_activity` latches once
    /// this client (or the client behind its relay/bridge) requested id 15;
    /// `activity_now` is the daemon's current label, refreshed per loop
    /// iteration for requesting clients; `activity_sent` what this client
    /// last received — the entry rides a visible frame only when they differ.
    wants_activity: bool,
    activity_now: Option<caps::SessionActivity>,
    activity_sent: Option<caps::SessionActivity>,
    /// The session's kind (`CAP_SESSION_KIND`, id 20): fixed at create time,
    /// so it rides the SAME visible frame as this client's first activity
    /// entry and never again (`kind_sent`). Stored per connection at accept
    /// so `queue_frame` needs no new parameter.
    kind: SessionKind,
    kind_sent: bool,
    /// RFC 0016 §2: this connection asked for push-cmd (`CAP_PUSH_CMD`);
    /// the offer rides its first activity-bearing frame once (`push_offered`).
    wants_push_cmd: bool,
    push_offered: bool,
    /// The geometry the newest visible dump built for this client was SHAPED
    /// FOR: `Some((rows, cols))` when `dump_vt_mirror` bounded it for that
    /// mirror size, `None` when it was the full fallback (whose in-flight
    /// frames still apply at any size) or no visible dump has been built. Recorded where
    /// the dump is built (`note_visible_dump_shape`), so no later moment has
    /// to re-derive it; `owes_regeometry_frame` compares it with the client's
    /// current size (posh#225).
    visible_shaped_for: Option<(u16, u16)>,
    /// A MorphDelta client's pending regeometry keyframe: owed from
    /// `prepare_regeometry_frame` dropping its base until the next visible
    /// frame is built (the replay, or whichever comes next), then that
    /// frame's number. Until the client acks it or a later frame,
    /// `apply_frame_ack` keeps dropping any base a late ack restores, so
    /// every visible frame stays a `Full`.
    regeometry_keyframe: Option<RegeometryKeyframe>,
    /// Paced delivery (posh#225): `Some` exactly when this client's Init
    /// carried a well-formed `CAP_PACED`. Its visible frames are not built
    /// per PTY read: `broadcast_output` and every other frame-owing site
    /// mark it dirty (`request_frame_from`), and `send_paced_frames` builds
    /// at most one per send opportunity (`paced_send_at`).
    pacing: Option<Pacing>,
    /// Whether this connection's `Tag::Init` has been applied (`apply_init`;
    /// read through `initialized`). Until then the daemon does not know
    /// whether it is a frame viewport, a baseline one, or a one-shot control
    /// connection (`posh list`'s Info probe, `history`, `kill`, a switch
    /// request) that never Inits, so it is sent no broadcast output
    /// (posh#239): the attach replay its Init triggers is its first screen.
    init_applied: bool,
}

/// See `ClientConn::regeometry_keyframe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegeometryKeyframe {
    /// Not built yet: every ack is for a pre-resize frame.
    Owed,
    /// Recorded by `queue_frame` when the keyframe was built.
    Sent(u64),
}

/// A paced client's send-time state (posh#225, RFC 0008 §3.2).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Pacing {
    /// A visible frame is owed: output reached the broadcast source since
    /// the last paced frame, or an event (attach replay, regeometry,
    /// resync, activity answer, source swap) asked for one.
    dirty: bool,
    /// When the newest paced visible frame was queued (the caller's clock);
    /// whether it is still outstanding is the producer's to say. `None`
    /// before the first, and after a RESYNC (the client gave up on what was
    /// outstanding): the next frame then waits only for an empty `write_buf`.
    last_fresh: Option<u64>,
    /// `(frame number, queued at)` for each paced visible frame not yet
    /// confirmed by an ack, oldest first, at most `SENT_FRAME_LOG_CAP`
    /// (posh#225 Stage 3.0): what an ack's round trip is measured from.
    /// Emptied on a RESYNC, with `last_fresh`.
    sent_frames: VecDeque<(u64, u64)>,
    /// Its frame-ack timing (posh#225 Stage 3.0): logged, and the RTT the
    /// v2 resend floor follows (`history_resend_after`).
    acks: AckLatency,
    /// RFC 0009 v2 history (posh#225 Stage 3): `Some` when this paced
    /// viewport's Init carried a well-formed `CAP_SCROLLBACK2`; it then gets
    /// `Scrollback2` bodies and never v1 `Scrollback` frames.
    history: Option<HistoryCursor>,
    /// The v2 extent (RFC 0009 §3.1, posh#225 Stage 4) as of the newest
    /// pass with the session terminal; rides every frame to a viewport that
    /// asked; frozen under the escape overlay. `None` for a viewport that
    /// did not ask, or has no cursor.
    extent: Option<caps::Scrollback2Extent>,
    /// Whether the newest paced send was a visible frame (`server_loop`'s
    /// `last_was_sb`, inverted): when both kinds are due, the other one
    /// goes. False before any send, so the first tie is the screen's.
    last_was_screen: bool,
}

/// The most entries `Pacing::sent_frames` keeps: 16 unacked visible frames
/// (scrollback slots are never logged). Unacked, they are sent one per
/// `PACED_ACK_WAIT_MS`, so the log covers a round trip of about 4 s; past
/// that the oldest is dropped before its ack lands, and acks stop sampling.
const SENT_FRAME_LOG_CAP: usize = 16;

/// A paced viewport's frame-ack timing (posh#225 Stage 3.0): the round trip
/// of a paced visible frame — from when it was queued, through the bridge
/// and the link, applied, to when its ack arrived — which is the only RTT
/// the daemon can see. Each frame is sampled at most once — an ack that
/// confirms several logged frames samples only the newest. Read by
/// `history_resend_after` and the backlog/ack-latency log lines. Its own
/// integer filter, not `datagram::RttEstimator`: that one serves a
/// `Connection`'s timestamp echoes and drops samples of 5 s or more — the
/// stalled-viewport regime this one exists to see.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct AckLatency {
    /// When the newest `Tag::FrameAck` arrived, advancing or not.
    last_ack_at: Option<u64>,
    last_ms: Option<u64>,
    /// Smoothed like TCP's SRTT: the first sample, then 7/8 old + 1/8 new.
    srtt_ms: Option<u64>,
    min_ms: Option<u64>,
    max_ms: u64,
    samples: u64,
    /// `samples` at the last `paced ack latency` line, and when it was written.
    logged_samples: u64,
    logged_at: Option<u64>,
}

impl AckLatency {
    fn record(&mut self, ms: u64) {
        self.last_ms = Some(ms);
        self.srtt_ms = Some(self.srtt_ms.map_or(ms, |s| (7 * s + ms) / 8));
        self.min_ms = Some(self.min_ms.map_or(ms, |m| m.min(ms)));
        self.max_ms = self.max_ms.max(ms);
        self.samples += 1;
    }

    /// `ack_ms=<last>/<srtt>/<min>/<max> ack_n=<samples> ack_age_ms=<ms>`,
    /// `none` where there is nothing yet.
    fn log_fields(&self, now: u64) -> String {
        let ms = match (self.last_ms, self.srtt_ms, self.min_ms) {
            (Some(last), Some(srtt), Some(min)) => format!("{last}/{srtt}/{min}/{}", self.max_ms),
            _ => "none".to_owned(),
        };
        let age = self
            .last_ack_at
            .map_or_else(|| "none".to_owned(), |at| now.saturating_sub(at).to_string());
        format!("ack_ms={ms} ack_n={} ack_age_ms={age}", self.samples)
    }

    /// When a `paced ack latency` line is due at `now` — new samples since
    /// the last one, which is at least `ACK_LOG_INTERVAL_MS` old — stamp it
    /// as written and return the samples new since the last.
    fn take_log_line(&mut self, now: u64) -> Option<u64> {
        let due = self.samples > self.logged_samples
            && self
                .logged_at
                .is_none_or(|at| now.saturating_sub(at) >= ACK_LOG_INTERVAL_MS);
        if !due {
            return None;
        }
        let new = self.samples - self.logged_samples;
        self.logged_samples = self.samples;
        self.logged_at = Some(now);
        Some(new)
    }
}

impl ClientConn {
    /// Retain the RFC 0014 entries in a cap table (§3): identity and state,
    /// keyed to this connection. `from_init` marks the table as this
    /// attachment's own (its pid becomes `attach_pid`); a later `ClientCaps`
    /// identity with another pid is the origin behind a relay.
    fn absorb_client_caps(&mut self, table: &[caps::Cap], now: u64, from_init: bool) {
        // RFC 0013 §5.2: an activity-label request latches for the
        // connection (the client re-sends it on every message anyway).
        if caps::find(table, caps::CAP_SESSION_ACTIVITY).is_some() {
            self.wants_activity = true;
        }
        if caps::find(table, caps::CAP_PUSH_CMD).is_some_and(|c| c.payload.is_empty()) {
            self.wants_push_cmd = true;
        }
        if let Some(cap) = caps::find(table, caps::CAP_CLIENT_IDENT) {
            if let Ok(ident) = introspect::decode_client_ident(&cap.payload) {
                if from_init {
                    self.attach_pid = Some(ident.pid);
                } else if let Some(attach) = self.attach_pid.filter(|p| *p != ident.pid) {
                    self.record.via_relay_pid = Some(attach);
                }
                self.record.ident = Some(ident);
            }
        }
        if let Some(cap) = caps::find(table, caps::CAP_CLIENT_STATE) {
            if let Ok(state) = introspect::decode_client_state(&cap.payload) {
                self.record.state = Some(state);
                self.record_at = now;
            }
        }
        // posh#225 Stage 3 (RFC 0009 §3): a v2 viewport's cumulative ack,
        // forwarded by the M2 bridge on every change. On an Init this finds
        // no cursor yet: `open_history` takes the Init's count itself.
        if let Some(entry) = caps::find(table, caps::CAP_SCROLLBACK2)
            .and_then(|c| caps::decode_scrollback2_client(&c.payload).ok())
        {
            if let Some(h) = self.history_mut() {
                h.on_ack(entry.epoch, entry.acked_rows);
            }
        }
    }

    /// Open this paced viewport's v2 cursor from its Init's SCROLLBACK2
    /// entry, at `term`'s total (the daemon's Init arm, beside `sb_floor`).
    /// A viewport holding an epoch is continued at its count, so a
    /// reconnect keeps its ring (a fresh epoch would make it clear it); one
    /// holding none (epoch 0) gets a fresh epoch. A bare re-Init keeps the
    /// cursor it has.
    fn open_history(&mut self, term: &Terminal) {
        if self.producer.is_none() {
            return;
        }
        let entry = caps::find(&self.caps, caps::CAP_SCROLLBACK2)
            .and_then(|c| caps::decode_scrollback2_client(&c.payload).ok());
        let (Some(p), Some(entry)) = (self.pacing.as_mut(), entry) else {
            return;
        };
        if p.history.is_some() {
            return;
        }
        let start = match entry.epoch {
            0 => HistoryStart::Fresh,
            epoch => HistoryStart::Continue {
                epoch,
                rows: entry.acked_rows,
            },
        };
        let mut cursor = HistoryCursor::new((self.rows, self.cols));
        cursor.activate(start, term.primary_scrollback_total());
        p.history = Some(cursor);
        // Seed the cached extent, so a frame sent before the first send pass
        // (the overlay up at attach) still carries one beside the id-10 entry.
        self.note_history_extent(term);
    }

    /// RFC 0013 §5.2: this client asked for the activity label and has not
    /// been sent the current one (the kind and push-cmd offer ride with it).
    fn answer_due(&self) -> bool {
        self.wants_activity && self.activity_now.is_some() && self.activity_now != self.activity_sent
    }

    /// This client's §4.2 record with `age=` filled in from `now`.
    fn record_now(&self, now: u64) -> introspect::ClientRecord {
        let mut r = self.record.clone();
        r.age_ms = r.state.map(|_| now.saturating_sub(self.record_at));
        r
    }

    fn queue(&mut self, tag: Tag, payload: &[u8]) {
        // Any append other than the coalescable visible frame `queue_frame` is
        // about to (re)establish breaks the "pending frame is a clean tail"
        // invariant, so drop the coalesce anchor here (posh#137). A `Tag::Output`,
        // `Tag::Exit`, or scrollback `Tag::Frame` landing after a visible frame
        // must not be truncated away; `queue_frame` re-sets the anchor AFTER its
        // own `self.queue(Tag::Frame, ..)` call, so the visible frame keeps it.
        self.pending_frame_start = None;
        ipc::append_frame(&mut self.write_buf, tag, payload);
    }

    /// Whether this client's frames should be coalesced right now: it advertised
    /// `CAP_COALESCE` AND the runtime toggle has not turned it off (posh#137).
    fn coalescing(&self) -> bool {
        self.coalesce && !self.coalesce_off
    }

    /// Applies a `Tag::Init` payload: a 4-byte resize prefix that sizes the
    /// PTY, optionally followed by an RFC 0001 capability table (the
    /// framesync handshake, github #100). Returns whether the reported size
    /// was updated. The trailing table is parsed and recorded but NOT acted
    /// on here — the daemon's output path is unchanged this task.
    ///
    /// The resize is decoded from the first 4 bytes only, because `posh`'s
    /// `decode_resize` rejects any non-4-byte payload; a cap-extended Init
    /// must still size the PTY. An absent or malformed trailing table leaves
    /// any previously negotiated caps in place (a bare re-`Init` on SIGCONT
    /// resume does not wipe them).
    fn apply_init(&mut self, payload: &[u8]) -> bool {
        self.init_applied = true;
        let resized = payload.get(..4).is_some_and(|prefix| self.apply_resize(prefix));
        if payload.len() > 4 {
            match caps::decode_table(&payload[4..]) {
                Ok((advertised, _)) => {
                    // A relay advertises `CAP_LOSSY` to opt this client into
                    // lossy mode (no self-ack; RFC 0008 §3). Tracks the latest
                    // negotiated table, so a bare re-Init (which skips this block)
                    // preserves it exactly like `self.caps`.
                    self.lossy = caps::find(&advertised, caps::CAP_LOSSY).is_some();
                    // A local stream client advertises `CAP_COALESCE` (posh#137):
                    // like lossy it is NOT self-acked, but it keeps plain local
                    // semantics (DumpDiff, no base_sum). Independent of `lossy` — a
                    // client is one or the other. Preserved across a bare re-Init.
                    self.coalesce = caps::find(&advertised, caps::CAP_COALESCE).is_some();
                    // posh#225 (RFC 0008 §3.2): a re-Init without the cap
                    // clears pacing; an owed screen then goes out by today's
                    // path on the next broadcast.
                    let paced = caps::find(&advertised, caps::CAP_PACED)
                        .and_then(|c| caps::decode_paced(&c.payload))
                        .is_some();
                    self.pacing = paced.then(|| self.pacing.take().unwrap_or_default());
                    // RFC 0014: a client's Init table may carry its identity
                    // and state (the local client always does; a relay carries
                    // its own identity here and the origin's via ClientCaps).
                    self.absorb_client_caps(&advertised, util::now_ms(), true);
                    self.caps = advertised;
                }
                Err(e) => util::log_write(
                    "warn",
                    &format!("malformed Init cap table, treating peer as baseline: {e}"),
                ),
            }
        }
        resized
    }

    /// Whether this connection's `Tag::Init` has been applied (see
    /// `init_applied`). `rows > 0` is no proxy: a `Tag::Resize` before the
    /// Init sizes a client too.
    fn initialized(&self) -> bool {
        self.init_applied
    }

    /// Applies a `Tag::Resize` payload: the client's reported size. Returns
    /// whether it decoded (a malformed payload is ignored). Extracted, like
    /// `apply_init`, so the daemon-loop arm and the inline tests drive one path.
    fn apply_resize(&mut self, payload: &[u8]) -> bool {
        match ipc::decode_resize(payload) {
            Some((r, w)) => {
                self.rows = r;
                self.cols = w;
                true
            }
            None => false,
        }
    }

    /// Records which geometry the visible dump about to be built from `src`
    /// is shaped for (`visible_shaped_for`) and returns it: `Some(size)` when
    /// `dump_vt_mirror` bounds it for this client's size, `None` for the full
    /// fallback. The one place both dump-building sites (`build_frame_from`,
    /// `broadcast_output`) note it; the return value is also
    /// `broadcast_output`'s dump-cache key.
    fn note_visible_dump_shape(&mut self, src: &Terminal) -> Option<(u16, u16)> {
        self.visible_shaped_for = src
            .dump_vt_mirror_is_bounded(self.rows, self.cols)
            .then_some((self.rows, self.cols));
        self.visible_shaped_for
    }

    /// Whether this client is owed a frame built for its current geometry: it
    /// holds a producer and its newest visible dump was shaped for another
    /// size (see `prepare_regeometry_frame` for the rule and its reasons).
    fn owes_regeometry_frame(&self) -> bool {
        self.producer.is_some() && self.visible_shaped_for.is_some_and(|g| g != (self.rows, self.cols))
    }

    /// Whether this client's visible frames use the MorphDelta codec: a lossy
    /// client that negotiated `CAP_MORPH` (`queue_frame`'s `use_morph`).
    fn uses_morph(&self) -> bool {
        self.lossy && caps::find(&self.caps, caps::CAP_MORPH).is_some()
    }

    /// The regeometry rule (posh#225), decided after a client's batch: returns
    /// whether this client is owed a frame for its current geometry
    /// (`owes_regeometry_frame`), and when it is, makes sure the replay the
    /// loop then queues actually carries that geometry's dump.
    ///
    /// No client re-renders the dump it already holds when it resizes
    /// itself, and neither the local nor the roaming client requests a resync
    /// on its own resize; a viewport is repainted by the next frame, which, if
    /// the session size does not change with it (it was not the smallest
    /// client), waits for the next PTY output. That was every client's lot
    /// before posh#225. What posh#225 changed is that a BOUNDED dump is shaped
    /// for the size it was built for (`visible_shaped_for`, recorded when the
    /// newest visible dump was built, so it is exact whatever the session's
    /// size did in between): such a client's in-flight frames are wrong at its
    /// new size, so it is owed a frame now. A client whose newest dump was
    /// the full fallback (wider, narrower or shorter than the session, or no
    /// size reported yet) is owed nothing NEW: its in-flight frames still
    /// apply at the new size, and, as before posh#225, the next output
    /// repaints it. Sending it a frame on every resize would be a ring-sized
    /// frame per resize event for a drag-resized wider viewport.
    ///
    /// A DumpDiff client keeps its acked base — a `Diff` against the
    /// old-geometry dump is byte-level, so it rebuilds the new dump exactly,
    /// and the producer already falls back to a `Full` when the diff is no
    /// win. A MorphDelta client's base is DROPPED, forcing a `Full` as
    /// `broadcast_source_swap` does: its encoder judges expressibility on the
    /// SESSION's dims and alt flag, which a client's own resize leaves
    /// unchanged, so it would morph between two identical snapshots — a
    /// near-empty frame with no dump to rebuild the new geometry from. The
    /// keyframe is made DURABLE through `regeometry_keyframe`: a late ack for
    /// a pre-resize frame would otherwise restore that frame as the base, and
    /// if the keyframe was lost on the way the client would never receive a
    /// new-geometry dump (see `apply_frame_ack`). Repeating the drop while the
    /// frame is still owed (the replay waits for the session's first PTY
    /// output) is harmless: it only ever makes the next visible frame a
    /// `Full`, and building that frame re-records `visible_shaped_for`, which
    /// ends the debt. For a paced client the frame also waits for its next
    /// send opportunity (`queue_replay` marks it owed; posh#225).
    fn prepare_regeometry_frame(&mut self) -> bool {
        if !self.owes_regeometry_frame() {
            return false;
        }
        if self.uses_morph() {
            if let Some(p) = self.producer.as_mut() {
                p.drop_acked_base();
                // Every frame from here on is built without a base, so the
                // first of them is the new-geometry keyframe the client must
                // ack; `queue_frame` records its number when it is built.
                self.regeometry_keyframe = Some(RegeometryKeyframe::Owed);
            }
        }
        true
    }

    /// Whether this client advertised the posh-proto frame protocol — i.e. its
    /// `Tag::Init` carried a capability table with `CAP_PROTOCOL_VERSION`. A
    /// baseline (no-table) peer is never frame-capable, so it always receives
    /// raw `Tag::Output`.
    fn is_frame_capable(&self) -> bool {
        caps::find(&self.caps, caps::CAP_PROTOCOL_VERSION).is_some()
    }

    /// Construct this client's `FrameProducer` when the client is frame-capable.
    /// Idempotent: a bare re-`Init` (SIGCONT resume) keeps the existing producer
    /// (and its acked base) rather than resetting it. A baseline client (no cap
    /// table) never gets one and stays on `Tag::Output` — the only remaining
    /// version-skew axis now that the daemon-side gate is retired.
    fn maybe_enable_frames(&mut self) {
        if self.producer.is_none() && self.is_frame_capable() {
            self.producer = Some(FrameProducer::new(self.rows.max(1), self.cols.max(1)));
        }
    }

    /// Paced AND framed: a paced client without a producer (not frame
    /// capable) takes the raw `Tag::Output` path like any other.
    fn is_paced(&self) -> bool {
        self.pacing.is_some() && self.producer.is_some()
    }

    /// Framed but NOT paced: `broadcast_output` builds this client a visible
    /// frame per PTY read (and so a dump of its geometry).
    fn takes_per_read_frames(&self) -> bool {
        self.producer.is_some() && !self.is_paced()
    }

    /// When this client may next be sent a fresh visible frame, on the
    /// caller's clock: `None` when it is not paced, owes nothing, or still
    /// has bytes queued (`POLLOUT` will wake the loop for those). The ONE
    /// predicate behind `send_paced_frames` and `paced_poll_timeout`, so
    /// the two cannot disagree — a disagreement is a busy loop or a stale
    /// screen.
    fn paced_send_at(&self) -> Option<u64> {
        if !self.owes_paced_frame() || !self.write_buf.is_empty() {
            return None;
        }
        let producer = self.producer.as_ref()?;
        let last_fresh = self.pacing.as_ref().and_then(|p| p.last_fresh);
        Some(match last_fresh {
            None => 0,
            Some(at) if producer.acked_num() >= producer.last_visible_num() => at + PACED_FRAME_FLOOR_MS,
            Some(at) => at + PACED_ACK_WAIT_MS,
        })
    }

    /// A paced client that owes a visible frame (`send_paced_frames` will
    /// build it).
    fn owes_paced_frame(&self) -> bool {
        self.pacing.as_ref().is_some_and(|p| p.dirty)
    }

    /// For a paced client, record that a visible frame is owed and return
    /// `true`: the caller must not build one now. Any other client: `false`,
    /// and the caller takes today's path.
    fn owe_paced_frame(&mut self) -> bool {
        if !self.is_paced() {
            return false;
        }
        if let Some(p) = self.pacing.as_mut() {
            p.dirty = true;
        }
        true
    }

    /// The one entry point for "this client is owed a visible frame of
    /// `src`" (attach / regeometry replay, RESYNC keyframe, activity answer):
    /// a paced client is marked dirty and its frame waits for its next send
    /// opportunity; a framed one is queued the frame now. Returns whether a
    /// frame is queued or owed — `false` for a producer-less client, whose
    /// caller falls back to `Tag::Output`.
    fn request_frame_from(&mut self, src: &Terminal) -> bool {
        self.owe_paced_frame() || (self.producer.is_some() && self.build_frame_from(src))
    }

    /// Build this paced client's fresh visible frame from `src` now — for
    /// its current geometry (`build_frame_from`, which also records
    /// `visible_shaped_for`), with any v1 scrollback right behind it
    /// (posh#181 threading) unless it takes v2 history, which has its own
    /// opportunity (`history_send_at`) — and record when it was sent.
    fn send_paced_frame(&mut self, src: &Terminal, now: u64) {
        if !self.build_frame_from(src) {
            return;
        }
        if !self.has_history() {
            self.maybe_queue_scrollback(src);
        }
        let sent = self.producer.as_ref().map(FrameProducer::last_visible_num);
        if let Some(p) = self.pacing.as_mut() {
            p.dirty = false;
            p.last_was_screen = true;
            p.last_fresh = Some(now);
            if let Some(num) = sent {
                if p.sent_frames.len() == SENT_FRAME_LOG_CAP {
                    p.sent_frames.pop_front();
                }
                p.sent_frames.push_back((num, now));
            }
        }
    }

    /// This client's RFC 0009 v2 history cursor (`Pacing::history`).
    fn history(&self) -> Option<&HistoryCursor> {
        self.pacing.as_ref()?.history.as_ref()
    }

    fn history_mut(&mut self) -> Option<&mut HistoryCursor> {
        self.pacing.as_mut()?.history.as_mut()
    }

    /// Whether this client takes RFC 0009 v2 history.
    fn has_history(&self) -> bool {
        self.history().is_some()
    }

    /// This client's Init asked for the v2 extent (posh#225 Stage 4).
    fn wants_history_extent(&self) -> bool {
        caps::find(&self.caps, caps::CAP_SCROLLBACK2_EXTENT)
            .and_then(|c| caps::decode_scrollback2_extent_request(&c.payload))
            .is_some()
    }

    /// Refresh the cached extent (`Pacing::extent`) from the session
    /// terminal.
    fn note_history_extent(&mut self, term: &Terminal) {
        let wants = self.wants_history_extent();
        if let Some(p) = self.pacing.as_mut() {
            p.extent = p.history.and_then(|h| h.extent(term)).filter(|_| wants);
        }
    }

    /// The v2 resend floor: twice the measured ack latency (never under the
    /// ack wait), `HISTORY_RESEND_INITIAL_MS` before any sample, doubled per
    /// resend without progress.
    fn history_resend_after(&self) -> u64 {
        let Some(p) = self.pacing.as_ref() else {
            return HISTORY_RESEND_INITIAL_MS;
        };
        let base = p
            .acks
            .srtt_ms
            .map_or(HISTORY_RESEND_INITIAL_MS, |s| (2 * s).max(PACED_ACK_WAIT_MS));
        let resends = p.history.map_or(0, |h| h.resends());
        base << resends.min(HISTORY_RESEND_MAX_DOUBLINGS)
    }

    /// When this client may next be sent a history body from `term` — the
    /// history half of the one-predicate rule (`send_paced_frames` and
    /// `paced_poll_timeout` both ask it): the cursor's `next_due` within
    /// `HISTORY_WINDOW_ROWS` — not the frame floor — and `None` with bytes
    /// queued, so a due body, once queued, cannot busy-loop the poll.
    fn history_send_at(&self, term: &Terminal) -> Option<u64> {
        let h = self.history()?;
        if !self.write_buf.is_empty() || self.producer.is_none() {
            return None;
        }
        h.next_due(
            term.primary_scrollback_total(),
            self.history_resend_after(),
            HISTORY_WINDOW_ROWS,
        )
    }

    /// Queue one v2 body from `term` (the session terminal) — from the send
    /// cursor, or from the ack when the resend floor has passed — riding the
    /// newest visible frame number (RFC 0009 §2: an annotation; the producer
    /// does not advance), with the epoch ack beside it — and, to a viewport
    /// that asked, the extent as of this body (RFC 0009 §3.1), refreshing
    /// `Pacing::extent`.
    fn send_history_body(&mut self, term: &Terminal, now: u64) {
        let rto = self.history_resend_after();
        let flags = self.echo_flag | self.overlay_flag;
        let wants_extent = self.wants_history_extent();
        let (Some(producer), Some(p)) = (self.producer.as_ref(), self.pacing.as_mut()) else {
            return;
        };
        let Some(h) = p.history.as_mut() else {
            return;
        };
        let (epoch, body) = h.next_body(term, now, rto, HISTORY_WINDOW_ROWS);
        p.extent = wants_extent.then(|| h.extent(term)).flatten();
        p.last_was_screen = false;
        let mut entries = vec![caps::encode_scrollback2_ack(epoch)];
        entries.extend(p.extent.as_ref().map(caps::encode_scrollback2_extent));
        let bytes = ServerFrame {
            flags,
            caps: caps::own_table(&entries),
            frame_num: producer.current_num(),
            input_ack: 0,
            echo_ack: 0,
            body,
        }
        .encode();
        self.queue(Tag::Frame, &bytes);
    }

    /// [`queue_frame`] with its inputs derived from one source terminal, for
    /// the per-client sites (`request_frame_from`, `send_paced_frame`).
    /// `broadcast_output` deliberately keeps its batched form: it derives the
    /// snapshot, alt flag and dims once per broadcast and builds the dump once
    /// per distinct bounded geometry (one shared full dump for the rest),
    /// cloning them per client.
    ///
    /// The dump is built for the geometry of THIS client's terminal — what its
    /// ring-less mirror can show — not replayed from the whole scrollback ring,
    /// which made every frame ~1 MiB once the ring filled (posh#225). A client
    /// that has not reported a size yet, or one wider, narrower or shorter than
    /// `src`, still gets the full dump: a row count cannot reproduce the full
    /// replay there. The rule itself lives in `Terminal::dump_vt_mirror`;
    /// the geometry this dump is shaped for is recorded in
    /// `visible_shaped_for`.
    fn build_frame_from(&mut self, src: &Terminal) -> bool {
        self.note_visible_dump_shape(src);
        self.queue_frame(
            src.dump_vt_mirror(self.rows, self.cols),
            Snapshot::from_term(src),
            src.is_alt_screen(),
            (src.rows(), src.cols()),
        )
    }

    /// Produce a visible frame from the supplied screen state and queue it as
    /// `Tag::Frame`. Returns `false` (queuing nothing) when this client has no
    /// producer, so the caller falls back to `Tag::Output`.
    ///
    /// Reliable client (the default local path): reliable-as-degenerate (RFC 0008
    /// §3) — the socket delivers in order with no loss, so after queuing the frame
    /// we immediately `ack` it. The acked base is always the last frame, the next
    /// frame is a `Diff` against it (DumpDiff — the socket cannot negotiate a
    /// codec), and the producer's retransmit machinery idles. `input_ack`/
    /// `echo_ack` are inert (the socket input stream is itself reliable).
    ///
    /// Lossy client (the Phase 3 relay, `CAP_LOSSY`): NOT self-acked — the base
    /// advances only on a forwarded `Tag::FrameAck`, so each new frame supersedes
    /// the last unacked one (bounding the relay's retransmit buffer to O(1)). The
    /// codec is selected from the negotiated caps (`CAP_MORPH` ⇒ MorphDelta) and,
    /// with `CAP_BASE_SUM`, the diff base's checksum is stamped so the far client
    /// can verify its base before applying (mirror of `server.rs`).
    fn queue_frame(&mut self, dump: Vec<u8>, snapshot: Snapshot, alt: bool, dims: (u16, u16)) -> bool {
        // Read the lossy-mode inputs before borrowing `producer` mutably. A
        // reliable client leaves all three false ⇒ today's exact behavior.
        let lossy = self.lossy;
        // Withhold the immediate self-ack for a lossy client OR a coalescing local
        // client (posh#137): both advance their diff base only on a `Tag::FrameAck`.
        // But `use_morph`/`stamp_base_sum` stay gated on `lossy` ONLY — a coalescing
        // client keeps DumpDiff + no base_sum (plain local semantics).
        let withhold = self.lossy || self.coalescing();
        let use_morph = self.uses_morph();
        let stamp_base_sum = lossy && caps::find(&self.caps, caps::CAP_BASE_SUM).is_some();
        // RFC 0013 §5.2: the activity label rides this visible frame only
        // when it changed since this client last received it (or never did).
        let mut frame_caps: Vec<caps::Cap> = if self.answer_due() {
            self.activity_sent = self.activity_now.clone();
            let mut entries: Vec<caps::Cap> =
                self.activity_now.iter().map(caps::encode_session_activity).collect();
            // The kind (id 20) rides the first activity-bearing frame once:
            // a session never changes kind, so there is nothing to refresh.
            if !self.kind_sent {
                self.kind_sent = true;
                entries.push(caps::encode_session_kind(self.kind));
            }
            // RFC 0016 §2: the push-cmd offer, the same placement, once.
            if self.wants_push_cmd && !self.push_offered {
                self.push_offered = true;
                entries.push(caps::encode_push_cmd());
            }
            entries
        } else {
            Vec::new()
        };
        // RFC 0009 §1.1 (posh#225 Stage 3): a v2 viewport adopts its epoch
        // from the server's SCROLLBACK2 entry on the first frame it gets, so
        // every visible frame to it carries one, as `server_loop`'s do.
        if let Some(epoch) = self.history().and_then(HistoryCursor::epoch) {
            frame_caps.push(caps::encode_scrollback2_ack(epoch));
        }
        // RFC 0009 §3.1 (posh#225 Stage 4): beside it, the cached extent, to
        // a viewport that asked (`Pacing::extent`; refreshed per send pass,
        // on a resize and when the cursor opens).
        let extent = self.pacing.as_ref().and_then(|p| p.extent);
        if let Some(x) = extent.filter(|_| self.wants_history_extent()) {
            frame_caps.push(caps::encode_scrollback2_extent(&x));
        }
        let encoded = match self.producer.as_mut() {
            None => return false,
            Some(producer) => {
                producer.advance_visible(dump, snapshot, alt, dims, 0);
                let mut body = producer.encode_visible(use_morph);
                // RFC 0006: stamp the diff base's checksum so a lossy client can
                // confirm it holds the same base before applying (mirror
                // server.rs:871-883). Diff only — a Morph base is a snapshot, not
                // the client's held dump bytes, so the byte checksum does not
                // apply there.
                if stamp_base_sum {
                    if let Some(acked) = producer.acked_dump() {
                        if let FrameBody::Diff { base_sum, .. } = &mut body {
                            *base_sum = Some(base_checksum(acked));
                        }
                    }
                }
                let frame_num = producer.current_num();
                let bytes = ServerFrame {
                    // FDR 0006: the active pty's ECHO state rides every
                    // frame (RFC 0008 §2 keeps acks 0 here; flags are real).
                    // FDR 0008: FLAG_OVERLAY rides too while the shell overlay
                    // is up (posh#178).
                    flags: self.echo_flag | self.overlay_flag,
                    caps: caps::own_table(&frame_caps),
                    frame_num,
                    input_ack: 0,
                    echo_ack: 0,
                    body,
                }
                .encode();
                // Reliable client: self-ack now (degenerate loss machinery). Lossy
                // or coalescing client: withhold — its base advances only on
                // `Tag::FrameAck` (posh#137).
                if !withhold {
                    producer.ack(frame_num);
                }
                bytes
            }
        };
        if self.regeometry_keyframe == Some(RegeometryKeyframe::Owed) {
            let built = self.producer.as_ref().map(FrameProducer::last_visible_num);
            self.regeometry_keyframe = built.map(RegeometryKeyframe::Sent);
        }
        // Coalesce the queued bytes for a coalescing client (posh#137): if the
        // previously-queued visible frame is still fully un-sent at the tail of
        // `write_buf`, truncate it and append the freshly-encoded latest frame in
        // its place (it re-encodes against the same acked base, so it is a complete
        // superset — no lost content). Otherwise (not coalescing, no pending frame,
        // or the tail is not a clean pending frame) append normally. `self.queue`
        // clears `pending_frame_start`, so compute the anchor offset BEFORE the
        // append and (re)set it AFTER — that keeps the anchor pointing only at THIS
        // visible frame, never across an intervening non-visible append.
        if self.coalescing() {
            if let Some(start) = self.pending_frame_start {
                if start <= self.write_buf.len() {
                    self.write_buf.truncate(start);
                }
            }
            let start = self.write_buf.len();
            self.queue(Tag::Frame, &encoded);
            self.pending_frame_start = Some(start);
        } else {
            self.queue(Tag::Frame, &encoded);
        }
        true
    }

    /// Apply a `Tag::FrameAck` from a client whose frames the daemon does NOT
    /// self-ack: a lossy relay client (RFC 0008 §3) OR a `CAP_COALESCE` local
    /// client (posh#137). Advances this client's producer base to the acked frame —
    /// the base-advance a reliable client gets from the immediate self-ack in
    /// `queue_frame`. The `FRAME_ACK_RESYNC` flag additionally drops the base so
    /// the next frame is a forced `Full` keyframe (base-sum divergence recovery).
    /// The `FRAME_ACK_COALESCE_OFF` flag (coalescing clients only) toggles
    /// write-buffer coalescing off/on at runtime, reverting the client to today's
    /// self-ack+append path — so a wedged coalescing client can be escaped from the
    /// command palette without dropping the session. A reliable (neither lossy nor
    /// coalescing) client, a malformed payload, or a producerless client is a
    /// no-op. Extracted (like `apply_init`) so the daemon-loop arm and the inline
    /// tests drive one path.
    /// Returns `true` when the ack carried `FRAME_ACK_RESYNC` and the base was
    /// dropped — the caller ([`handle_frame_ack`]) owes the client an immediate
    /// recovering `Full` keyframe. A RESYNC also releases a paced client's
    /// ack wait: it rejected the outstanding frame, so that ack is not coming.
    fn apply_frame_ack(&mut self, payload: &[u8]) -> bool {
        // `Tag::FrameAck` is a not-self-acked verb: a reliable client self-acks in
        // `queue_frame` and never sends it, so ignore it here — that keeps a
        // reliable client's producer state provably untouched by this path. Gated
        // on the ADVERTISED cap (`self.coalesce`, not `coalescing()`): a toggle-OFF
        // ack must still be processed to flip the runtime state back.
        if !self.lossy && !self.coalesce {
            return false;
        }
        let Some((acked, flags)) = ipc::decode_frame_ack(payload) else {
            return false;
        };
        // Runtime coalescing toggle (posh#137). Only a `CAP_COALESCE` client can
        // flip it — a lossy relay ack must never touch it. Clearing the anchor on
        // turn-OFF keeps the drain/queue bookkeeping consistent with the client's
        // reverted self-ack+append behavior.
        if self.coalesce {
            self.coalesce_off = flags & ipc::FRAME_ACK_COALESCE_OFF != 0;
            if self.coalesce_off {
                self.pending_frame_start = None;
            }
        }
        let Some(producer) = self.producer.as_mut() else {
            return false;
        };
        if let Some(sb_total) = producer.ack(acked) {
            self.acked_sb_total = self.acked_sb_total.max(sb_total);
        }
        // A morph client owed a regeometry keyframe (posh#225): an ack below
        // it may have restored a PRE-resize frame as the base, which the
        // client's new-size mirror cannot be morphed from — drop it again, so
        // the next visible frame is a `Full` (scrollback bookkeeping above
        // advanced as usual). Once the keyframe or a later frame is acked AND
        // held as the base, the client holds a new-geometry dump: done.
        match self.regeometry_keyframe {
            Some(RegeometryKeyframe::Sent(keyframe)) if acked >= keyframe => {
                if producer.has_acked_base() {
                    self.regeometry_keyframe = None;
                }
            }
            Some(_) => producer.drop_acked_base(),
            None => {}
        }
        if flags & ipc::FRAME_ACK_RESYNC != 0 {
            producer.drop_acked_base();
            if let Some(p) = self.pacing.as_mut() {
                p.last_fresh = None;
                p.sent_frames.clear();
            }
            return true;
        }
        false
    }

    /// Record a frame ack's arrival for a paced client (posh#225 Stage 3.0);
    /// `acked_before` is the producer's `acked_num()` before it applied. An
    /// ack that advanced it samples the round trip of the newest logged
    /// frame it confirms, and drops every logged frame it confirms — not
    /// just the newest frame's ack, which a long RTT under steady output
    /// never sees. A repeated ack or a RESYNC is no sample.
    fn note_frame_ack(&mut self, acked_before: Option<u64>, now: u64) {
        let (Some(before), Some(producer)) = (acked_before, self.producer.as_ref()) else {
            return;
        };
        let acked = producer.acked_num();
        let Some(p) = self.pacing.as_mut() else {
            return;
        };
        p.acks.last_ack_at = Some(now);
        if acked <= before {
            return;
        }
        let mut confirmed = None;
        while let Some(&(num, queued_at)) = p.sent_frames.front() {
            if num > acked {
                break;
            }
            confirmed = Some(queued_at);
            p.sent_frames.pop_front();
        }
        if let Some(queued_at) = confirmed {
            p.acks.record(now.saturating_sub(queued_at));
        }
    }

    /// Whether this client advertised `CAP_SCROLLBACK` (RFC 0002 §1) on its
    /// `Tag::Init` — i.e. it understands `FrameBody::Scrollback` and wants
    /// scrolled-off rows synced to its local ring. Socket caps are Init-only and
    /// persistent (unlike the UDP path's per-message advertisement), so this is a
    /// stable per-connection property.
    fn wants_scrollback(&self) -> bool {
        caps::find(&self.caps, caps::CAP_SCROLLBACK).is_some()
    }

    /// Produce a scrollback-growth frame from the daemon terminal and queue it as
    /// a SEPARATE `Tag::Frame` — mirroring the roaming server's scrollback body
    /// (server.rs). Meant to ride immediately AFTER this client's visible frame:
    /// that frame advanced the acked base, and the scrollback frame threads off
    /// it (its `base` is the confirmed visible frame, and it inherits that visible
    /// dump so the diff-base chain stays unbroken across the interleaved frames).
    ///
    /// Returns `false` (queuing nothing) unless every gate holds: the client
    /// wants scrollback, the terminal is on its primary screen (the alt screen
    /// has no scrollback), a visible baseline is confirmed (#95 — a Scrollback
    /// body carries the acked visible dump forward as its diff base), and the
    /// daemon's monotonic scrollback total has grown past this client's
    /// floor/ack. Reliable-as-degenerate (RFC 0008 §3): the frame is self-acked at
    /// once, so `acked_sb_total` tracks the shipped total immediately.
    fn maybe_queue_scrollback(&mut self, term: &Terminal) -> bool {
        if !self.wants_scrollback() || term.is_alt_screen() {
            return false;
        }
        let cur_sb_total = term.primary_scrollback_total();
        let floor = self.acked_sb_total.max(self.sb_floor);
        if cur_sb_total <= floor {
            return false;
        }
        let has_base = self
            .producer
            .as_ref()
            .is_some_and(FrameProducer::has_acked_base);
        if !has_base {
            return false;
        }
        // Whether to withhold the scrollback frame's self-ack, read BEFORE the
        // mutable `producer` borrow below (posh#137). A lossy OR coalescing client
        // is NOT self-acked — its base advances only on the client's
        // `Tag::FrameAck`, mirroring the visible-frame path in `queue_frame`.
        let withhold = self.lossy || self.coalescing();
        let producer = self.producer.as_mut().expect("has_base implies Some");
        // posh#181: thread off the visible frame queued just before this one
        // (`broadcast_output` orders them so), NOT the acked base. A
        // not-self-acked client (lossy relay/bridge, coalescing) applies that
        // visible frame first, and its RFC 0002 §3 rule accepts a scrollback
        // body only at `base == applied_num` — anchored at the older acked
        // base, every scrollback frame was rejected and the ring never grew.
        // For the self-acked reliable client the two anchors coincide.
        producer.advance_scrollback_after_visible(cur_sb_total);
        // The rows that entered scrollback since this client's floor/ack, bounded
        // by what the ring still holds. Work in ring positions (newest-anchored):
        // `grown` rows entered since this frame's coverage and sit at the tail — 0
        // on the reliable socket, where produced == acked — so `end` is the whole
        // ring; `want` (rows since the floor/ack) is capped to what the ring still
        // holds, since evicted older rows are gone by design.
        //
        // mirror of `server_loop`'s v1 scrollback body (`remote/server.rs`) — keep in sync.
        let ring_len = term.primary_scrollback_len();
        let frame_sb_total = producer.current_sb_total();
        let grown = cur_sb_total.saturating_sub(frame_sb_total) as usize;
        let end = ring_len.saturating_sub(grown);
        let want = frame_sb_total.saturating_sub(floor) as usize;
        let appended = want.min(end);
        let start = end - appended;
        let rows: Vec<Vec<u8>> = (start..end)
            .map(|i| term.dump_scrollback_row(i).unwrap_or_default())
            .collect();
        let frame_num = producer.current_num();
        // `base` names the visible frame this body follows (its dump is what
        // the slot above inherited), so the client at that frame applies it.
        let body = FrameBody::Scrollback {
            base: producer.last_visible_num(),
            rows,
        };
        let bytes = ServerFrame {
            flags: self.echo_flag | self.overlay_flag,
            caps: caps::own_table(&[]),
            frame_num,
            input_ack: 0,
            echo_ack: 0,
            body,
        }
        .encode();
        // Reliable client self-acks the scrollback frame at once (produced ==
        // acked); a lossy OR coalescing client is NOT self-acked (see `withhold`
        // above, computed before the `producer` borrow). Missing the coalescing
        // case here would advance the base server-side without the client's ack,
        // defeating the CAP_COALESCE invariant. The scrollback bytes still go
        // through `self.queue` (never coalesced away — they carry unique history).
        if !withhold {
            if let Some(sb_total) = producer.ack(frame_num) {
                self.acked_sb_total = self.acked_sb_total.max(sb_total);
            }
        }
        self.queue(Tag::Frame, &bytes);
        true
    }
}

// The `$POSH_SESSION_FRAMES` daemon-side frame-emission gate (RFC 0008 §6's
// rollback switch) was RETIRED on 2026-08-25 (posh#171 item 2, local/remote
// parity): the roaming server never had one, frames had been default-on
// fleet-wide, and the version-skew protection it duplicated is the client's
// own capability advertisement — a client without `CAP_PROTOCOL_VERSION` on
// its Init still gets raw `Tag::Output` (`is_frame_capable`). The env var is
// now ignored; rollback to Architecture A is the bootstrap-side `POSH_RELAY=0`.

/// The fields both backlog log lines carry (posh#131 diagnosis): stalled vs
/// bursty, and `paced=` — a paced viewport holds at most one frame pair
/// (RFC 0008 §3.2), so a paced client at the high-water mark or dropped is
/// a posh bug, not a slow reader. Telling the viewport why it was dropped
/// is posh#226. Then the ack latency (posh#225 Stage 3.0): `none` and 0 for
/// a client that is not paced, which records none.
fn backlog_log_fields(c: &ClientConn, now: u64) -> String {
    format!(
        "fd={} backlog={} drained_total={} last_drain_age_ms={} paced={} {}",
        c.stream.as_raw_fd(),
        c.write_buf.len(),
        c.bytes_drained,
        now.saturating_sub(c.last_drain_ms),
        u8::from(c.is_paced()),
        c.pacing.as_ref().map_or_else(
            || AckLatency::default().log_fields(now),
            |p| p.acks.log_fields(now)
        ),
    )
}

/// The throttled `paced ack latency` line for one client (posh#225 Stage
/// 3.0), when one is due — `None` for a client that is not paced — and
/// records that it was written. `new=` counts the samples since the last.
fn ack_latency_log_line(c: &mut ClientConn, now: u64) -> Option<String> {
    let new = c.pacing.as_mut()?.acks.take_log_line(now)?;
    Some(format!("paced ack latency {} new={new}", backlog_log_fields(c, now)))
}

/// Broadcasts a PTY-output chunk to every attached client: a posh-proto
/// `ServerFrame` (`Tag::Frame`) for each frame-capable client, the raw `bcast`
/// bytes (`Tag::Output`) for the rest. The snapshot frame inputs are derived
/// once from `term` and cloned per producer — each client diffs against its OWN
/// acked base — and the dump once per DISTINCT bounded client geometry, plus
/// at most one shared full dump for every geometry `dump_vt_mirror` cannot
/// bound, since each client's dump is built for its own terminal
/// (`ClientConn::build_frame_from`, posh#225). Both ONLY when at least one
/// non-paced client is frame-capable (`takes_per_read_frames`), so a session
/// with none pays exactly today's cost and emits exactly today's `Tag::Output`
/// bytes (the gate-off invariant). A paced client (posh#225, RFC 0008 §3.2)
/// takes none of this: it is only marked dirty, and `send_paced_frames` builds
/// its frame later.
fn broadcast_output(clients: &mut [ClientConn], term: &Terminal, bcast: &[u8]) {
    let producers = clients.iter().filter(|c| c.takes_per_read_frames()).count();
    let frame_inputs = (producers > 0).then(|| {
        (
            Snapshot::from_term(term),
            term.is_alt_screen(),
            (term.rows(), term.cols()),
        )
    });
    // Clients that would get the same dump share it, keyed by the geometry it
    // is shaped for (`note_visible_dump_shape`): `Some(size)` for a bounded
    // geometry, and ONE `None` key for every geometry `dump_vt_mirror` cannot
    // bound (wider, narrower, shorter, not yet reported), since each would
    // build the same full ring dump. A dump is copied only for a LATER client
    // that shares its key; the last client with a key takes it by move, so an
    // unshared dump (a lone producer, or a geometry no one else has) is never
    // copied.
    type DumpKey = Option<(u16, u16)>;
    // Each client's key in client order; `None` for a client without a
    // producer, and for a paced one: its `visible_shaped_for` must keep
    // naming the dump it actually holds.
    let keys: Vec<Option<DumpKey>> = clients
        .iter_mut()
        .map(|c| c.takes_per_read_frames().then(|| c.note_visible_dump_shape(term)))
        .collect();
    let mut held: Vec<(DumpKey, Vec<u8>)> = Vec::new();
    let mut dump_for = |i: usize, c: &ClientConn| -> Vec<u8> {
        let key = keys[i].expect("only a producer client takes a dump");
        let dump = match held.iter().position(|(k, _)| *k == key) {
            Some(at) => held.swap_remove(at).1,
            None => term.dump_vt_mirror(c.rows, c.cols),
        };
        if keys[i + 1..].contains(&Some(key)) {
            held.push((key, dump.clone()));
        }
        dump
    };
    for (i, c) in clients.iter_mut().enumerate() {
        // posh#239: a connection whose Init is not yet applied is sent
        // nothing — not even `Tag::Output` (a frame viewport would paint it
        // raw, and the relay would take it for a frames-off daemon). Its
        // Init's attach replay covers what it missed.
        if !c.initialized() || c.owe_paced_frame() {
            continue;
        }
        let produced = match &frame_inputs {
            Some((snap, alt, dims)) => {
                // A producer-less client gets an empty dump, but is still
                // called: `queue_frame` marks a due activity answer sent
                // before it looks at the producer.
                let dump = if c.producer.is_some() { dump_for(i, c) } else { Vec::new() };
                c.queue_frame(dump, snap.clone(), *alt, *dims)
            }
            None => false,
        };
        if !produced {
            c.queue(Tag::Output, bcast);
        } else {
            // Scrollback growth rides as a SEPARATE frame AFTER the visible one
            // (RFC 0002): the visible frame just advanced this client's acked
            // base, so the scrollback frame threads off it. A no-op unless the
            // client wants scrollback and the terminal grew primary rows.
            c.maybe_queue_scrollback(term);
        }
    }
}

/// The daemon-loop `Tag::FrameAck` arm, extracted so the inline tests drive
/// the exact production path (like `apply_init`/`apply_frame_ack`): apply the
/// ack against `c`'s producer, and when it carried `FRAME_ACK_RESYNC` ship the
/// recovering `Full` keyframe IMMEDIATELY from `src` (the active broadcast
/// source: the overlay terminal while one is up, else the live session).
///
/// The immediacy is load-bearing (the mux-session `sc list`/vim wedge): the
/// resync's contract is "drop the base so the next frame is a Full", but on a
/// static screen no next frame ever comes — the client already rejected the
/// outstanding diffs (base-behind basemis, #95) and the relay/bridge cleared
/// its held frame on the same RESYNC, so without this forced frame both ends
/// sit silent forever. Mirrors the single-peer server's `force_frame = true`
/// ("ships it even if the screen is static", server.rs).
///
/// A paced client's recovering `Full` is instead its next paced frame
/// (posh#225, RFC 0008 §3.2), and waits only for an empty buffer:
/// `apply_frame_ack` released its ack wait.
///
/// `now` (the caller's clock) times a paced client's ack
/// (`note_frame_ack`, posh#225 Stage 3.0); it changes no stream.
fn handle_frame_ack(c: &mut ClientConn, payload: &[u8], src: &Terminal, now: u64) {
    let acked_before = c.producer.as_ref().map(FrameProducer::acked_num);
    let resync = c.apply_frame_ack(payload);
    c.note_frame_ack(acked_before, now);
    if resync {
        c.request_frame_from(src);
    }
}

/// The attach / regeometry replay for one client (github #16; posh#225):
/// a frame for its current geometry (`request_frame_from` — for a paced
/// client its next paced frame, which also ends any regeometry debt), else
/// the flat dump for a baseline client, which pays only that.
fn queue_replay(c: &mut ClientConn, src: &Terminal) {
    if !c.request_frame_from(src) {
        c.queue(Tag::Output, &src.dump_vt_flat());
    }
}

/// RFC 0013 §5.2 / RFC 0016 §2: an answer that is due and was not on a
/// replay (a bridge's `Tag::ClientCaps` request, a label change on an idle
/// screen) rides a frame of its own rather than wait for output — for a
/// paced client, its next paced frame (a title that changes every line would
/// otherwise bypass pacing during a flood). Runs before `send_paced_frames`.
fn queue_due_answers(clients: &mut [ClientConn], src: &Terminal) {
    for c in clients.iter_mut().filter(|c| c.answer_due()) {
        c.request_frame_from(src);
    }
}

/// Force every frame-capable client's producer to emit a fresh `Full` keyframe
/// on its next frame, then broadcast `src`. Called on both edges of the
/// escape-to-shell overlay (FDR 0008): the broadcast source swaps wholesale
/// (session↔overlay), so a `Diff` against each client's acked base would be a
/// full-screen diff — correct but huge. Dropping the acked base makes the next
/// `encode_visible` a `Full` (mirrors the remote server's `force_frame = true`).
/// `bcast` is the raw fallback for any baseline (non-framing) client.
fn broadcast_source_swap(clients: &mut [ClientConn], src: &Terminal, bcast: &[u8]) {
    for c in clients.iter_mut() {
        if let Some(p) = c.producer.as_mut() {
            p.drop_acked_base();
        }
    }
    broadcast_output(clients, src, bcast);
}

/// The paced send pass (posh#225, RFC 0008 §3.2): every paced client with a
/// send opportunity at `now` gets ONE body — a fresh visible frame built
/// from `src` as it is now (screens produced since its last frame were
/// never built), or a v2 history body from `history`. When both are due,
/// the kind that did not go last goes (`server_loop`'s coin; the first tie
/// is the screen's); the other goes next iteration, after the drain.
/// `history` is the session terminal, `None` while the escape overlay is up
/// (as `server_loop` pauses history for it), which also freezes each
/// client's cached v2 extent (`Pacing::extent`). Runs at the end of a loop
/// iteration; `now` is a parameter so tests drive a fake clock.
fn send_paced_frames(clients: &mut [ClientConn], src: &Terminal, history: Option<&Terminal>, now: u64) {
    for c in clients.iter_mut() {
        if let Some(t) = history {
            c.note_history_extent(t);
        }
        let screen = c.paced_send_at().is_some_and(|at| now >= at);
        let rows = history.filter(|t| c.history_send_at(t).is_some_and(|at| now >= at));
        let last_was_screen = c.pacing.as_ref().is_some_and(|p| p.last_was_screen);
        match (screen, rows) {
            (true, Some(t)) if last_was_screen => c.send_history_body(t, now),
            (true, _) => c.send_paced_frame(src, now),
            (false, Some(t)) => c.send_history_body(t, now),
            (false, None) => {}
        }
    }
}

/// The daemon's poll timeout: milliseconds until the nearest paced send
/// opportunity — a visible frame, or a history body from `history` (the
/// session terminal, `None` while the escape overlay is up) — `0` when one
/// is already due, `-1` (block) when no paced client owes either — never a
/// busy-wait.
fn paced_poll_timeout(clients: &[ClientConn], history: Option<&Terminal>, now: u64) -> i32 {
    clients
        .iter()
        .flat_map(|c| [c.paced_send_at(), history.and_then(|t| c.history_send_at(t))])
        .flatten()
        .map(|at| at.saturating_sub(now))
        .min()
        .map_or(-1, |ms| i32::try_from(ms).unwrap_or(i32::MAX))
}

/// Before `Exit`: every paced client that still owes a frame gets it now,
/// whatever its pacing — the session's last screen must not be lost.
fn flush_paced_frames(clients: &mut [ClientConn], src: &Terminal, now: u64) {
    for c in clients.iter_mut().filter(|c| c.owes_paced_frame()) {
        c.send_paced_frame(src, now);
    }
}

/// How the daemon handles the app's model-produced terminal-query replies
/// (kitty, DA, DSR), decided from the attached clients (RFC 0010).
///
/// Kitty-protocol detection is by reply PRESENCE, not value. The app enables the
/// protocol when a `CSI ? <flags> u` reply comes back at all, then pushes the
/// flags it wants (posh-term records them; FDR 0013 mirrors them outward). The
/// daemon therefore never substitutes a value into the reply; it answers with
/// the model's own current flags, and the client capability only gates whether
/// the kitty reply is spoken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryPolicy {
    /// Write the model's responses verbatim (kitty reply + DA/DSR): no clients,
    /// or every frame client's real terminal supports kitty.
    Answer,
    /// Write nothing: a legacy `Tag::Output` client is attached, whose real
    /// terminal receives the raw query and answers it (a daemon answer too would
    /// double-reply).
    Silent,
    /// Answer DA/DSR but SUPPRESS the kitty reply: every client is a frame
    /// client but at least one real terminal does not support the kitty
    /// keyboard protocol, so the daemon must not claim support the terminal
    /// cannot deliver (the app would then encode keys the terminal can't send).
    SuppressKitty,
}

/// RFC 0010: pick the query-reply policy from the attached clients. The client
/// capability is a GATE (does the real terminal speak kitty?), not a value.
fn query_policy(clients: &[ClientConn]) -> QueryPolicy {
    // posh#239: a connection whose Init is not yet applied is sent no output,
    // so its terminal never sees the query; it neither answers nor votes.
    let attached = || clients.iter().filter(|c| c.initialized());
    if attached().next().is_none() {
        return QueryPolicy::Answer; // model is authoritative with no client
    }
    if attached().any(|c| c.producer.is_none()) {
        return QueryPolicy::Silent; // a legacy client's real terminal answers
    }
    // All frame clients: the kitty reply is spoken only if every real terminal
    // supports it (advertised CAP_KITTY_KEYBOARD). Absence ⇒ suppress kitty.
    let all_kitty = attached().all(|c| caps::find(&c.caps, caps::CAP_KITTY_KEYBOARD).is_some());
    if all_kitty {
        QueryPolicy::Answer
    } else {
        QueryPolicy::SuppressKitty
    }
}

/// RFC 0010: drop the kitty-keyboard query reply (`CSI ? <digits> u`) from a
/// response buffer, leaving every other response (DA `…c`, DSR `…R`) intact.
/// Used for [`QueryPolicy::SuppressKitty`]: the app must conclude "no kitty
/// support" (no `CSI ? u` reply) while still getting its device-attribute and
/// cursor-position replies. Only the exact `\x1b[?<digits>u` form is removed.
fn strip_kitty_reply(responses: &[u8]) -> Vec<u8> {
    const PREFIX: &[u8] = b"\x1b[?";
    let mut out = Vec::with_capacity(responses.len());
    let mut i = 0;
    while i < responses.len() {
        if responses[i..].starts_with(PREFIX) {
            let mut j = i + PREFIX.len();
            while j < responses.len() && responses[j].is_ascii_digit() {
                j += 1;
            }
            if j < responses.len() && responses[j] == b'u' && j > i + PREFIX.len() {
                i = j + 1; // skip the whole kitty reply
                continue;
            }
        }
        out.push(responses[i]);
        i += 1;
    }
    out
}

/// The session's RFC 0013 §5 activity label now: the terminal title each
/// call, the foreground process re-probed at most every PROBE_INTERVAL_MS
/// (`process` / `probed_at` are the loop's cache of it).
fn current_activity(
    pty_fd: RawFd,
    term: &Terminal,
    process: &mut String,
    probed_at: &mut u64,
) -> caps::SessionActivity {
    let t = util::now_ms();
    if t.saturating_sub(*probed_at) >= super::activity::PROBE_INTERVAL_MS {
        *probed_at = t;
        *process = crate::pty::foreground_command(pty_fd).unwrap_or_default();
    }
    caps::SessionActivity {
        process: process.clone(),
        title: term.title().to_string(),
    }
}

/// The terminal a client should render: the escape overlay's screen while one is
/// up (FDR 0008), else the live session. The broadcast source AND a
/// (re)attaching client's replay must agree on this — a client that attaches or
/// SIGCONT-resumes mid-overlay has to base on the overlay screen, not the live
/// session underneath (else it renders the session until the next overlay
/// output — indefinite at an idle prompt — and a baseline client is corrupted by
/// overlay deltas applied on a session base).
fn active_source<'a>(overlay_term: Option<&'a Terminal>, term: &'a Terminal) -> &'a Terminal {
    overlay_term.unwrap_or(term)
}

/// Substituted for RIS in the broadcast: the model performed a full reset,
/// so push the outer terminal's shared modes back to defaults without
/// letting it leave the alternate screen the client pinned it to (a raw
/// RIS would switch the outer terminal to its primary buffer — the user's
/// shell — and clear it). DECSTR covers cursor/charsets/SGR/region/keypad
/// and the kitty key stack; the explicit resets cover what DECSTR leaves
/// (mouse, paste, focus, alternate scroll, cursor blink/visibility,
/// DECCKM/reverse-video/autorepeat/LNM/insert, a pending synchronized
/// update, dynamic colors). A repaint of the (now empty) model screen
/// follows from the caller.
const RIS_SUBSTITUTE: &[u8] = b"\x1b[!p\
    \x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?9l\x1b[?1005l\x1b[?1006l\x1b[?1016l\
    \x1b[?2004l\x1b[?1004l\x1b[?1007l\x1b[?12l\x1b[?25h\x1b[?1l\x1b[?5l\x1b[?8h\
    \x1b[?2026l\x1b>\x1b[20l\x1b[4l\x1b]104\x07\x1b]110\x07\x1b]111\x07\x1b]112\x07";

/// Rebuilds a DECSET/DECRST sequence with the alt-screen modes (47/1047/
/// 1049) stripped, so co-set modes still reach the outer terminal (e.g.
/// `CSI ? 1049 ; 2004 h` forwards as `CSI ? 2004 h`). Returns None when
/// nothing remains or the held bytes aren't the plain `ESC [ ? params h/l`
/// shape (interleaved C0s, C1 CSI restarts); dropping the sequence whole
/// is safe because the model-faithful repaint follows either way.
fn strip_alt_screen_params(seq: &[u8]) -> Option<Vec<u8>> {
    let body = seq.strip_prefix(b"\x1b[?")?;
    let (&final_byte, params) = body.split_last()?;
    if !matches!(final_byte, b'h' | b'l') {
        return None;
    }
    let mut kept: Vec<&[u8]> = Vec::new();
    for part in params.split(|&b| b == b';') {
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        // Match numerically so leading zeros ("0047") can't sneak through.
        let n: u32 = std::str::from_utf8(part).ok()?.parse().unwrap_or(0);
        if !matches!(n, 47 | 1047 | 1049) {
            kept.push(part);
        }
    }
    if kept.is_empty() {
        return None;
    }
    let mut out = b"\x1b[?".to_vec();
    out.extend_from_slice(&kept.join(&b';'));
    out.push(final_byte);
    Some(out)
}

/// Virtualizes the application's screen switches in the raw output
/// broadcast.
///
/// Attached clients hold the outer terminal on ITS alternate screen for
/// the whole attach, so detach can restore the user's shell exactly as it
/// was. The inner application's own switch sequences (DECSET/DECRST
/// 47/1047/1049) and RIS must therefore never reach the outer terminal
/// raw: each is excised from the stream and replaced with a repaint of the
/// newly active screen generated from the daemon's terminal model.
///
/// Bytes are held back while the parser is mid-escape/CSI (the only states
/// that can complete into a switch), which also keeps sequences split
/// across PTY reads from being forwarded in halves.
#[derive(Default)]
struct ScreenSwitchFilter {
    held: Vec<u8>,
}

/// Cap on bytes held back mid-sequence; see the flush in `feed`.
const MAX_HELD: usize = 4096;

impl ScreenSwitchFilter {
    /// Feeds one PTY chunk through the model and appends the broadcast
    /// bytes (raw passthrough with switches substituted) to `out`.
    fn feed(&mut self, term: &mut Terminal, chunk: &[u8], out: &mut Vec<u8>) {
        // Fast path: nothing held, parser at rest, and no byte that could
        // begin an escape sequence (0x1b, or 0x9b as a raw C1 CSI).
        if self.held.is_empty()
            && !term.mid_escape()
            && !chunk.iter().any(|&b| b == 0x1b || b == 0x9b)
        {
            term.process(chunk);
            out.extend_from_slice(chunk);
            return;
        }
        for &b in chunk {
            self.held.push(b);
            term.process(&[b]);
            if let Some(kind) = term.take_screen_switch() {
                let seq = std::mem::take(&mut self.held);
                match kind {
                    ScreenSwitch::Reset => out.extend_from_slice(RIS_SUBSTITUTE),
                    ScreenSwitch::Alt => {
                        if let Some(rest) = strip_alt_screen_params(&seq) {
                            out.extend_from_slice(&rest);
                        }
                    }
                }
                out.extend_from_slice(&term.dump_screen_switch());
            } else if !term.mid_escape() {
                out.append(&mut self.held);
            } else if self.held.len() > MAX_HELD {
                // A real switch sequence is ~10 bytes; an escape this long
                // is garbage that can't be excised later anyway. Flush it
                // so a malicious stream can't grow the hold buffer.
                out.append(&mut self.held);
            }
        }
    }
}

/// Where this session is, by ADR 0008's cascade, from the facts a daemon
/// holds: its child's kernel cwd, the shell's OSC 7 report, its own start
/// directory, `$HOME`. No caller cwd — a daemon asks on nobody's behalf.
fn daemon_cwd(child_pid: libc::pid_t, term: &Terminal, start: &str) -> super::cwd::Resolved {
    let kernel = crate::pty::process_cwd(child_pid);
    let home = std::env::var("HOME").ok();
    super::cwd::session_cwd(super::cwd::Facts {
        caller: None,
        kernel: kernel.as_deref(),
        osc7: Some(term.pwd()).filter(|p| !p.is_empty()),
        start,
        home: home.as_deref(),
    })
}

/// RFC 0016 §4.1: the token of a push-cmd request in `table` that this daemon
/// has not served yet, recording it in `served` (which keeps the 64 most
/// recent). `None` for no request, a malformed one, or a repeat.
fn take_push_request(table: &[caps::Cap], served: &mut Vec<u64>) -> Option<u64> {
    const SERVED_KEPT: usize = 64;
    let token = caps::decode_push_cmd_request(&caps::find(table, caps::CAP_PUSH_CMD_REQUEST)?.payload)?;
    if served.contains(&token) {
        return None;
    }
    served.push(token);
    if served.len() > SERVED_KEPT {
        served.drain(..served.len() - SERVED_KEPT);
    }
    Some(token)
}

/// RFC 0016 §4.2: create the pushed session — anonymous, the next auto-id in
/// `group`, running `$POSH_ESCAPE_CMD` (or the login shell) in `cwd`, with
/// `token` already served. Returns its name.
fn create_pushed_session(group: &str, cwd: &str, token: u64) -> Result<String> {
    let cfg = Config::new(group)?;
    let name = session::next_autoid(&cfg)?;
    if !ensure_session_in(&cfg, &name, escape_command(), SessionKind::Anonymous, Some(cwd), Some(token))? {
        // Another creator took the slot between the scan and the bind.
        return Err(Error::Msg(format!("{name} already exists")));
    }
    Ok(name)
}

/// FDR 0012 (RFC 0008 §3.1): pick the ONE attached connection a switch
/// routes to — the most-recent-input client, tmux's current-client
/// heuristic (per-viewport by construction: every relay/M2 channel serves
/// exactly one viewport). The requester's own connection is excluded; ties
/// and the never-typed case fall to the LATEST-attached candidate (highest
/// index — accept order). `None` when no other connection is attached.
/// Deliberately unfiltered by frame capability: the most-recent-input
/// connection IS the issuing viewport, and re-routing to a "more capable"
/// other viewport would switch the wrong screen — an old client that skips
/// the unknown tag is the specified visible no-op instead. (A push-cmd
/// requester, by contrast, IS the viewport: RFC 0016 §4.3.)
fn switch_route_target(clients: &[ClientConn], requester: usize) -> Option<usize> {
    let mut best: Option<(u64, usize)> = None;
    for (j, c) in clients.iter().enumerate() {
        if j == requester {
            continue;
        }
        let key = (c.last_input_ms, j);
        if best.is_none_or(|b| key >= b) {
            best = Some(key);
        }
    }
    best.map(|(_, j)| j)
}

/// Elementwise minimum size across all clients that have reported one
/// (tmux `window-size smallest`).
fn min_client_size(clients: &[ClientConn]) -> Option<(u16, u16)> {
    let mut acc: Option<(u16, u16)> = None;
    for c in clients {
        if c.rows == 0 || c.cols == 0 {
            continue;
        }
        acc = Some(match acc {
            None => (c.rows, c.cols),
            Some((r, w)) => (r.min(c.rows), w.min(c.cols)),
        });
    }
    acc
}

fn apply_client_size(clients: &[ClientConn], pty_fd: RawFd, term: &mut Terminal) {
    if let Some((rows, cols)) = min_client_size(clients) {
        pty::set_term_size(pty_fd, rows, cols);
        term.resize(rows, cols);
    }
}

/// Scrollback reflow reset, run after `apply_client_size` with the session
/// width from before it: a WIDTH change reflows the terminal (the case RFC
/// 0002 §4 exists for — its text says "on a resize", but only a width change
/// renumbers rows; a height change pushes or pops ring rows at the tail), so
/// every framed client's appended-row counting restarts at the reflowed
/// total. This is the session-socket stand-in for the UDP client's
/// one-message CAP_SCROLLBACK suppression — socket caps are Init-only, so the
/// restart is handled daemon-side. The matching client drops its ring on its
/// own resize, so both sides go forward-only from here: no reflowed rows
/// shipped against a stale floor, no mixed-width rows in the ring.
///
/// Only a width change: the resize path also runs when ANOTHER client
/// attaches, leaves or changes height, and a paced client still holds every
/// row scrolled since its last paced pair — a reset then would skip them
/// while that viewport keeps its ring (posh#225). On the viewport's own
/// height-only resize its ring is empty anyway, so shipping them is harmless.
fn reset_scrollback_floors_on_reflow(clients: &mut [ClientConn], term: &Terminal, cols_before: u16) {
    if term.cols() == cols_before {
        return;
    }
    let sb_total = term.primary_scrollback_total();
    for c in clients.iter_mut() {
        if c.producer.is_some() {
            c.sb_floor = sb_total;
        }
    }
}

/// v2 row spaces (RFC 0009 §1.1, posh#225 Stage 3), run beside
/// `reset_scrollback_floors_on_reflow` with the session size from before
/// the resize. A viewport whose own reported size changed cleared its ring,
/// so it gets a new epoch at the current total. Every OTHER viewport keeps
/// its epoch, ring and ack, and is re-anchored (`HistoryCursor::reanchor`:
/// unsent and sent-but-lost rows become one forward jump) when the
/// daemon's ring was renumbered under it without the total moving: a width
/// change reflowed it, or a height grow popped ring rows back onto the
/// grid. A height shrink needs nothing: it pushes grid rows into the ring
/// through the total, as a scroll does. So another viewport attaching
/// narrower, or leaving, never clears its history (v1 never did).
fn reset_history_on_resize(clients: &mut [ClientConn], term: &Terminal, (rows_before, cols_before): (u16, u16)) {
    let total = term.primary_scrollback_total();
    let renumbered = term.cols() != cols_before || term.rows() > rows_before;
    for c in clients.iter_mut() {
        let size = (c.rows, c.cols);
        let Some(h) = c.history_mut() else { continue };
        if !h.on_client_size(size, total) && renumbered {
            h.reanchor(total);
        }
        // The cached extent must name the epoch the next frame's id-10 entry
        // does, even if that frame goes out under the overlay or in the exit
        // flush (no send pass with the session terminal in between).
        c.note_history_extent(term);
    }
}

/// The environment a session daemon adds on top of its own when it spawns the
/// session shell: the session's identity, and always a non-empty TERM —
/// `term` (the daemon's own `TERM` reading) when it has one, a resolved one
/// when it does not.
///
/// The TERM floor exists because a session's shell inherits the environment
/// that created the session, and a create path that forgets to forward TERM
/// strands an interactive shell without one: no colors, visible character
/// re-echo. The `ph host:+` remote-atomic create did exactly that (its ssh
/// exec carried no environment), and while that path now forwards TERM like
/// every other posh-over-ssh spawn, no future one should be able to strand a
/// shell this way. A TERM the daemon already has is passed through untouched
/// — the client's own forwarded value always wins over a resolved guess.
pub(crate) fn session_spawn_env(
    name: &str,
    group: &str,
    term: Option<&str>,
) -> Vec<(String, String)> {
    // resolve_term never yields an empty string, even with no terminfo DB.
    let term = match term.filter(|t| !t.is_empty()) {
        Some(t) => t.to_string(),
        None => crate::terminfo::resolve_term(),
    };
    vec![
        ("POSH_SESSION".to_string(), name.to_string()),
        ("POSH_GROUP".to_string(), group.to_string()),
        ("TERM".to_string(), term),
    ]
}

fn daemon_main(
    cfg: &Config,
    name: &str,
    listener: UnixListener,
    command: Option<Vec<String>>,
    kind: SessionKind,
    seed: Option<u64>,
) -> ! {
    util::redirect_stdio_devnull();
    let _ = util::log_init(&cfg.log_path(name));
    // `posh --record FILE` names THIS session's recording. Take it out of the
    // environment before the shell is spawned, so neither the shell's own
    // `posh start` nor a push-cmd fork of this daemon records over it.
    let record_file = std::env::var_os("POSH_RECORD_FILE");
    std::env::remove_var("POSH_RECORD_FILE");
    // A daemon panic used to abort with no trace in the posh log (only the exit
    // paths that log first are visible), so a panic-death was indistinguishable
    // from a signal-kill. Record it before the default hook unwinds/aborts.
    // The hook only touches the already-initialized file logger (no unwinding
    // across the FFI boundary), so it is panic-safe.
    std::panic::set_hook(Box::new(|info| {
        util::log_write("error", &format!("daemon panic: {info}"));
    }));
    // Catch SIGTERM/SIGHUP/SIGINT and record which one fired: a terminating
    // signal now names itself in the teardown log instead of killing the daemon
    // silently under the default disposition (posh#136 silent-death diagnosis).
    util::install_daemon_signal_handlers();
    let socket_path = cfg.socket_path(name).expect("socket path");
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    // stdio is detached, so the PTY starts at the 24x80 default; the first
    // client Init resizes it.
    let (rows, cols) = (24u16, 80u16);
    let envs = session_spawn_env(name, &cfg.group, std::env::var("TERM").ok().as_deref());
    let child = match pty::spawn_shell(command.as_deref(), rows, cols, &envs, None) {
        Ok(c) => c,
        Err(e) => {
            util::log_write("error", &format!("failed to spawn pty: {e}"));
            let _ = std::fs::remove_file(&socket_path);
            std::process::exit(1);
        }
    };
    util::log_write(
        "info",
        &format!(
            "daemon started session={name} kind={} pid={}",
            kind.as_str(),
            child.pid
        ),
    );

    let _ = listener.set_nonblocking(true);
    let _ = util::set_nonblocking(child.master);

    let mut term = Terminal::with_scrollback(rows, cols, SCROLLBACK);
    let mut clients: Vec<ClientConn> = Vec::new();
    // Join argv with NUL (not spaces): the Tag::Info wire form, lossless for
    // arguments that contain spaces. github #18.
    let info_cmd = command.as_ref().map(|c| c.join("\0")).unwrap_or_default();

    // Optional `.castx` recording (posh --record FILE). Best-effort: a failure
    // to open never blocks the session.
    let recorder = open_recorder(record_file, rows, cols);

    // RFC 0014 §4.1: the session status socket (connect → response → EOF)
    // beside the session socket, its `.status.pid` liveness record written
    // before the bind. Best-effort: a failure degrades `posh status` only.
    let status_sock = cfg.status_socket_path(name);
    let status_pidfile = status_sock.with_extension("pid");
    let _ = std::fs::remove_file(&status_sock);
    let status_listener = std::fs::write(&status_pidfile, std::process::id().to_string())
        .and_then(|()| UnixListener::bind(&status_sock))
        .map_err(|e| {
            util::log_write(
                "warn",
                &format!("status socket unavailable {}: {e}", status_sock.display()),
            )
        })
        .ok();
    if let Some(l) = &status_listener {
        let _ = l.set_nonblocking(true);
    }

    let end = daemon_loop(
        &listener,
        status_listener.as_ref(),
        name,
        &cfg.group,
        &child,
        &mut term,
        &mut clients,
        &info_cmd,
        &cwd,
        kind,
        recorder,
        seed,
    );
    // The status socket is introspection, not a rendezvous: always removed.
    drop(status_listener);
    let _ = std::fs::remove_file(&status_sock);
    let _ = std::fs::remove_file(&status_pidfile);

    // Teardown. Reap the shell first: when it already exited (the pty-EIO
    // path) WNOHANG captures its real status before the group kills below.
    // The SIGHUP -> grace -> SIGKILL sequence always runs against the whole
    // process group regardless — background jobs survive the shell's own
    // exit and must not outlive the session.
    util::log_write("info", &format!("shutting down daemon session={name}"));
    let reaped = util::try_reap(child.pid);
    util::kill_pgroup(child.pid, libc::SIGHUP);
    std::thread::sleep(std::time::Duration::from_millis(500));
    util::kill_pgroup(child.pid, libc::SIGKILL);
    let status = reaped.unwrap_or_else(|| util::reap(child.pid));
    util::close_fd(child.master);
    let code = util::exit_code(status);
    // Tell attached clients WHY (posh#194; its own record ahead of Exit, so
    // an older client just skips it) and the real status before hanging up
    // (their EOF is the detach notice). Best-effort: a stuck client cannot
    // block teardown. github #18.
    let cause = caps::encode_exit_cause(end);
    for c in clients.iter_mut() {
        ipc::append_frame(&mut c.write_buf, Tag::ExitCause, &cause);
        ipc::append_frame(&mut c.write_buf, Tag::Exit, &ipc::encode_exit(code));
        let _ = util::write_all_retry(c.stream.as_raw_fd(), &c.write_buf, 100);
    }
    clients.clear();
    let _ = std::fs::remove_file(&socket_path);
    std::process::exit(code);
}

/// The session-line fields of the RFC 0014 §4.2 status response. `pub(crate)`
/// so the Architecture-A roaming server answers with the identical shape.
pub(crate) struct SessionStatus<'a> {
    pub(crate) name: &'a str,
    pub(crate) group: &'a str,
    pub(crate) daemon_pid: u32,
    pub(crate) frames: bool,
    pub(crate) echo_flag: bool,
    pub(crate) alt_screen: bool,
    pub(crate) activity: &'a str,
    /// ADR 0008's answer, omitted when the writer cannot resolve one.
    pub(crate) cwd: Option<&'a super::cwd::Resolved>,
}

/// The RFC 0014 §4.2 status response: the session line, then one client line
/// per attached client (`records` carry their `age=` already).
pub(crate) fn status_response(s: &SessionStatus<'_>, records: &[introspect::ClientRecord]) -> String {
    let mut out = format!(
        "session={} group={} daemon={} pid={} frames={} echo_flag={} \
         alt_screen={} clients={} activity={:?}",
        s.name,
        s.group,
        env!("POSH_BUILD"),
        s.daemon_pid,
        if s.frames { "on" } else { "off" },
        s.echo_flag as u8,
        s.alt_screen as u8,
        records.len(),
        s.activity,
    );
    if let Some(c) = s.cwd {
        out.push_str(&format!(" cwd={:?} cwd_source={}", c.dir, c.source.as_str()));
    }
    out.push('\n');
    for r in records {
        out.push_str(&introspect::render_client_line(r));
        out.push('\n');
    }
    out
}

/// Answer every pending connection on the status socket (RFC 0014 §4.1):
/// write the response, close. Never reads; a slow reader cannot stall the
/// daemon (the write is bounded by `write_all_retry`'s budget).
pub(crate) fn serve_status(listener: &UnixListener, response: &str) {
    while let Ok((stream, _)) = listener.accept() {
        let _ = util::write_all_retry(stream.as_raw_fd(), response.as_bytes(), 100);
    }
}

#[allow(clippy::too_many_arguments)]
fn daemon_loop(
    listener: &UnixListener,
    status: Option<&UnixListener>,
    name: &str,
    group: &str,
    child: &PtyChild,
    term: &mut Terminal,
    clients: &mut Vec<ClientConn>,
    info_cmd: &str,
    cwd: &str,
    kind: SessionKind,
    mut recorder: Option<SessionRecorder>,
    seed: Option<u64>,
) -> caps::SessionEnd {
    let listener_fd = listener.as_raw_fd();
    let pty_fd = child.master;
    let mut has_pty_output = false;
    let mut filter = ScreenSwitchFilter::default();
    let err_events = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
    // t=0 for recording timestamps (only used when recorder.is_some()).
    let rec_start = std::time::Instant::now();
    // Escape-to-shell overlay (FDR 0008), generalized from the roaming server to
    // the daemon (FDR 0011 Phase 2.4b). `Some` while a transient shell spawned by
    // a client's `Tag::Shell` is up: it becomes the broadcast source and input
    // sink, the live session keeps advancing `term` underneath, and the session
    // repaints when the overlay shell exits. `None` ⇒ today's behavior, exactly.
    let mut overlay: Option<Overlay> = None;
    // RFC 0013 §5.2 (#193): the on-frame activity label's foreground-process
    // half, probed at most every PROBE_INTERVAL_MS while any client wants it.
    let mut activity_probe_at: u64 = 0;
    let mut activity_process = String::new();
    // RFC 0016 §4.1: push-cmd tokens already served, seeded with the one
    // that created this session (if one did).
    let mut served_pushes: Vec<u64> = seed.into_iter().collect();

    // Why the loop ended (posh#194): reported to attached clients as the
    // `Tag::ExitCause` record ahead of `Tag::Exit`.
    let end = 'daemon: loop {
        if util::take_flag(&util::SIGTERM_RECEIVED) {
            let signo = util::LAST_SIGNAL.load(std::sync::atomic::Ordering::Acquire);
            util::log_write(
                "info",
                &format!(
                    "{} received, shutting down gracefully",
                    util::signal_name(signo)
                ),
            );
            break 'daemon caps::SessionEnd::Signaled(signo.clamp(0, 255) as u8);
        }

        // Backlog growth breadcrumb (posh#131 sibling diagnosis): one line per
        // new whole-MiB high-water above 4 MiB, so a real run shows the GROWTH
        // shape approaching the drop — and whether the socket is draining while
        // it climbs (stalled vs bursty). Throttled via `hiwater_mb`; only ever
        // logs while a client is more than a quarter of the way to the cap.
        let now = util::now_ms();
        for c in clients.iter_mut() {
            let mb = c.write_buf.len() / (1024 * 1024);
            if mb >= 4 && mb > c.hiwater_mb {
                c.hiwater_mb = mb;
                util::log_write(
                    "warn",
                    &format!("client backlog high-water {}", backlog_log_fields(c, now)),
                );
            }
            // A paced viewport's ack latency (posh#225 Stage 3.0): at most
            // one line per `ACK_LOG_INTERVAL_MS`, and only with new samples.
            if let Some(line) = ack_latency_log_line(c, now) {
                util::log_write("info", &line);
            }
        }

        // Drop stuck readers before building the pollfd set (so the fd<->client
        // index mapping stays consistent for this iteration). github #11. The
        // drained_total / last_drain_age discriminate the drop cause: a STALLED
        // reader shows drained_total flat and a large last_drain_age; a BURSTY
        // one shows recent draining (small age, growing drained_total) yet still
        // outran the cap.
        clients.retain(|c| {
            if c.write_buf.len() > MAX_CLIENT_BACKLOG {
                util::log_write(
                    "warn",
                    &format!("dropping slow client {}", backlog_log_fields(c, now)),
                );
                false
            } else {
                true
            }
        });

        let mut fds = Vec::with_capacity(3 + clients.len());
        fds.push(util::pollfd(listener_fd, libc::POLLIN));
        fds.push(util::pollfd(pty_fd, libc::POLLIN));
        for c in clients.iter() {
            let mut events = libc::POLLIN;
            if !c.write_buf.is_empty() {
                events |= libc::POLLOUT;
            }
            fds.push(util::pollfd(c.stream.as_raw_fd(), events));
        }
        // Client fds occupy indices 2..2+n_client_fds; the overlay master (if
        // up) is appended AFTER them so the fixed client index math is unchanged.
        let n_client_fds = clients.len();
        let overlay_idx = match &overlay {
            Some(o) => {
                fds.push(util::pollfd(o.child.master, libc::POLLIN));
                fds.len() - 1
            }
            None => usize::MAX,
        };
        // The RFC 0014 status socket, appended last for the same reason.
        let status_idx = match status {
            Some(l) => {
                fds.push(util::pollfd(l.as_raw_fd(), libc::POLLIN));
                fds.len() - 1
            }
            None => usize::MAX,
        };

        // Block until an fd is ready or the nearest paced send opportunity
        // (posh#225): `-1` while no paced client owes a frame or a history
        // body (history pauses while the escape overlay is up).
        match util::poll(&mut fds, paced_poll_timeout(clients, overlay.is_none().then_some(&*term), now)) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                util::log_write("error", &format!("poll failed: {e}"));
                break 'daemon caps::SessionEnd::Failed;
            }
        }

        // FDR 0006: stamp the ACTIVE pty's ECHO state onto every frame this
        // iteration produces (`ClientConn::echo_flag` → the frames' FLAG_ECHO),
        // exactly as `server_loop` computes `echo_flag` per send. Without
        // this the daemon's frames never carried FLAG_ECHO at all, so on the
        // relay path (the default bootstrap) the client's optimistic-echo
        // gate read "echo off" for the whole session and the model predicted
        // nothing — precisely where the slow-link escalation selects it.
        let echo_flag = {
            let active_master = overlay.as_ref().map(|o| o.child.master).unwrap_or(pty_fd);
            if crate::pty::echo_on(active_master) {
                crate::remote::sync::FLAG_ECHO
            } else {
                0
            }
        };
        // FDR 0008 (posh#178): while the escape-to-shell overlay is up, stamp
        // FLAG_OVERLAY on every frame so a roaming client clears its "opening
        // shell…" notice — the daemon's counterpart to the Arch-A server's
        // overlay signal, forwarded verbatim by the relay.
        let overlay_flag = if overlay.is_some() {
            crate::remote::sync::FLAG_OVERLAY
        } else {
            0
        };
        // RFC 0013 §5.2: refresh the activity label for requesting clients
        // (the title from the model each turn, the process on a throttle);
        // `queue_frame` attaches it to a visible frame only when it changed
        // for that client.
        let activity = clients
            .iter()
            .any(|c| c.wants_activity)
            .then(|| current_activity(pty_fd, term, &mut activity_process, &mut activity_probe_at));
        for c in clients.iter_mut() {
            c.echo_flag = echo_flag;
            c.overlay_flag = overlay_flag;
            if c.wants_activity {
                c.activity_now = activity.clone();
            }
        }

        // New client connections.
        if fds[0].revents & err_events != 0 {
            util::log_write("error", "server socket error");
            break 'daemon caps::SessionEnd::Failed;
        }
        // RFC 0014 §4.1: answer status readers — connect → response → close.
        if let Some(l) = status.filter(|_| fds[status_idx].revents & libc::POLLIN != 0) {
            let now = util::now_ms();
            let records: Vec<introspect::ClientRecord> =
                clients.iter().map(|c| c.record_now(now)).collect();
            let activity = super::activity::compose(
                crate::pty::foreground_command(pty_fd).as_deref(),
                term.title(),
            );
            let now_cwd = daemon_cwd(child.pid, term, cwd);
            let response = status_response(
                &SessionStatus {
                    name,
                    group,
                    daemon_pid: std::process::id(),
                    frames: true,
                    echo_flag: clients.iter().any(|c| c.echo_flag != 0),
                    alt_screen: term.is_alt_screen(),
                    activity: &activity,
                    cwd: Some(&now_cwd),
                },
                &records,
            );
            serve_status(l, &response);
        }

        if fds[0].revents & libc::POLLIN != 0 {
            if let Ok((stream, _)) = listener.accept() {
                let _ = stream.set_nonblocking(true);
                util::log_write(
                    "info",
                    &format!("client connected fd={}", stream.as_raw_fd()),
                );
                clients.push(ClientConn {
                    stream,
                    read_buf: FrameBuffer::new(),
                    write_buf: Vec::new(),
                    rows: 0,
                    cols: 0,
                    caps: Vec::new(),
                    producer: None,
                    lossy: false,
                    coalesce: false,
                    coalesce_off: false,
                    pending_frame_start: None,
                    sb_floor: 0,
                    acked_sb_total: 0,
                    bytes_drained: 0,
                    last_drain_ms: util::now_ms(),
                    hiwater_mb: 0,
                    echo_flag: 0,
                    overlay_flag: 0,
                    record: introspect::ClientRecord::default(),
                    record_at: 0,
                    attach_pid: None,
                    last_input_ms: 0,
                    wants_activity: false,
                    activity_now: None,
                    activity_sent: None,
                    kind,
                    kind_sent: false,
                    wants_push_cmd: false,
                    push_offered: false,
                    visible_shaped_for: None,
                    regeometry_keyframe: None,
                    pacing: None,
                    init_applied: false,
                });
            }
        }

        // PTY output: feed the terminal model, return any query replies to
        // the application, and broadcast the bytes to all clients — raw,
        // except that screen switches are virtualized (clients pin the
        // outer terminal to its alternate screen for the whole attach).
        if fds[1].revents & (libc::POLLIN | err_events) != 0 {
            let mut buf = [0u8; 4096];
            match util::read_fd(pty_fd, &mut buf) {
                Ok(0) => {
                    util::log_write("info", "shell exited");
                    break 'daemon caps::SessionEnd::Exited;
                }
                Ok(n) => {
                    let mut bcast = Vec::with_capacity(n);
                    filter.feed(term, &buf[..n], &mut bcast);
                    // Record the RAW chunk (what the emulator processed), not
                    // the screen-switch-filtered broadcast — that's what makes
                    // a poshterity replay reproduce this session's screen.
                    if let Some(rec) = recorder.as_mut() {
                        if rec.output(rec_start.elapsed().as_secs_f64(), &buf[..n]).is_err() {
                            recorder = None; // disable on write error; never kill the session
                        }
                    }
                    // The model answers the app's queries (DA/DSR/kitty/...).
                    // github #13 kept it silent whenever any client was
                    // attached, on the theory the real terminal answers — true
                    // only for a legacy Tag::Output client whose terminal sees
                    // the raw query. A FRAME client never receives the raw query
                    // (RFC 0008 sends screen state, not the byte stream), so
                    // under frame transport nobody answers and an app probing
                    // kitty support (CSI ? u) concludes "unsupported" — the
                    // Shift+Enter root cause (posh#128). RFC 0010: when every
                    // attached client is a frame client (or none), the daemon
                    // answers itself, rewriting the kitty reply to the effective
                    // client-terminal capability; with any legacy client, it
                    // stays silent so that terminal answers (no double reply).
                    let responses = term.take_responses();
                    if !responses.is_empty() {
                        match query_policy(clients) {
                            QueryPolicy::Answer => {
                                let _ = util::write_all_retry(pty_fd, &responses, 100);
                            }
                            QueryPolicy::SuppressKitty => {
                                let out = strip_kitty_reply(&responses);
                                if !out.is_empty() {
                                    let _ = util::write_all_retry(pty_fd, &out, 100);
                                }
                            }
                            QueryPolicy::Silent => {}
                        }
                    }
                    has_pty_output = true;
                    // While an escape overlay is up it owns the broadcast (FDR
                    // 0008): the session model still advances above, but its
                    // output is not broadcast until the overlay closes.
                    if overlay.is_none() && !bcast.is_empty() {
                        broadcast_output(clients, term, &bcast);
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    // EIO on Linux when the slave side is gone.
                    util::log_write("info", "pty closed");
                    break 'daemon caps::SessionEnd::Exited;
                }
            }
        }

        // Escape-overlay shell output (FDR 0008): feed the overlay terminal (the
        // active broadcast source) and broadcast from it. On EOF/EIO the overlay
        // shell exited — tear it down and repaint the restored session, forcing a
        // keyframe since the broadcast source swaps back to the live session.
        if overlay_idx != usize::MAX
            && fds[overlay_idx].revents & (libc::POLLIN | err_events) != 0
        {
            let mut closed = false;
            let mut ov_bcast: Vec<u8> = Vec::new();
            if let Some(o) = overlay.as_mut() {
                let mut buf = [0u8; 4096];
                match util::read_fd(o.child.master, &mut buf) {
                    Ok(0) => closed = true,
                    Ok(n) => {
                        o.term.process(&buf[..n]);
                        let responses = o.term.take_responses();
                        if !responses.is_empty() {
                            let _ = util::write_all_retry(o.child.master, &responses, 100);
                        }
                        ov_bcast.extend_from_slice(&buf[..n]);
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => closed = true,
                }
            }
            if closed {
                close_overlay(&mut overlay);
                // Restore the live session view (Ctrl-D returned to the session).
                broadcast_source_swap(clients, term, &term.dump_vt_flat());
            } else if !ov_bcast.is_empty() {
                // Frame-capable clients diff/dump from the overlay terminal; a
                // baseline client receives the raw overlay bytes.
                if let Some(o) = overlay.as_ref() {
                    broadcast_output(clients, &o.term, &ov_bcast);
                }
            }
        }

        // Client traffic. Iterate only over the clients present when the
        // pollfd set was built; walk backwards so removal is safe.
        let polled = n_client_fds;
        let mut i = clients.len().min(polled);
        while i > 0 {
            i -= 1;
            let revents = fds[i + 2].revents;
            if revents == 0 {
                continue;
            }
            let mut remove = false;
            let mut resized = false;
            let mut needs_replay = false;
            let mut detach_all = false;
            let mut open_shell = false;
            let mut switch_req: Option<Vec<u8>> = None;
            let mut push_for: Option<u64> = None;
            let total_clients = clients.len();
            {
                let c = &mut clients[i];
                if revents & libc::POLLIN != 0 {
                    match c.read_buf.read_from(c.stream.as_raw_fd()) {
                        Ok(0) => remove = true,
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => remove = true,
                    }
                    if !remove {
                        loop {
                            let frame = match c.read_buf.next() {
                                Ok(Some(frame)) => frame,
                                Ok(None) => break,
                                // Oversize/corrupt framing from this peer: drop it.
                                Err(_) => {
                                    remove = true;
                                    break;
                                }
                            };
                            match frame.tag {
                                Tag::Input => {
                                    // Route to the overlay shell while it is up
                                    // (FDR 0008), else the session PTY.
                                    let target = overlay
                                        .as_ref()
                                        .map(|o| o.child.master)
                                        .unwrap_or(pty_fd);
                                    let _ = util::write_all_retry(target, &frame.payload, 100);
                                    // FDR 0012: the switch router's
                                    // current-viewport signal.
                                    c.last_input_ms = util::now_ms();
                                }
                                Tag::Init => {
                                    if c.apply_init(&frame.payload) {
                                        resized = true;
                                    }
                                    push_for = push_for.or(take_push_request(&c.caps, &mut served_pushes));
                                    // Enable per-client frame production for a
                                    // frame-capable client; a no-op for a
                                    // baseline client (the replay/broadcast
                                    // then stay on Tag::Output). RFC 0008.
                                    let framed_before = c.producer.is_some();
                                    c.maybe_enable_frames();
                                    // Forward-only scrollback (RFC 0002 §3): a
                                    // freshly framed client starts with an empty
                                    // ring, so anchor its floor at the current
                                    // total — only rows appended AFTER attach are
                                    // synced, never pre-attach history.
                                    if !framed_before && c.producer.is_some() {
                                        c.sb_floor = term.primary_scrollback_total();
                                    }
                                    // RFC 0009 v2 (posh#225 Stage 3): a paced
                                    // viewport that advertised SCROLLBACK2 gets
                                    // its history cursor here; a bare re-Init is
                                    // a no-op inside.
                                    c.open_history(term);
                                    // Replay the current screen so the client
                                    // sees state it missed (including the first
                                    // attach to a detached-created session). The
                                    // dump is queued after the resize below so
                                    // it reflects the new client size. github #16.
                                    // It also covers everything the client was
                                    // not sent before this Init (posh#239), which
                                    // is only ever PTY output or an overlay's.
                                    needs_replay = has_pty_output || overlay.is_some();
                                }
                                Tag::Resize => {
                                    if c.apply_resize(&frame.payload) {
                                        resized = true;
                                    }
                                }
                                Tag::ClientCaps => {
                                    // RFC 0014 §3: the relay forwarding its
                                    // roaming client's identity/state as they
                                    // arrive. A malformed table is dropped, the
                                    // held record kept.
                                    if let Ok((table, _)) = caps::decode_table(&frame.payload) {
                                        c.absorb_client_caps(&table, util::now_ms(), false);
                                        push_for = push_for.or(take_push_request(&table, &mut served_pushes));
                                    }
                                }
                                Tag::Detach => {
                                    remove = true;
                                    break;
                                }
                                Tag::DetachAll => {
                                    detach_all = true;
                                    break;
                                }
                                Tag::Kill => break 'daemon caps::SessionEnd::Killed,
                                Tag::Info => {
                                    // RFC 0013 §5 activity label: the pty's
                                    // foreground-process command plus the
                                    // terminal title the shell/app set.
                                    let activity = super::activity::compose(
                                        crate::pty::foreground_command(pty_fd).as_deref(),
                                        term.title(),
                                    );
                                    let info = SessionInfo {
                                        clients: (total_clients - 1) as u64,
                                        pid: child.pid,
                                        cmd: info_cmd.to_string(),
                                        cwd: cwd.to_string(),
                                        activity,
                                        kind,
                                        cwd_now: Some(daemon_cwd(child.pid, term, cwd)),
                                    };
                                    c.queue(Tag::Info, &info.encode());
                                }
                                Tag::History => {
                                    let out = if ipc::decode_history_format(&frame.payload) {
                                        term.dump_vt()
                                    } else {
                                        term.dump_text().into_bytes()
                                    };
                                    c.queue(Tag::History, &out);
                                }
                                Tag::Run => {
                                    let _ = util::write_all_retry(pty_fd, &frame.payload, 1000);
                                    c.queue(Tag::Ack, b"");
                                }
                                Tag::Shell => {
                                    // Escape-to-shell (FDR 0008): defer the spawn
                                    // out of this per-client borrow so the source
                                    // swap can iterate every client's producer.
                                    // The `overlay.is_none()` guard (below) makes
                                    // a retransmitted request idempotent.
                                    open_shell = true;
                                }
                                Tag::SwitchRequest => {
                                    // FDR 0012 in-place switch (RFC 0008 §3.1):
                                    // sent by the in-session `posh attach
                                    // <sibling>` over a fresh connection. Defer
                                    // routing out of this per-client borrow —
                                    // the target is ANOTHER attached client. A
                                    // malformed payload is dropped (the sender
                                    // validated; nothing to answer).
                                    if ipc::decode_switch_target(&frame.payload).is_some() {
                                        switch_req = Some(frame.payload.clone());
                                    }
                                }
                                // A lossy relay client (RFC 0008 §3) OR a
                                // coalescing local client (CAP_COALESCE, posh#137)
                                // acking one of its `Tag::Frame`s — the base-advance
                                // a reliable client gets from the immediate self-ack;
                                // also carries the runtime coalescing toggle. Shared
                                // with the tests via `apply_frame_ack` (like
                                // `apply_init`).
                                Tag::FrameAck => handle_frame_ack(
                                    c,
                                    &frame.payload,
                                    active_source(overlay.as_ref().map(|o| &o.term), term),
                                    util::now_ms(),
                                ),
                                // Output, Ack, Exit, Frame, and Switch are all
                                // daemon->client only; ignore if received from
                                // a client.
                                Tag::Output
                                | Tag::Ack
                                | Tag::Exit
                                | Tag::ExitCause
                                | Tag::Frame
                                | Tag::Switch => {}
                            }
                        }
                    }
                }
                if !remove && revents & libc::POLLOUT != 0 && !c.write_buf.is_empty() {
                    match c.stream.write(&c.write_buf) {
                        Ok(n) => {
                            c.write_buf.drain(..n);
                            // Coalesce-anchor bookkeeping (posh#137): the drain
                            // shifts the anchor left by `n`. `checked_sub` yields
                            // `None` exactly when `n > start` — the pending frame
                            // has begun going on the wire, so it can no longer be
                            // truncated — and `Some(start - n)` otherwise (including
                            // `Some(0)` at `n == start`, still fully un-sent).
                            if let Some(start) = c.pending_frame_start {
                                c.pending_frame_start = start.checked_sub(n);
                            }
                            // Backlog instrumentation: record the drain so the
                            // high-water / drop lines can tell stalled from bursty.
                            if n > 0 {
                                c.bytes_drained += n as u64;
                                c.last_drain_ms = util::now_ms();
                            }
                            // Recovered below a MiB ⇒ re-arm the high-water log so
                            // a later climb is reported afresh (not one-shot).
                            let mb = c.write_buf.len() / (1024 * 1024);
                            if mb < c.hiwater_mb {
                                c.hiwater_mb = mb;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => remove = true,
                    }
                }
                if revents & err_events != 0 {
                    remove = true;
                }
                // Is this frame client owed a frame for the geometry it now
                // reports (posh#225)? The rule and its reasons live on
                // `prepare_regeometry_frame`. Reuses the replay below, which
                // runs after `apply_client_size`.
                if c.prepare_regeometry_frame() {
                    needs_replay |= has_pty_output;
                }
            }
            // RFC 0013 §5.2 / RFC 0016 §2: a request this client's messages
            // latched after the loop-top refresh (Init, Tag::ClientCaps) is
            // answered this iteration — on the replay below, else by the
            // end-of-iteration pass (`queue_due_answers`) — not after the
            // next output.
            if !remove && clients[i].wants_activity && clients[i].activity_now.is_none() {
                clients[i].activity_now =
                    Some(current_activity(pty_fd, term, &mut activity_process, &mut activity_probe_at));
            }
            // FDR 0012 (RFC 0008 §3.1): route a validated switch request to
            // the most-recent-input attached connection — the issuing
            // viewport — BEFORE any removal shifts indices (the requester
            // usually EOFs in the same batch it sends in). The requester's
            // own connection (index i) is excluded; with no other connection
            // attached the switch is a visible no-op, as specified.
            if let Some(payload) = switch_req.take() {
                if let Some(j) = switch_route_target(clients, i) {
                    util::log_write(
                        "info",
                        &format!(
                            "switch: routing to client fd={}",
                            clients[j].stream.as_raw_fd()
                        ),
                    );
                    clients[j].queue(Tag::Switch, &payload);
                } else {
                    util::log_write("info", "switch: no attached viewport to route to");
                }
            }
            // RFC 0016 §4: serve a push-cmd — a new anonymous session here,
            // then re-home THIS connection (the requesting viewport) onto it.
            // A requester that is already gone gets nothing created.
            if let Some(token) = push_for.filter(|_| !remove) {
                let here = daemon_cwd(child.pid, term, cwd);
                match create_pushed_session(group, &here.dir, token) {
                    Ok(pushed) => {
                        util::log_write(
                            "info",
                            &format!(
                                "push-cmd: created {pushed} in {} (cwd_source={}) for client fd={}",
                                here.dir,
                                here.source.as_str(),
                                clients[i].stream.as_raw_fd()
                            ),
                        );
                        clients[i].queue(Tag::Switch, &ipc::encode_switch_target(group, &pushed));
                    }
                    Err(e) => util::log_write("error", &format!("push-cmd: create failed: {e}")),
                }
            }
            if detach_all {
                util::log_write("info", &format!("detach all clients={}", clients.len()));
                clients.clear();
                break;
            }
            if remove {
                // The backlog fields (`fd=` first) carry a paced viewport's
                // final ack latency (posh#225 Stage 3.0).
                let fields = backlog_log_fields(&clients[i], util::now_ms());
                clients.remove(i);
                util::log_write(
                    "info",
                    &format!("client disconnected {fields} remaining={}", clients.len()),
                );
                // The smallest client may have left; grow back (zmx issue #8).
                resized = true;
            }
            if resized {
                let (rows_before, cols_before) = (term.rows(), term.cols());
                apply_client_size(clients, pty_fd, term);
                // Keep the escape overlay sized to the session in lockstep (FDR
                // 0008): both PTYs and both terminal models track the new dims.
                if let Some(o) = overlay.as_mut() {
                    pty::set_term_size(o.child.master, term.rows(), term.cols());
                    o.term.resize(term.rows(), term.cols());
                }
                // Record the new effective size (asciinema "COLSxROWS").
                if let Some(rec) = recorder.as_mut() {
                    let t = rec_start.elapsed().as_secs_f64();
                    if rec.resize(t, term.cols(), term.rows()).is_err() {
                        recorder = None;
                    }
                }
                reset_scrollback_floors_on_reflow(clients, term, cols_before);
                reset_history_on_resize(clients, term, (rows_before, cols_before));
            }
            // Replay after the resize so the dump reflects the client's size.
            // Skip if the client was removed this iteration. github #16.
            // Flat dump: the client pinned the outer terminal to its alt
            // screen, so the replay must never switch the outer's buffers
            // (the outer primary belongs to the user's shell). Session
            // scrollback stays reachable via `posh history`.
            if needs_replay && !remove && i < clients.len() {
                // For a frame-capable client the replay IS the producer's first
                // frame: a fresh producer holds only the empty frame-0 base, so
                // `encode_visible` yields a `Full` keyframe — the equivalent of
                // the dump replay. A baseline client keeps the flat `dump_vt`
                // (it pinned the outer terminal to its alt screen, so the replay
                // must never switch buffers). RFC 0008.
                // After a frame client's own resize, whether this replay may be
                // a `Diff` or must be a `Full` was settled by
                // `prepare_regeometry_frame` (above).
                // Replay the ACTIVE broadcast source: while an escape overlay is
                // up it is what every client sees (FDR 0008), so a client
                // attaching / resuming mid-overlay must base on the overlay
                // screen, not the live session underneath (see `active_source`).
                let src = active_source(overlay.as_ref().map(|o| &o.term), term);
                queue_replay(&mut clients[i], src);
            }
            // Escape-to-shell (FDR 0008): a client asked to open the overlay.
            // Deferred here so the source swap can iterate every client's
            // producer without conflicting with the per-client borrow above.
            // Idempotent via the `overlay.is_none()` guard: a retransmitted
            // request while the overlay is up is a no-op.
            if open_shell && overlay.is_none() {
                // ADR 0008: the same cascade `Tag::Info` reports, so the
                // overlay and every other consumer agree on "here".
                let ov_cwd = daemon_cwd(child.pid, term, cwd).dir;
                let cmd = escape_command();
                let (r, w) = (term.rows(), term.cols());
                match pty::spawn_shell(cmd.as_deref(), r, w, &[], Some(&ov_cwd)) {
                    Ok(oc) => {
                        let _ = util::set_nonblocking(oc.master);
                        overlay = Some(Overlay {
                            child: oc,
                            term: Terminal::new(r, w),
                        });
                        // Force a keyframe on the source swap and paint the (blank)
                        // overlay now; the shell's prompt follows as a Diff.
                        if let Some(o) = overlay.as_ref() {
                            let dump = o.term.dump_vt_flat();
                            broadcast_source_swap(clients, &o.term, &dump);
                        }
                    }
                    Err(e) => {
                        util::log_write("error", &format!("escape-to-shell spawn failed: {e}"))
                    }
                }
            }
        }

        let src = active_source(overlay.as_ref().map(|o| &o.term), term);
        queue_due_answers(clients, src);

        // posh#225 (RFC 0008 §3.2): the paced send pass runs last, so its
        // frames reflect everything this iteration fed the terminal. A frame
        // queued here is written next iteration (`POLLOUT` is armed for a
        // non-empty `write_buf`). v2 history comes from the session terminal
        // and pauses while the escape overlay is up.
        send_paced_frames(clients, src, overlay.is_none().then_some(&*term), util::now_ms());
    };

    // A paced client's last screen may still be owed (posh#225): build it now,
    // from the source it was owed from — the overlay if one is still up, as a
    // non-paced client's last frame was — before the overlay is closed. It is
    // written with `Exit` in `daemon_main`'s teardown.
    flush_paced_frames(
        clients,
        active_source(overlay.as_ref().map(|o| &o.term), term),
        util::now_ms(),
    );

    // Tear down any escape overlay before the shell/session cleanup (FDR 0008).
    close_overlay(&mut overlay);

    // Flush the recording's held UTF-8 tail + buffered writer on the way out
    // (shell exit / SIGTERM / kill).
    if let Some(mut rec) = recorder {
        let _ = rec.finish();
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session shell's spawn env always names the session and its group,
    /// and never leaves TERM unset: a create path that forwarded none (the
    /// `ph host:+` regression) must not strand an interactive shell without
    /// one. A TERM the daemon already has is passed through untouched.
    #[test]
    fn session_spawn_env_floors_term_without_overriding_a_forwarded_one() {
        let term_of = |env: &[(String, String)]| -> Option<String> {
            env.iter().find(|(k, _)| k == "TERM").map(|(_, v)| v.clone())
        };
        let identity = |env: &[(String, String)]| -> Vec<(String, String)> {
            env.iter().filter(|(k, _)| k != "TERM").cloned().collect()
        };
        let expected_identity = [("POSH_SESSION", "s-1"), ("POSH_GROUP", "grp")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .to_vec();

        let forwarded = session_spawn_env("s-1", "grp", Some("xterm-kitty"));
        assert_eq!(term_of(&forwarded).as_deref(), Some("xterm-kitty"));
        assert_eq!(identity(&forwarded), expected_identity);

        // Absent or empty: a resolved TERM, never nothing and never "".
        for missing in [None, Some("")] {
            let env = session_spawn_env("s-1", "grp", missing);
            let term = term_of(&env).expect("a TERM is set when the daemon has none");
            assert!(!term.is_empty(), "resolved TERM must not be empty");
            assert_eq!(identity(&env), expected_identity);
        }
    }

    /// RFC 0014 §3: an Init table's identity is the ATTACHMENT's own; a later
    /// `ClientCaps` identity with another pid is the origin behind a relay
    /// (`via=relay`), and its state lands with an `age=` from `record_at`.
    #[test]
    fn absorb_client_caps_keeps_the_originating_record_behind_a_relay() {
        let (c, _peer) = frame_capable_conn(24, 80);
        let mut c = c;
        let relay = introspect::Ident {
            version: "1".into(),
            git_sha: "a".into(),
            pid: 100,
            start_unix_ms: 1,
        };
        c.absorb_client_caps(&[introspect::encode_client_ident(&relay)], 5, true);
        assert_eq!(c.attach_pid, Some(100));
        assert_eq!(c.record.via_relay_pid, None);
        let origin = introspect::Ident {
            pid: 200,
            ..relay.clone()
        };
        let state = introspect::coverage_fixture();
        c.absorb_client_caps(
            &[
                introspect::encode_client_ident(&origin),
                introspect::encode_client_state(&state),
            ],
            1_000,
            false,
        );
        assert_eq!(c.record.via_relay_pid, Some(100));
        assert_eq!(c.record.ident.as_ref().map(|i| i.pid), Some(200));
        assert_eq!(c.record.state, Some(state));
        let line = introspect::render_client_line(&c.record_now(1_250));
        assert!(line.contains("client pid=200 build=1+a via=relay pid=100 echo=optimistic"), "{line}");
        assert!(line.ends_with(" age=250"), "{line}");
        // A malformed state entry keeps the held record.
        c.absorb_client_caps(
            &[caps::Cap {
                id: caps::CAP_CLIENT_STATE,
                payload: vec![9, 9],
            }],
            2_000,
            false,
        );
        assert_eq!(c.record.state, Some(state));
    }

    /// RFC 0014 §4.1: the socket contract end to end — `serve_status` answers
    /// connect → response → EOF, `read_status_socket` reads exactly that, and
    /// a bound-then-dropped socket reads as `stale`.
    #[test]
    fn status_socket_serves_and_reads_the_response() {
        // Short /tmp path so the unix socket stays within SUN_LEN (the scratch
        // $TMPDIR is too deep) — the agent.rs/mux.rs `temp_base` convention.
        let dir = std::path::PathBuf::from(format!("/tmp/posh-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("w1.status.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let reader = {
            let sock = sock.clone();
            std::thread::spawn(move || session::read_status_socket(&sock))
        };
        // Poll-serve until the reader has connected and been answered.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut answered = false;
        while !answered && std::time::Instant::now() < deadline {
            let mut fds = [util::pollfd(listener.as_raw_fd(), libc::POLLIN)];
            if util::poll(&mut fds, 100).is_ok() && fds[0].revents & libc::POLLIN != 0 {
                serve_status(&listener, "session=w1 clients=0\n");
                answered = true;
            }
        }
        assert_eq!(reader.join().unwrap().unwrap(), "session=w1 clients=0\n");
        drop(listener);
        assert!(session::read_status_socket(&sock).unwrap_err().to_string().contains("stale"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RFC 0014 §4.2: the session line then one client line each, the
    /// registered fields present, an old client rendering `echo=unknown`.
    #[test]
    fn status_response_renders_session_and_client_lines() {
        let reported = introspect::ClientRecord {
            state: Some(introspect::coverage_fixture()),
            age_ms: Some(7),
            ..Default::default()
        };
        let old = introspect::ClientRecord::default();
        let out = status_response(
            &SessionStatus {
                name: "w1",
                group: "default",
                daemon_pid: 42,
                frames: true,
                echo_flag: true,
                alt_screen: false,
                activity: "fish · ~/x",
                cwd: None,
            },
            &[reported, old],
        );
        let mut lines = out.lines();
        let session = lines.next().unwrap();
        assert!(session.starts_with("session=w1 group=default daemon="), "{session}");
        assert!(session.ends_with(" pid=42 frames=on echo_flag=1 alt_screen=0 clients=2 activity=\"fish · ~/x\""), "{session}");
        assert!(!session.contains("cwd"), "an unknown cwd is omitted (§4.2 MAY): {session}");
        let first = lines.next().unwrap();
        for key in introspect::CLIENT_FIELDS {
            assert!(first.contains(&format!(" {key}=")), "missing {key}= in {first}");
        }
        assert_eq!(lines.next().unwrap(), "client build=unknown echo=unknown");
        assert!(lines.next().is_none());
    }

    /// ADR 0008: the session line ends with where the session IS and which
    /// cascade step said so — the "why did this open in ~?" answer.
    #[test]
    fn status_response_reports_the_cwd_and_its_source() {
        let now = super::super::cwd::Resolved {
            dir: "/w/my repo".into(),
            source: super::super::cwd::Source::Osc7,
        };
        let out = status_response(
            &SessionStatus {
                name: "w1",
                group: "default",
                daemon_pid: 42,
                frames: true,
                echo_flag: false,
                alt_screen: false,
                activity: "",
                cwd: Some(&now),
            },
            &[],
        );
        let session = out.lines().next().unwrap();
        assert!(
            session.ends_with(" activity=\"\" cwd=\"/w/my repo\" cwd_source=osc7"),
            "{session}"
        );
    }

    // ---- An in-process session daemon (design 2026-09-21 §1) ----

    /// A private, SHORT-path base dir (the `mux.rs` / `agent.rs` `temp_base`
    /// pattern: the scratch `$TMPDIR` is too deep for sun_path).
    fn temp_base() -> std::path::PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::path::PathBuf::from(format!("/tmp/posh-daemon-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)
            .unwrap();
        base
    }

    /// The PRODUCTION [`daemon_loop`] running on a thread over a bound
    /// session socket and a real PTY child — no double-fork, no stdio
    /// redirect, no process exit — so a test talks to it through the socket
    /// exactly as a client does. `shutdown` ends it the way `posh kill` does;
    /// dropping it without one (a failed assertion) still reaps the child,
    /// closes the PTY, and removes the socket dir — the daemon thread is
    /// left to die with the test binary rather than joined without a kill.
    struct TestDaemon {
        base: std::path::PathBuf,
        socket: std::path::PathBuf,
        child_pid: libc::pid_t,
        master: RawFd,
        thread: Option<std::thread::JoinHandle<caps::SessionEnd>>,
        torn_down: bool,
    }

    impl TestDaemon {
        /// `Tag::Kill` the loop, join it, then reap the PTY child (the test's
        /// stand-in for `daemon_main`'s teardown).
        fn shutdown(mut self) -> caps::SessionEnd {
            let stream = UnixStream::connect(&self.socket).unwrap();
            ipc::send(stream.as_raw_fd(), Tag::Kill, b"").unwrap();
            let end = self.thread.take().unwrap().join().unwrap();
            self.teardown();
            end
        }

        /// Best-effort, idempotent: the child, the master fd, the dir.
        fn teardown(&mut self) {
            if self.torn_down {
                return;
            }
            self.torn_down = true;
            util::kill_pgroup(self.child_pid, libc::SIGKILL);
            util::reap(self.child_pid);
            util::close_fd(self.master);
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    impl Drop for TestDaemon {
        fn drop(&mut self) {
            self.teardown();
        }
    }

    /// Binds `cfg`'s socket for `name` and runs [`daemon_loop`] on a thread
    /// with `command` (default: a 30 s `sleep`, a quiet child) and `kind`.
    /// The handle owns `cfg.socket_dir` (removed on drop). NOTE: `daemon_loop`
    /// reads the process-global `util::SIGTERM_RECEIVED` flag, so a future
    /// test that raises a signal at the test binary can bleed into a
    /// concurrent in-process daemon.
    fn spawn_test_daemon(
        cfg: &Config,
        name: &str,
        command: Option<Vec<String>>,
        kind: SessionKind,
    ) -> TestDaemon {
        let socket = cfg.socket_path(name).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let command = command.unwrap_or_else(|| vec!["sleep".into(), "30".into()]);
        let child = pty::spawn_shell(Some(&command), 24, 80, &[], None).unwrap();
        util::set_nonblocking(child.master).unwrap();
        let (child_pid, master) = (child.pid, child.master);
        let (name, group) = (name.to_string(), cfg.group.clone());
        let thread = std::thread::spawn(move || {
            let mut term = Terminal::with_scrollback(24, 80, SCROLLBACK);
            let mut clients = Vec::new();
            daemon_loop(
                &listener,
                None,
                &name,
                &group,
                &child,
                &mut term,
                &mut clients,
                &command.join("\0"),
                "",
                kind,
                None,
                None,
            )
        });
        TestDaemon {
            base: cfg.socket_dir.clone(),
            socket,
            child_pid,
            master,
            thread: Some(thread),
            torn_down: false,
        }
    }

    #[test]
    fn info_reports_the_kind_the_session_was_created_with() {
        let cfg = Config {
            socket_dir: temp_base(),
            group: "default".into(),
        };
        let handle = spawn_test_daemon(&cfg, "k1", None, SessionKind::Anonymous);
        let probe = crate::session::probe_session(&cfg.socket_path("k1").unwrap()).unwrap();
        assert_eq!(probe.info.kind, SessionKind::Anonymous);
        assert_eq!(handle.shutdown(), caps::SessionEnd::Killed);
    }

    fn new_term() -> Terminal {
        Terminal::with_scrollback(5, 20, 100)
    }

    /// Feeds chunks through a fresh filter+model, returning the broadcast.
    fn run_filter(term: &mut Terminal, chunks: &[&[u8]]) -> Vec<u8> {
        let mut filter = ScreenSwitchFilter::default();
        let mut out = Vec::new();
        for chunk in chunks {
            filter.feed(term, chunk, &mut out);
        }
        out
    }

    fn row_text(t: &Terminal, r: u16) -> String {
        t.screen().row(r).unwrap().text(true)
    }

    fn assert_mirrors(session: &Terminal, outer: &Terminal) {
        for r in 0..session.rows() {
            assert_eq!(
                row_text(session, r),
                row_text(outer, r),
                "row {r} diverged"
            );
        }
        assert_eq!(session.cursor().row, outer.cursor().row, "cursor row");
        assert_eq!(session.cursor().col, outer.cursor().col, "cursor col");
    }

    #[test]
    fn passthrough_without_switches_is_byte_identical() {
        let mut term = new_term();
        let input: &[u8] = b"hello \x1b[31mred\x1b[0m\r\n\x1b]2;title\x07done";
        let out = run_filter(&mut term, &[input]);
        assert_eq!(out, input);
    }

    #[test]
    fn fast_path_plain_text_is_byte_identical() {
        let mut term = new_term();
        let input: &[u8] = b"no escapes at all, just text\r\n";
        let out = run_filter(&mut term, &[input]);
        assert_eq!(out, input);
    }

    #[test]
    fn alt_switch_is_excised_and_substituted() {
        let mut term = new_term();
        let out = run_filter(&mut term, &[b"abc\x1b[?1049hdef"]);
        let s = String::from_utf8_lossy(&out);
        assert!(s.starts_with("abc"), "{s:?}");
        assert!(s.ends_with("def"), "{s:?}");
        assert!(!s.contains("\x1b[?1049"), "raw switch leaked: {s:?}");
        assert!(s.contains("\x1b[2J"), "no repaint substitute: {s:?}");
    }

    #[test]
    fn switch_split_across_reads_is_still_excised() {
        let mut term = new_term();
        let out = run_filter(&mut term, &[b"x\x1b[?10", b"49h", b"y"]);
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("\x1b[?1049"), "raw switch leaked: {s:?}");
        assert!(s.starts_with('x') && s.ends_with('y'), "{s:?}");
    }

    #[test]
    fn co_set_modes_survive_the_strip() {
        let mut term = new_term();
        let out = run_filter(&mut term, &[b"\x1b[?1049;2004h"]);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("\x1b[?2004h"), "co-set mode lost: {s:?}");
        assert!(!s.contains("1049"), "{s:?}");
    }

    #[test]
    fn non_switch_private_modes_pass_raw() {
        let mut term = new_term();
        let out = run_filter(&mut term, &[b"\x1b[?2004h\x1b[?1000h\x1b[?1049$p"]);
        assert_eq!(out, b"\x1b[?2004h\x1b[?1000h\x1b[?1049$p");
    }

    #[test]
    fn outer_terminal_mirrors_session_through_a_vim_cycle() {
        // `outer` is the attached client's real terminal: it receives the
        // filtered broadcast and must show the same screen as the session
        // model at every step, without ever switching its own buffers.
        let mut session = new_term();
        let mut outer = new_term();
        let mut filter = ScreenSwitchFilter::default();
        let mut play = |session: &mut Terminal, outer: &mut Terminal, bytes: &[u8]| {
            let mut filter_out = Vec::new();
            filter.feed(session, bytes, &mut filter_out);
            outer.process(&filter_out);
        };
        play(&mut session, &mut outer, b"$ ls\r\nfile.txt\r\n$ vim\x1b[1;7H");
        assert_mirrors(&session, &outer);
        play(
            &mut session,
            &mut outer,
            b"\x1b[?1049h\x1b[2J\x1b[H~ VIM ~\x1b[2;1H\x1b[?2004h",
        );
        assert_mirrors(&session, &outer);
        assert!(session.is_alt_screen());
        assert!(!outer.is_alt_screen(), "outer must never switch buffers");
        play(&mut session, &mut outer, b"\x1b[?2004l\x1b[?1049l");
        assert_mirrors(&session, &outer);
        assert!(!outer.is_alt_screen());
        assert_eq!(row_text(&outer, 0), "$ ls");
        assert_eq!(row_text(&outer, 1), "file.txt");
    }

    #[test]
    fn ris_is_substituted_with_reset_preamble() {
        let mut term = new_term();
        let out = run_filter(&mut term, &[b"junk\x1bcafter"]);
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("\x1bc"), "raw RIS leaked: {s:?}");
        assert!(s.contains("\x1b[!p"), "no soft reset in substitute: {s:?}");
        assert!(s.contains("\x1b[2J"), "no repaint after reset: {s:?}");
        assert!(s.ends_with("after"), "{s:?}");
    }

    fn test_client_conn() -> ClientConn {
        // A connected pair gives the struct a real fd without a daemon; only
        // the parse-side fields (rows/cols/caps) are exercised here.
        let (stream, _peer) = UnixStream::pair().unwrap();
        ClientConn {
            stream,
            read_buf: FrameBuffer::new(),
            write_buf: Vec::new(),
            rows: 0,
            cols: 0,
            caps: Vec::new(),
            producer: None,
            lossy: false,
            coalesce: false,
            coalesce_off: false,
            pending_frame_start: None,
            sb_floor: 0,
            acked_sb_total: 0,
            bytes_drained: 0,
            last_drain_ms: 0,
            hiwater_mb: 0,
            echo_flag: 0,
            overlay_flag: 0,
            record: introspect::ClientRecord::default(),
            record_at: 0,
            attach_pid: None,
            last_input_ms: 0,
            wants_activity: false,
            activity_now: None,
            activity_sent: None,
            kind: SessionKind::Unknown,
            kind_sent: false,
            wants_push_cmd: false,
            push_offered: false,
            visible_shaped_for: None,
            regeometry_keyframe: None,
            pacing: None,
            init_applied: false,
        }
    }

    /// A baseline (legacy) viewport: `test_client_conn` after a bare 4-byte
    /// Init, so it has no caps and no producer but IS attached.
    fn baseline_conn() -> ClientConn {
        let mut c = test_client_conn();
        c.apply_init(&ipc::encode_resize(24, 80));
        c
    }

    #[test]
    fn init_with_cap_table_records_protocol_version_and_resizes() {
        let mut c = test_client_conn();
        let mut payload = ipc::encode_resize(24, 80).to_vec();
        payload.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));

        let resized = c.apply_init(&payload);

        assert!(resized, "resize prefix must still size the PTY");
        assert_eq!((c.rows, c.cols), (24, 80), "size decoded from the 4-byte prefix");
        assert!(
            caps::find(&c.caps, caps::CAP_PROTOCOL_VERSION).is_some(),
            "PROTOCOL_VERSION must be recorded from the trailing table: {:?}",
            c.caps
        );
    }

    #[test]
    fn bare_init_records_empty_caps_and_resizes() {
        let mut c = test_client_conn();

        let resized = c.apply_init(&ipc::encode_resize(10, 40));

        assert!(resized, "a baseline 4-byte Init still resizes");
        assert_eq!((c.rows, c.cols), (10, 40));
        assert!(c.caps.is_empty(), "no trailing table => no caps");
    }

    #[test]
    fn bare_reinit_preserves_already_negotiated_caps() {
        // SIGCONT resume re-Inits with a bare 4-byte payload; that must not
        // wipe the caps a cap-extended Init negotiated earlier.
        let mut c = test_client_conn();
        let mut first = ipc::encode_resize(24, 80).to_vec();
        first.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        c.apply_init(&first);

        c.apply_init(&ipc::encode_resize(30, 100));

        assert_eq!((c.rows, c.cols), (30, 100), "the re-Init still resizes");
        assert!(
            caps::find(&c.caps, caps::CAP_PROTOCOL_VERSION).is_some(),
            "caps survive a bare re-Init"
        );
    }

    #[test]
    fn strict_decode_resize_rejects_cap_extended_payload() {
        // Why the client re-asserts its size via Tag::Resize after a
        // cap-extended Init: a pre-#100 daemon ran decode_resize on the whole
        // payload, which rejects anything but exactly 4 bytes and would drop
        // the initial size.
        let mut payload = ipc::encode_resize(24, 80).to_vec();
        payload.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        assert!(ipc::decode_resize(&payload).is_none());
    }

    #[test]
    fn strip_alt_screen_params_shapes() {
        assert_eq!(strip_alt_screen_params(b"\x1b[?1049h"), None);
        assert_eq!(strip_alt_screen_params(b"\x1b[?47l"), None);
        // Leading zeros still match numerically.
        assert_eq!(strip_alt_screen_params(b"\x1b[?0047h"), None);
        assert_eq!(
            strip_alt_screen_params(b"\x1b[?1049;2004h").as_deref(),
            Some(b"\x1b[?2004h".as_slice())
        );
        assert_eq!(
            strip_alt_screen_params(b"\x1b[?2004;1049;1000l").as_deref(),
            Some(b"\x1b[?2004;1000l".as_slice())
        );
        // Unexpected shapes are dropped whole (the repaint follows anyway).
        assert_eq!(strip_alt_screen_params(b"\x1b[?10\x0749h"), None);
        assert_eq!(strip_alt_screen_params(b"\x1bc"), None);
    }

    // ---- Task 1.4: per-client frame production (RFC 0008) ----

    use crate::remote::framesync::{ApplyOutcome, DumpDiff, FrameApplier};
    use crate::remote::sync::{FrameBody, ScrollbackRing};

    /// A frame-capable client: its `Tag::Init` carries an RFC 0001 cap table, so
    /// with the gate on `maybe_enable_frames` constructs its `FrameProducer`.
    /// The peer end is returned so the socket stays open for the test's lifetime.
    fn frame_capable_conn(rows: u16, cols: u16) -> (ClientConn, UnixStream) {
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut c = ClientConn {
            stream,
            read_buf: FrameBuffer::new(),
            write_buf: Vec::new(),
            rows: 0,
            cols: 0,
            caps: Vec::new(),
            producer: None,
            lossy: false,
            coalesce: false,
            coalesce_off: false,
            pending_frame_start: None,
            sb_floor: 0,
            acked_sb_total: 0,
            bytes_drained: 0,
            last_drain_ms: 0,
            hiwater_mb: 0,
            echo_flag: 0,
            overlay_flag: 0,
            record: introspect::ClientRecord::default(),
            record_at: 0,
            attach_pid: None,
            last_input_ms: 0,
            wants_activity: false,
            activity_now: None,
            activity_sent: None,
            kind: SessionKind::Unknown,
            kind_sent: false,
            wants_push_cmd: false,
            push_offered: false,
            visible_shaped_for: None,
            regeometry_keyframe: None,
            pacing: None,
            init_applied: false,
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        c.apply_init(&init);
        c.maybe_enable_frames();
        (c, peer)
    }

    /// A frame client advertising `CAP_KITTY_KEYBOARD` with `flags` (RFC 0010).
    fn kitty_frame_conn(rows: u16, cols: u16, flags: u8) -> (ClientConn, UnixStream) {
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut c = ClientConn {
            stream,
            read_buf: FrameBuffer::new(),
            write_buf: Vec::new(),
            rows: 0,
            cols: 0,
            caps: Vec::new(),
            producer: None,
            lossy: false,
            coalesce: false,
            coalesce_off: false,
            pending_frame_start: None,
            sb_floor: 0,
            acked_sb_total: 0,
            bytes_drained: 0,
            last_drain_ms: 0,
            hiwater_mb: 0,
            echo_flag: 0,
            overlay_flag: 0,
            record: introspect::ClientRecord::default(),
            record_at: 0,
            attach_pid: None,
            last_input_ms: 0,
            wants_activity: false,
            activity_now: None,
            activity_sent: None,
            kind: SessionKind::Unknown,
            kind_sent: false,
            wants_push_cmd: false,
            push_offered: false,
            visible_shaped_for: None,
            regeometry_keyframe: None,
            pacing: None,
            init_applied: false,
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[caps::Cap {
            id: caps::CAP_KITTY_KEYBOARD,
            payload: vec![flags],
        }])));
        c.apply_init(&init);
        c.maybe_enable_frames();
        (c, peer)
    }

    // ---- FDR 0012 (RFC 0008 §3.1): the switch router ----

    #[test]
    fn switch_routes_to_most_recent_input_excluding_requester() {
        let mut a = test_client_conn();
        a.last_input_ms = 100;
        let mut b = test_client_conn();
        b.last_input_ms = 200;
        let requester = test_client_conn(); // never typed (it only requested)
        let clients = vec![a, b, requester];
        // b typed most recently; the requester (index 2) is excluded.
        assert_eq!(switch_route_target(&clients, 2), Some(1));
        // Were the recent typist itself the requester, the other viewer wins.
        assert_eq!(switch_route_target(&clients, 1), Some(0));
    }

    #[test]
    fn switch_falls_back_to_latest_attached_and_none_when_alone() {
        // Nobody has typed: the latest-attached candidate (highest index)
        // wins — accept order, the freshest viewport.
        let clients = vec![test_client_conn(), test_client_conn(), test_client_conn()];
        assert_eq!(switch_route_target(&clients, 0), Some(2));
        assert_eq!(switch_route_target(&clients, 2), Some(1));
        // Only the requester is connected: nothing to route to.
        let lone = vec![test_client_conn()];
        assert_eq!(switch_route_target(&lone, 0), None);
    }

    // ---- RFC 0010: terminal query passthrough / kitty keyboard negotiation ----

    #[test]
    fn query_policy_no_clients_answers() {
        // No clients: the model is authoritative — answer verbatim.
        assert_eq!(query_policy(&[]), QueryPolicy::Answer);
    }

    #[test]
    fn query_policy_legacy_client_is_silent() {
        // A legacy (non-frame) client's real terminal answers the raw query, so
        // the daemon must stay silent — no double reply.
        let legacy = baseline_conn(); // no producer, no caps
        assert_eq!(query_policy(std::slice::from_ref(&legacy)), QueryPolicy::Silent);
    }

    #[test]
    fn query_policy_ignores_a_connection_before_its_init() {
        // posh#239: a not-yet-Init connection (a viewport mid-attach, or a
        // `posh list` probe that never Inits) is sent no output, so its
        // terminal never sees the query and must not silence the daemon.
        let pending = test_client_conn();
        assert_eq!(query_policy(std::slice::from_ref(&pending)), QueryPolicy::Answer);
        let (frame, _pf) = kitty_frame_conn(24, 80, 0);
        assert_eq!(query_policy(&[frame, pending]), QueryPolicy::Answer);
    }

    #[test]
    fn query_policy_kitty_frame_client_answers() {
        // Every frame client's terminal supports kitty ⇒ answer verbatim (the
        // kitty reply's presence lets the app enable the protocol; its value is
        // the model's own current flags, unchanged).
        let (c, _p) = kitty_frame_conn(24, 80, 0);
        assert_eq!(query_policy(std::slice::from_ref(&c)), QueryPolicy::Answer);
    }

    #[test]
    fn query_policy_non_kitty_frame_client_suppresses_kitty() {
        // A frame client whose terminal does NOT support kitty ⇒ suppress the
        // kitty reply (so the app concludes unsupported) but keep DA/DSR.
        let (c, _p) = frame_capable_conn(24, 80); // no CAP_KITTY_KEYBOARD
        assert_eq!(
            query_policy(std::slice::from_ref(&c)),
            QueryPolicy::SuppressKitty
        );
    }

    #[test]
    fn query_policy_all_frame_clients_must_support_kitty() {
        // Every frame client must advertise for the kitty reply to be spoken;
        // one non-kitty terminal ⇒ suppress (don't claim support it can't do).
        let (adv, _p1) = kitty_frame_conn(24, 80, 0);
        let (plain, _p2) = frame_capable_conn(24, 80);
        let clients = vec![adv, plain];
        assert_eq!(query_policy(&clients), QueryPolicy::SuppressKitty);

        // Both kitty ⇒ answer.
        let (a, _pa) = kitty_frame_conn(24, 80, 0);
        let (b, _pb) = kitty_frame_conn(24, 80, 0);
        assert_eq!(query_policy(&[a, b]), QueryPolicy::Answer);
    }

    #[test]
    fn query_policy_mixed_frame_and_legacy_is_silent() {
        // A legacy client present ⇒ silent regardless of the frame clients'
        // caps (the legacy terminal answers the raw query).
        let (frame, _pf) = kitty_frame_conn(24, 80, 0);
        let legacy = baseline_conn();
        let clients = vec![frame, legacy];
        assert_eq!(query_policy(&clients), QueryPolicy::Silent);
    }

    #[test]
    fn strip_kitty_reply_removes_only_the_kitty_reply() {
        // The kitty reply is dropped; DA (…c) and DSR (…R) replies survive so
        // the app still gets its device-attribute / cursor answers.
        let responses = b"\x1b[?31u\x1b[?62;22c\x1b[5;9R";
        assert_eq!(strip_kitty_reply(responses), b"\x1b[?62;22c\x1b[5;9R");
    }

    #[test]
    fn strip_kitty_reply_leaves_non_kitty_untouched() {
        // No kitty reply present ⇒ buffer returned verbatim.
        let responses = b"\x1b[?62;22c";
        assert_eq!(strip_kitty_reply(responses), responses);
    }

    /// Fills the screen so a later one-character edit is a clear diff win (a
    /// `Diff`, not a `Full`) — the diff-economics fixture the producer needs.
    fn fill_screen(term: &mut Terminal) {
        term.process(b"\x1b[2J\x1b[H");
        for i in 0..20u8 {
            term.process(format!("line {i:02} of representative session content\r\n").as_bytes());
        }
    }

    /// Decode the `Tag::Frame` `ServerFrame` bodies queued in a client's write
    /// buffer, asserting every queued record is a `Tag::Frame` (no `Tag::Output`
    /// leaked in for a frame-capable client).
    fn decode_frame_bodies(write_buf: &[u8]) -> Vec<FrameBody> {
        let mut fb = FrameBuffer::new();
        fb.feed(write_buf);
        let mut bodies = Vec::new();
        while let Some(frame) = fb.next().unwrap() {
            assert_eq!(frame.tag, Tag::Frame, "frame-capable client must receive Tag::Frame");
            bodies.push(ServerFrame::decode(&frame.payload).unwrap().body);
        }
        bodies
    }

    /// Reconstruct a frame-capable client's view: apply its queued `Tag::Frame`
    /// stream through the `DumpDiff` applier into a scratch `Terminal` and return
    /// the rendered `Snapshot`. This is the real client-side codec, so a passing
    /// equality against the daemon's own `Snapshot` is a genuine round-trip, not
    /// a tautology.
    fn reconstruct(write_buf: &[u8], rows: u16, cols: u16) -> Snapshot {
        reconstruct_seeded(write_buf, rows, cols, &[])
    }

    /// Reconstruct a coalescing client's view when its `write_buf` holds a diff
    /// whose base was already applied+acked and coalesced OUT of the buffer
    /// (posh#137): call with `base_dump` = the acked base the client still holds
    /// locally, and the applier is seeded with it (and its rendered screen) before
    /// the queued frame(s) apply — the real client state after acking a frame the
    /// daemon then coalesced away.
    fn reconstruct_coalesced(write_buf: &[u8], rows: u16, cols: u16, base_dump: &[u8]) -> Snapshot {
        reconstruct_seeded(write_buf, rows, cols, base_dump)
    }

    /// Apply a `write_buf`'s queued frames onto a scratch terminal seeded with
    /// `base_dump` (empty = a fresh blank screen, the plain [`reconstruct`] case;
    /// non-empty = the coalesced case, [`reconstruct_coalesced`]).
    fn reconstruct_seeded(write_buf: &[u8], rows: u16, cols: u16, base_dump: &[u8]) -> Snapshot {
        Snapshot::from_term(&mirror_frames(write_buf, rows, cols, base_dump))
    }

    /// The scratch terminal [`reconstruct_seeded`] renders: a RING-LESS mirror
    /// of `rows` x `cols`, seeded with `base_dump`, with `write_buf`'s queued
    /// frames applied — for tests that read the mirror's rows, not a Snapshot.
    fn mirror_frames(write_buf: &[u8], rows: u16, cols: u16, base_dump: &[u8]) -> Terminal {
        let mut fb = FrameBuffer::new();
        fb.feed(write_buf);
        let mut term = Terminal::with_scrollback(rows, cols, 0);
        term.process(base_dump);
        let mut applier = DumpDiff;
        let mut applied: Vec<u8> = base_dump.to_vec();
        while let Some(frame) = fb.next().unwrap() {
            assert_eq!(frame.tag, Tag::Frame, "frame-capable client must receive Tag::Frame");
            let body = ServerFrame::decode(&frame.payload).unwrap().body;
            match applier.apply(rows, cols, &applied, &mut term, &body) {
                ApplyOutcome::Advanced { dump } => applied = dump,
                ApplyOutcome::AdvancedNoDump | ApplyOutcome::NoChange => {}
                ApplyOutcome::ReackAndWait => panic!("DumpDiff could not apply a queued body"),
            }
        }
        term
    }

    #[test]
    fn producer_constructed_only_when_capable() {
        // Capable (cap table on Init) => producer.
        let (capable, _p) = frame_capable_conn(24, 80);
        assert!(capable.producer.is_some(), "cap table => producer");

        // NOT capable (bare Init) => none — the one remaining skew axis now
        // that the daemon-side gate is retired (posh#171).
        let mut baseline = test_client_conn();
        baseline.apply_init(&ipc::encode_resize(24, 80));
        baseline.maybe_enable_frames();
        assert!(baseline.producer.is_none(), "a non-capable client never gets a producer");
    }

    #[test]
    fn frames_carry_the_daemons_echo_flag() {
        // FDR 0006: the active pty's ECHO state rides every daemon frame —
        // visible AND scrollback — as FLAG_ECHO (`echo_flag`, refreshed per
        // loop iteration). Pre-fix the daemon never set the flag, so on the
        // relay path (the default bootstrap) the client's optimistic-echo
        // gate read "echo off" for entire sessions and the model predicted
        // nothing — exactly where the slow-link escalation selects it.
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = frame_capable_conn(rows, cols);
        c.echo_flag = crate::remote::sync::FLAG_ECHO;
        assert!(c.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            term.is_alt_screen(),
            (rows, cols),
        ));
        let mut fb = FrameBuffer::new();
        fb.feed(&c.write_buf);
        let mut saw = 0;
        while let Some(frame) = fb.next().unwrap() {
            let decoded = ServerFrame::decode(&frame.payload).unwrap();
            assert_ne!(
                decoded.flags & crate::remote::sync::FLAG_ECHO,
                0,
                "every frame carries the stamped FLAG_ECHO"
            );
            saw += 1;
        }
        assert!(saw > 0, "a frame was actually produced");

        // And echo-off (a password prompt) stamps it back off.
        c.write_buf.clear();
        c.echo_flag = 0;
        term.process(b"x");
        assert!(c.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            false,
            (rows, cols),
        ));
        let mut fb = FrameBuffer::new();
        fb.feed(&c.write_buf);
        while let Some(frame) = fb.next().unwrap() {
            let decoded = ServerFrame::decode(&frame.payload).unwrap();
            assert_eq!(decoded.flags & crate::remote::sync::FLAG_ECHO, 0);
        }
    }

    #[test]
    fn frames_carry_the_overlay_flag_while_the_shell_overlay_is_up() {
        // FDR 0008 / posh#178: while the escape-to-shell overlay is up the
        // daemon stamps FLAG_OVERLAY on every frame, so a roaming client
        // (through the relay/mux, which forwards flags verbatim) clears its
        // "opening shell…" notice. Pre-fix the daemon never set it, so a
        // channel-attached client's notice lingered forever.
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = frame_capable_conn(rows, cols);
        c.overlay_flag = crate::remote::sync::FLAG_OVERLAY;
        assert!(c.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            false,
            (rows, cols),
        ));
        let mut fb = FrameBuffer::new();
        fb.feed(&c.write_buf);
        let mut saw = 0;
        while let Some(frame) = fb.next().unwrap() {
            let decoded = ServerFrame::decode(&frame.payload).unwrap();
            assert_ne!(
                decoded.flags & crate::remote::sync::FLAG_OVERLAY,
                0,
                "every frame carries FLAG_OVERLAY while the overlay is up"
            );
            saw += 1;
        }
        assert!(saw > 0);

        // Overlay closed ⇒ the flag clears.
        c.write_buf.clear();
        c.overlay_flag = 0;
        term.process(b"y");
        assert!(c.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            term.is_alt_screen(),
            (rows, cols),
        ));
        let mut fb = FrameBuffer::new();
        fb.feed(&c.write_buf);
        while let Some(frame) = fb.next().unwrap() {
            let decoded = ServerFrame::decode(&frame.payload).unwrap();
            assert_eq!(decoded.flags & crate::remote::sync::FLAG_OVERLAY, 0);
        }
    }

    #[test]
    fn maybe_enable_frames_is_idempotent_across_reinit() {
        // A bare re-Init (SIGCONT resume) must NOT rebuild an established
        // producer — that would reset frame numbering to 0 and stale the
        // consumer's acked base. Mirrors the cap-idempotency test.
        let (mut c, _peer) = frame_capable_conn(24, 80);
        // Advance the producer past frame 0 so a reset would be observable.
        assert!(c.queue_frame(b"dump".to_vec(), Snapshot::blank(24, 80), false, (24, 80)));
        let num_before = c.producer.as_ref().unwrap().current_num();
        assert_eq!(num_before, 1, "producing one frame must advance current_num to 1");

        c.maybe_enable_frames();

        assert!(c.producer.is_some(), "the producer survives a re-Init");
        assert_eq!(
            c.producer.as_ref().unwrap().current_num(),
            num_before,
            "a re-Init must preserve frame numbering, not reset to a fresh producer"
        );
    }

    #[test]
    fn frame_capable_client_receives_reconstructable_frames() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);

        let (mut c, _peer) = frame_capable_conn(rows, cols);
        assert!(c.producer.is_some());

        // Replay on attach: the producer's first frame is a Full keyframe.
        assert!(c.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            term.is_alt_screen(),
            (rows, cols),
        ));

        // A later visible change broadcasts a frame against the acked base.
        // Append at the cursor (screen bottom) so the long shared prefix makes
        // the prefix/suffix diff a clear win — i.e. a Diff, not a Full.
        term.process(b"appended output");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw bytes ignored>");

        let bodies = decode_frame_bodies(&c.write_buf);
        assert_eq!(bodies.len(), 2, "one replay keyframe + one broadcast frame");
        assert!(
            matches!(bodies[0], FrameBody::Full(_)),
            "fresh attach => Full keyframe, got {:?}",
            bodies[0]
        );
        assert!(
            matches!(bodies[1], FrameBody::Diff { base: 1, .. }),
            "established base => Diff against frame 1, got {:?}",
            bodies[1]
        );

        // The applied frames reconstruct the daemon's screen exactly.
        assert_eq!(
            reconstruct(&c.write_buf, rows, cols),
            Snapshot::from_term(&term),
            "client-applied frames must reproduce the daemon screen"
        );
    }

    #[test]
    fn per_client_producers_diff_against_independent_bases() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);

        // Client A attaches first and gets its Full keyframe (frame 1).
        let (mut a, _pa) = frame_capable_conn(rows, cols);
        assert!(a.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            term.is_alt_screen(),
            (rows, cols),
        ));

        // A visible change (appended at the cursor so A's diff is a clear win);
        // then client B attaches AFTER it. B's first-ever frame is a Full of the
        // NEW screen, while A — in the same broadcast — gets a Diff against its
        // own acked base.
        term.process(b"appended output");
        let (mut b, _pb) = frame_capable_conn(rows, cols);
        assert!(b.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            term.is_alt_screen(),
            (rows, cols),
        ));
        broadcast_output(std::slice::from_mut(&mut a), &term, b"x");

        let a_bodies = decode_frame_bodies(&a.write_buf);
        let b_bodies = decode_frame_bodies(&b.write_buf);
        assert!(matches!(a_bodies[0], FrameBody::Full(_)));
        assert!(
            matches!(a_bodies[1], FrameBody::Diff { base: 1, .. }),
            "A's established producer diffs, got {:?}",
            a_bodies[1]
        );
        assert_eq!(b_bodies.len(), 1, "B has only its replay keyframe");
        assert!(
            matches!(b_bodies[0], FrameBody::Full(_)),
            "B's first-ever frame is a Full regardless of A's state, got {:?}",
            b_bodies[0]
        );

        // Both clients reconstruct the same final screen.
        assert_eq!(reconstruct(&a.write_buf, rows, cols), Snapshot::from_term(&term));
        assert_eq!(reconstruct(&b.write_buf, rows, cols), Snapshot::from_term(&term));
    }

    #[test]
    fn non_capable_client_gets_output_even_with_gate_on() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 100);
        term.process(b"content");

        // No cap table in the Init => baseline peer; gate ON.
        let mut c = test_client_conn();
        c.apply_init(&ipc::encode_resize(rows, cols));
        c.maybe_enable_frames();
        assert!(c.producer.is_none(), "a non-capable client never gets a producer");

        let raw = b"raw broadcast bytes";
        broadcast_output(std::slice::from_mut(&mut c), &term, raw);

        let mut fb = FrameBuffer::new();
        fb.feed(&c.write_buf);
        let frame = fb.next().unwrap().expect("one queued record");
        assert_eq!(frame.tag, Tag::Output);
        assert_eq!(frame.payload, raw);
    }

    #[test]
    fn mixed_clients_each_get_their_own_transport() {
        // One frame-capable + one baseline client in the same broadcast: the
        // capable one gets Tag::Frame, the baseline one gets the raw Tag::Output
        // — neither regresses the other.
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);

        let (capable, _pc) = frame_capable_conn(rows, cols);
        let mut baseline = test_client_conn();
        baseline.apply_init(&ipc::encode_resize(rows, cols));
        baseline.maybe_enable_frames();
        assert!(baseline.producer.is_none());

        let mut clients = vec![capable, baseline];
        let raw = b"raw delta";
        broadcast_output(&mut clients, &term, raw);

        // Capable client => a single Tag::Frame (a Full, since fresh).
        let cap_bodies = decode_frame_bodies(&clients[0].write_buf);
        assert_eq!(cap_bodies.len(), 1);
        assert!(matches!(cap_bodies[0], FrameBody::Full(_)));

        // Baseline client => Tag::Output with the raw bytes.
        let mut fb = FrameBuffer::new();
        fb.feed(&clients[1].write_buf);
        let frame = fb.next().unwrap().expect("one queued record");
        assert_eq!(frame.tag, Tag::Output);
        assert_eq!(frame.payload, raw);
    }

    // ---- Task 1.6: 4-way session-socket version-skew matrix (RFC 0008 §6) ----

    /// Assert a client's whole queued backlog is a single `Tag::Output` record
    /// carrying `expected` verbatim — the baseline (`Tag::Output`) outcome for
    /// every skew cell except new×new.
    fn assert_single_output(write_buf: &[u8], expected: &[u8]) {
        let mut fb = FrameBuffer::new();
        fb.feed(write_buf);
        let frame = fb.next().unwrap().expect("one queued record");
        assert_eq!(frame.tag, Tag::Output, "expected the baseline Tag::Output");
        assert_eq!(frame.payload, expected, "Tag::Output must carry the raw broadcast bytes unchanged");
        assert!(fb.next().unwrap().is_none(), "exactly one queued record");
    }

    /// The socket version-skew matrix of RFC 0008 §6, as a CURRENT daemon can
    /// exercise it: "old client" is a bare 4-byte Init with no capability table.
    /// The "old daemon" rows are a genuinely older binary — the daemon-side
    /// `POSH_SESSION_FRAMES` gate that used to model them is retired (posh#171),
    /// so cell 3 here pins only the client-side property that makes that row
    /// work: the size a cap-extended Init carries is recoverable by an old
    /// daemon through the Tag::Resize re-assertion.
    ///
    /// | daemon | client (Init)        | screen output |
    /// |--------|----------------------|---------------|
    /// | new    | new (caps)           | `Tag::Frame`  |
    /// | new    | old (bare)           | `Tag::Output` |
    /// | old    | new (caps + Resize)  | `Tag::Output` (size via Resize) |
    /// | old    | old (bare)           | unchanged baseline (not modelled) |
    #[test]
    fn four_way_socket_version_skew_matrix() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let raw = b"raw screen-output bytes";

        // Cell 1 — new daemon (gate ON) × new client (cap table) ⇒ Tag::Frame.
        // The frame cap is observed, so the daemon negotiates frames and serves
        // the screen as a posh-proto ServerFrame (a Full keyframe on first paint).
        {
            let (mut c, _peer) = frame_capable_conn(rows, cols);
            assert!(c.producer.is_some(), "cell 1: gate on + cap table ⇒ producer");
            broadcast_output(std::slice::from_mut(&mut c), &term, raw);
            let bodies = decode_frame_bodies(&c.write_buf); // also asserts every record is Tag::Frame
            assert_eq!(bodies.len(), 1, "cell 1: one screen-output frame");
            assert!(
                matches!(bodies[0], FrameBody::Full(_)),
                "cell 1: a fresh frame-capable attach ⇒ Full keyframe, got {:?}",
                bodies[0]
            );
        }

        // Cell 2 — new daemon (gate ON) × old client (bare Init) ⇒ Tag::Output.
        // The daemon never observes a frame cap, so even with the gate on it
        // builds no producer and serves the baseline raw dump.
        {
            let mut c = test_client_conn();
            c.apply_init(&ipc::encode_resize(rows, cols));
            c.maybe_enable_frames();
            assert!(c.producer.is_none(), "cell 2: no cap table ⇒ no producer even with gate on");
            broadcast_output(std::slice::from_mut(&mut c), &term, raw);
            assert_single_output(&c.write_buf, raw);
        }

        // Cell 3 (the critical cross-version cell) — old daemon × new client
        // (cap-extended Init + the Tag::Resize re-assertion). An old daemon is a
        // real older binary (not a mode of this one), so what is pinned here is
        // the size property that makes the row work.
        {
            let cap_extended_init = {
                let mut init = ipc::encode_resize(rows, cols).to_vec();
                init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
                init
            };

            // The cross-version size property, pinned on the REAL decoder applied
            // to the GENUINE payloads (not a field write-then-read tautology):
            //
            // (1) An OLD daemon decodes resize from the WHOLE Init payload and
            // rejects any non-4-byte length, so the cap-extended Init's size is
            // dropped on its floor — which is precisely why the new client must
            // re-assert via Tag::Resize.
            assert!(
                ipc::decode_resize(&cap_extended_init).is_none(),
                "cell 3: an old daemon's strict whole-payload decode must drop the cap-extended Init's size"
            );
            // (2) The 4-byte Tag::Resize the new client re-asserts after the Init
            // decodes to the right dims — every daemon version honors Tag::Resize,
            // so even an old daemon that dropped the Init size recovers it here.
            let resize_payload = ipc::encode_resize(rows, cols);
            assert_eq!(
                ipc::decode_resize(&resize_payload),
                Some((rows, cols)),
                "cell 3: the client's Tag::Resize re-assertion must carry the recoverable size"
            );
        }

        // Cell 4 — old daemon × old client: the unchanged baseline, exercised by
        // an older binary, not modelled here.
    }

    // ---- Task 2.5a: daemon produces scrollback frames (RFC 0002) ----

    /// A frame-capable client that ALSO advertises `CAP_SCROLLBACK` (RFC 0002
    /// §1), so it both frames the screen AND wants scrolled-off rows synced.
    fn scrollback_capable_conn(rows: u16, cols: u16) -> (ClientConn, UnixStream) {
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut c = ClientConn {
            stream,
            read_buf: FrameBuffer::new(),
            write_buf: Vec::new(),
            rows: 0,
            cols: 0,
            caps: Vec::new(),
            producer: None,
            lossy: false,
            coalesce: false,
            coalesce_off: false,
            pending_frame_start: None,
            sb_floor: 0,
            acked_sb_total: 0,
            bytes_drained: 0,
            last_drain_ms: 0,
            hiwater_mb: 0,
            echo_flag: 0,
            overlay_flag: 0,
            record: introspect::ClientRecord::default(),
            record_at: 0,
            attach_pid: None,
            last_input_ms: 0,
            wants_activity: false,
            activity_now: None,
            activity_sent: None,
            kind: SessionKind::Unknown,
            kind_sent: false,
            wants_push_cmd: false,
            push_offered: false,
            visible_shaped_for: None,
            regeometry_keyframe: None,
            pacing: None,
            init_applied: false,
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[caps::Cap {
            id: caps::CAP_SCROLLBACK,
            payload: vec![0],
        }])));
        c.apply_init(&init);
        c.maybe_enable_frames();
        (c, peer)
    }

    /// Push `n` lines through the terminal so more rows than the screen holds
    /// scroll off the top into the primary scrollback ring.
    fn scroll_off(term: &mut Terminal, n: u16) {
        for i in 0..n {
            term.process(format!("scrollback row {i:03}\r\n").as_bytes());
        }
    }

    /// The core Task 2.5a property: a scrollback-capable client, framed with the
    /// gate on, receives the scrolled-off rows as `FrameBody::Scrollback` bodies,
    /// and a `ScrollbackRing` fed those bodies holds exactly the daemon's
    /// `dump_scrollback_row(i)` for every scrolled-off row. Attach happens while
    /// the daemon scrollback is empty (`sb_floor` = 0), so accumulation is
    /// forward-only from there — every row scrolled off after attach is synced.
    #[test]
    fn scrollback_capable_client_rings_the_daemons_scrolled_off_rows() {
        let (rows, cols) = (5u16, 24u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);

        let (mut c, _peer) = scrollback_capable_conn(rows, cols);
        assert!(c.producer.is_some(), "caps ⇒ producer");
        assert!(c.wants_scrollback(), "the client advertised CAP_SCROLLBACK");

        // Attach replay: the Full keyframe establishes the acked visible base
        // (frame 1) that scrollback bodies thread off. The term's scrollback is
        // empty here, so sb_floor stays 0 and later growth is fully synced.
        assert!(c.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            term.is_alt_screen(),
            (rows, cols),
        ));

        // Scroll many rows off the top, then broadcast the growth.
        scroll_off(&mut term, 12);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");

        let scrolled = term.primary_scrollback_len();
        assert!(scrolled > 0, "the output must have scrolled rows into scrollback");

        // Reconstruct the client's ring from the Scrollback bodies it received.
        // `decode_frame_bodies` also asserts every queued record is a Tag::Frame.
        let mut ring = ScrollbackRing::new(1000);
        let mut sb_frames = 0;
        let mut saw_visible = false;
        for body in decode_frame_bodies(&c.write_buf) {
            match body {
                FrameBody::Scrollback { base, rows } => {
                    // The scrollback frame threads off the confirmed visible base.
                    assert!(base >= 1, "a scrollback frame's base is a real visible frame");
                    ring.append(&rows);
                    sb_frames += 1;
                }
                _ => saw_visible = true,
            }
        }
        assert!(saw_visible, "the broadcast still carries the visible frame(s)");
        assert!(sb_frames >= 1, "a scrollback-capable client must receive Scrollback frames");
        assert_eq!(ring.len(), scrolled, "the ring holds every scrolled-off row");
        for i in 0..scrolled {
            assert_eq!(
                ring.row(i).map(<[u8]>::to_vec),
                term.dump_scrollback_row(i),
                "ring row {i} must equal the daemon's dump_scrollback_row(i)"
            );
        }
    }

    /// A frame-capable client that did NOT advertise `CAP_SCROLLBACK` gets its
    /// visible frames but never a Scrollback body — the daemon must not push
    /// scrollback to a client that cannot consume it. Isolates the cap gate from
    /// the frame gate.
    #[test]
    fn frame_client_without_scrollback_cap_gets_no_scrollback_frames() {
        let (rows, cols) = (5u16, 24u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);

        // frame_capable_conn advertises only PROTOCOL_VERSION — no CAP_SCROLLBACK.
        let (mut c, _peer) = frame_capable_conn(rows, cols);
        assert!(c.producer.is_some());
        assert!(!c.wants_scrollback(), "no CAP_SCROLLBACK advertised");

        // Replay keyframe (establish the base), then scroll and broadcast.
        assert!(c.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            term.is_alt_screen(),
            (rows, cols),
        ));
        scroll_off(&mut term, 12);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");

        assert!(term.primary_scrollback_len() > 0, "output really did scroll");
        for body in decode_frame_bodies(&c.write_buf) {
            assert!(
                !matches!(body, FrameBody::Scrollback { .. }),
                "a client without CAP_SCROLLBACK must receive no Scrollback bodies"
            );
        }
    }

    // ---- Task 2.4b: daemon escape-to-shell overlay (FDR 0008) ----

    /// The core Task 2.4b property, exercised at the level the daemon's overlay
    /// logic is testable without a live shell PTY: when the broadcast source
    /// swaps wholesale (session→overlay on `Tag::Shell`, overlay→session on the
    /// overlay shell's EOF), `broadcast_source_swap` forces every frame-capable
    /// client's producer to emit a fresh `Full` keyframe — never a full-screen
    /// `Diff` against the now-irrelevant acked base — and broadcasts the new
    /// source's screen. The keyframe force is the resolution of the plan's Step 4:
    /// `FrameProducer::drop_acked_base` (already used by the remote server's
    /// RESYNC) makes the next `encode_visible` a `Full`. The poll/spawn/EOF
    /// plumbing around it is a straight-line mirror of the tested remote server.
    #[test]
    fn overlay_source_swap_forces_keyframes_and_broadcasts_each_screen() {
        let (rows, cols) = (24u16, 80u16);
        let mut session = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut session);

        let (mut c, _peer) = frame_capable_conn(rows, cols);
        assert!(c.producer.is_some());

        // Establish the acked visible base (attach replay): a Full keyframe.
        assert!(c.queue_frame(
            session.dump_vt(),
            Snapshot::from_term(&session),
            session.is_alt_screen(),
            (rows, cols),
        ));

        // A live session edit broadcasts a Diff against that base — the contrast
        // that proves the later keyframes come from the source swap, not a fresh
        // producer.
        session.process(b"appended session output");
        broadcast_output(std::slice::from_mut(&mut c), &session, b"<raw ignored>");

        // Overlay ENTER: the daemon spawns a shell overlay and swaps the
        // broadcast source to it. Its screen replaces the session view.
        let mut overlay = Terminal::new(rows, cols);
        overlay.process(b"\x1b[2J\x1b[Hoverlay-shell:/session/cwd$ ");
        broadcast_source_swap(
            std::slice::from_mut(&mut c),
            &overlay,
            &overlay.dump_vt_flat(),
        );
        let after_enter = c.write_buf.clone();

        // Overlay EXIT (the shell's Ctrl-D/EOF): swap back to the live session.
        broadcast_source_swap(
            std::slice::from_mut(&mut c),
            &session,
            &session.dump_vt_flat(),
        );

        // Body sequence: the base Full, the live-edit Diff, then a Full on EACH
        // source swap. A plain broadcast at those points would have been a Diff;
        // the two Fulls are the keyframe force.
        let bodies = decode_frame_bodies(&c.write_buf);
        assert_eq!(bodies.len(), 4, "base + edit + enter + exit");
        assert!(matches!(bodies[0], FrameBody::Full(_)), "base keyframe");
        assert!(
            matches!(bodies[1], FrameBody::Diff { base: 1, .. }),
            "an established base diffs, got {:?}",
            bodies[1]
        );
        assert!(
            matches!(bodies[2], FrameBody::Full(_)),
            "overlay ENTER forces a Full keyframe, got {:?}",
            bodies[2]
        );
        assert!(
            matches!(bodies[3], FrameBody::Full(_)),
            "overlay EXIT forces a Full keyframe, got {:?}",
            bodies[3]
        );

        // Reconstructed screens: the overlay screen is what the client shows while
        // the overlay is up, and the live session resumes once it closes.
        assert_eq!(
            reconstruct(&after_enter, rows, cols),
            Snapshot::from_term(&overlay),
            "the overlay screen replaces the session view for the client"
        );
        assert_eq!(
            reconstruct(&c.write_buf, rows, cols),
            Snapshot::from_term(&session),
            "the live session resumes when the overlay closes"
        );
    }

    /// Regression for the Task 2.4b replay-source bug (found in code review):
    /// a client that attaches (or SIGCONT-resumes) WHILE an escape overlay is up
    /// must replay the OVERLAY screen, not the live session underneath. The
    /// daemon's replay derives its first producer frame from `active_source`, so
    /// with an overlay present the attaching client reconstructs the overlay; with
    /// none it reconstructs the session.
    #[test]
    fn replay_mid_overlay_bases_on_the_overlay_screen() {
        let (rows, cols) = (24u16, 80u16);
        let mut session = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut session);
        let mut overlay = Terminal::new(rows, cols);
        overlay.process(b"\x1b[2J\x1b[Hoverlay-shell:/tmp$ ");

        // Source selection: the overlay while up, the session when gone.
        assert_eq!(
            Snapshot::from_term(active_source(Some(&overlay), &session)),
            Snapshot::from_term(&overlay),
            "active_source picks the overlay while one is up"
        );
        assert_eq!(
            Snapshot::from_term(active_source(None, &session)),
            Snapshot::from_term(&session),
            "active_source falls back to the session with no overlay"
        );

        // A frame-capable client attaching mid-overlay replays the overlay screen
        // (the bug: it used to replay `session` and render it until the next
        // overlay output).
        let (mut c, _peer) = frame_capable_conn(rows, cols);
        let src = active_source(Some(&overlay), &session);
        assert!(c.queue_frame(
            src.dump_vt(),
            Snapshot::from_term(src),
            src.is_alt_screen(),
            (rows, cols),
        ));
        assert_eq!(
            reconstruct(&c.write_buf, rows, cols),
            Snapshot::from_term(&overlay),
            "a mid-overlay attach reconstructs the overlay screen, not the session"
        );
    }

    // ---- Task 3.0: daemon lossy-client mode + Tag::FrameAck (RFC 0008 §3) ----

    /// A LOSSY relay client: its `Tag::Init` advertises `CAP_LOSSY` plus any
    /// `extra` content caps (MORPH/BASE_SUM/SCROLLBACK). With the gate on it gets a
    /// `FrameProducer` like any frame-capable client, but `lossy` is set so it is
    /// NOT self-acked — its base advances only on `apply_frame_ack`.
    fn lossy_conn(rows: u16, cols: u16, extra: &[caps::Cap]) -> (ClientConn, UnixStream) {
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut c = ClientConn {
            stream,
            read_buf: FrameBuffer::new(),
            write_buf: Vec::new(),
            rows: 0,
            cols: 0,
            caps: Vec::new(),
            producer: None,
            lossy: false,
            coalesce: false,
            coalesce_off: false,
            pending_frame_start: None,
            sb_floor: 0,
            acked_sb_total: 0,
            bytes_drained: 0,
            last_drain_ms: 0,
            hiwater_mb: 0,
            echo_flag: 0,
            overlay_flag: 0,
            record: introspect::ClientRecord::default(),
            record_at: 0,
            attach_pid: None,
            last_input_ms: 0,
            wants_activity: false,
            activity_now: None,
            activity_sent: None,
            kind: SessionKind::Unknown,
            kind_sent: false,
            wants_push_cmd: false,
            push_offered: false,
            visible_shaped_for: None,
            regeometry_keyframe: None,
            pacing: None,
            init_applied: false,
        };
        let mut table = vec![caps::Cap {
            id: caps::CAP_LOSSY,
            payload: vec![],
        }];
        table.extend_from_slice(extra);
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&table)));
        c.apply_init(&init);
        c.maybe_enable_frames();
        (c, peer)
    }

    /// RFC 0013 §5.2 (#193): the activity label rides a visible frame only for
    /// a client that requested id 15, only on the first frame after the
    /// request, and again only when the label changes.
    #[test]
    fn activity_label_rides_visible_frames_on_request_and_change_only() {
        let (mut c, _peer) = frame_capable_conn(24, 80);
        let mut term = Terminal::with_scrollback(24, 80, 0);
        term.process(b"hello");
        let label = |process: &str, title: &str| caps::SessionActivity {
            process: process.into(),
            title: title.into(),
        };
        // Not requested: the label is known daemon-side but never attached.
        c.activity_now = Some(label("fish", ""));
        assert!(c.request_frame_from(&term));
        let frames = decode_server_frames(&c.write_buf);
        assert!(caps::find(&frames[0].caps, caps::CAP_SESSION_ACTIVITY).is_none());
        c.write_buf.clear();

        // Requested (a forwarded ClientCaps table, as a relay/bridge sends):
        // the next visible frame carries it, the one after (unchanged) not.
        c.absorb_client_caps(
            &[caps::Cap {
                id: caps::CAP_SESSION_ACTIVITY,
                payload: vec![],
            }],
            0,
            false,
        );
        assert!(c.wants_activity);
        term.process(b" one");
        assert!(c.request_frame_from(&term));
        term.process(b" two");
        assert!(c.request_frame_from(&term));
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(frames.len(), 2);
        let got = caps::find(&frames[0].caps, caps::CAP_SESSION_ACTIVITY).expect("first frame carries it");
        assert_eq!(caps::decode_session_activity(&got.payload).unwrap(), label("fish", ""));
        assert!(caps::find(&frames[1].caps, caps::CAP_SESSION_ACTIVITY).is_none(), "unchanged: not repeated");
        c.write_buf.clear();

        // The label changes (the app set a title): attached once more.
        c.activity_now = Some(label("vim", "~/notes"));
        term.process(b" three");
        assert!(c.request_frame_from(&term));
        let frames = decode_server_frames(&c.write_buf);
        let got = caps::find(&frames[0].caps, caps::CAP_SESSION_ACTIVITY).expect("changed: attached");
        assert_eq!(
            caps::decode_session_activity(&got.payload).unwrap().label(),
            "~/notes \u{b7} vim"
        );
    }

    /// `CAP_SESSION_KIND` (id 20): a client that requested id 15 gets the
    /// kind on the SAME frame as its first activity entry, and never again
    /// (the kind never changes); a client that did not request activity
    /// never gets it.
    #[test]
    fn kind_rides_the_first_activity_bearing_frame_once() {
        let label = |process: &str| caps::SessionActivity {
            process: process.into(),
            title: String::new(),
        };
        let mut term = Terminal::with_scrollback(24, 80, 0);
        term.process(b"hello");

        // No activity request: the kind is known but never attached.
        let (mut quiet, _peer) = frame_capable_conn(24, 80);
        quiet.kind = SessionKind::Anonymous;
        quiet.activity_now = Some(label("fish"));
        assert!(quiet.request_frame_from(&term));
        let frames = decode_server_frames(&quiet.write_buf);
        assert!(caps::find(&frames[0].caps, caps::CAP_SESSION_KIND).is_none());
        assert!(!quiet.kind_sent);

        // Requested: the first activity-bearing frame carries both entries.
        let (mut c, _peer) = frame_capable_conn(24, 80);
        c.kind = SessionKind::Anonymous;
        c.absorb_client_caps(
            &[caps::Cap {
                id: caps::CAP_SESSION_ACTIVITY,
                payload: vec![],
            }],
            0,
            false,
        );
        c.activity_now = Some(label("fish"));
        assert!(c.request_frame_from(&term));
        let frames = decode_server_frames(&c.write_buf);
        assert!(caps::find(&frames[0].caps, caps::CAP_SESSION_ACTIVITY).is_some());
        let got = caps::find(&frames[0].caps, caps::CAP_SESSION_KIND).expect("kind rides the first activity frame");
        assert_eq!(caps::decode_session_kind(&got.payload), Some(SessionKind::Anonymous));
        c.write_buf.clear();

        // A later activity change: the label again, the kind not.
        c.activity_now = Some(label("vim"));
        term.process(b" more");
        assert!(c.request_frame_from(&term));
        let frames = decode_server_frames(&c.write_buf);
        assert!(caps::find(&frames[0].caps, caps::CAP_SESSION_ACTIVITY).is_some());
        assert!(caps::find(&frames[0].caps, caps::CAP_SESSION_KIND).is_none(), "sent once");
    }

    /// RFC 0016 §2: the push-cmd offer rides the first activity-bearing frame
    /// once, and only to a connection that asked with `CAP_PUSH_CMD`.
    #[test]
    fn push_cmd_is_offered_once_beside_the_first_activity_answer_to_a_client_that_asks() {
        let label = |process: &str| caps::SessionActivity {
            process: process.into(),
            title: String::new(),
        };
        let wants = |c: &mut ClientConn, ids: &[u8]| {
            let table: Vec<caps::Cap> = ids.iter().map(|&id| caps::Cap { id, payload: vec![] }).collect();
            c.absorb_client_caps(&table, 0, false);
        };
        let mut term = Terminal::with_scrollback(24, 80, 0);
        term.process(b"hello");

        // Asked for activity only: never offered.
        let (mut quiet, _peer) = frame_capable_conn(24, 80);
        wants(&mut quiet, &[caps::CAP_SESSION_ACTIVITY]);
        quiet.activity_now = Some(label("fish"));
        assert!(quiet.request_frame_from(&term));
        let frames = decode_server_frames(&quiet.write_buf);
        assert!(caps::find(&frames[0].caps, caps::CAP_SESSION_ACTIVITY).is_some());
        assert!(caps::find(&frames[0].caps, caps::CAP_PUSH_CMD).is_none());

        // Asked for both: offered (empty payload) beside the first answer.
        let (mut c, _peer) = frame_capable_conn(24, 80);
        wants(&mut c, &[caps::CAP_SESSION_ACTIVITY, caps::CAP_PUSH_CMD]);
        c.activity_now = Some(label("fish"));
        assert!(c.request_frame_from(&term));
        let frames = decode_server_frames(&c.write_buf);
        let offer = caps::find(&frames[0].caps, caps::CAP_PUSH_CMD).expect("offered on the first activity frame");
        assert!(offer.payload.is_empty());
        c.write_buf.clear();

        // A later activity change: not offered again.
        c.activity_now = Some(label("vim"));
        term.process(b" more");
        assert!(c.request_frame_from(&term));
        let frames = decode_server_frames(&c.write_buf);
        assert!(caps::find(&frames[0].caps, caps::CAP_SESSION_ACTIVITY).is_some());
        assert!(caps::find(&frames[0].caps, caps::CAP_PUSH_CMD).is_none(), "offered once");
    }

    /// RFC 0016 §4.1: each token is served once; a seed makes it served from
    /// the start; the set keeps at least the 64 most recent.
    #[test]
    fn a_push_request_is_served_once_per_token() {
        let req = |t: u64| vec![caps::encode_push_cmd_request(t)];
        let mut served = Vec::new();
        assert_eq!(take_push_request(&req(7), &mut served), Some(7));
        assert_eq!(take_push_request(&req(7), &mut served), None, "a repeat");
        assert_eq!(take_push_request(&req(8), &mut served), Some(8));
        assert_eq!(take_push_request(&[], &mut served), None, "no request");
        let malformed = caps::Cap { id: caps::CAP_PUSH_CMD_REQUEST, payload: vec![0; 8] };
        assert_eq!(take_push_request(&[malformed], &mut served), None, "token 0");

        let mut seeded = vec![7];
        assert_eq!(take_push_request(&req(7), &mut seeded), None, "the seed is served");

        let mut many = Vec::new();
        for t in 1..=200u64 {
            assert_eq!(take_push_request(&req(t), &mut many), Some(t));
        }
        for t in 137..=200u64 {
            assert_eq!(take_push_request(&req(t), &mut many), None, "token {t} forgotten");
        }
    }

    /// Decode the queued `Tag::Frame` records into whole `ServerFrame`s (header +
    /// body), asserting every record is a `Tag::Frame`. Unlike `decode_frame_bodies`
    /// this keeps `frame_num`, so the ack-lag test can check the number climbing
    /// while the diff base stays frozen.
    fn decode_server_frames(write_buf: &[u8]) -> Vec<ServerFrame> {
        let mut fb = FrameBuffer::new();
        fb.feed(write_buf);
        let mut out = Vec::new();
        while let Some(frame) = fb.next().unwrap() {
            assert_eq!(frame.tag, Tag::Frame, "a frame client must receive Tag::Frame");
            out.push(ServerFrame::decode(&frame.payload).unwrap());
        }
        out
    }

    #[test]
    fn init_with_cap_lossy_marks_client_lossy() {
        let mut c = test_client_conn();
        let mut init = ipc::encode_resize(24, 80).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[caps::Cap {
            id: caps::CAP_LOSSY,
            payload: vec![],
        }])));
        c.apply_init(&init);
        assert!(c.lossy, "CAP_LOSSY on Init marks the client lossy");

        // A bare re-Init preserves it (skips the cap block), like `self.caps`.
        c.apply_init(&ipc::encode_resize(30, 100));
        assert!(c.lossy, "a bare re-Init preserves the lossy marker");

        // A reliable Init (no CAP_LOSSY) leaves it false.
        let mut r = test_client_conn();
        let mut rinit = ipc::encode_resize(24, 80).to_vec();
        rinit.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        r.apply_init(&rinit);
        assert!(!r.lossy, "no CAP_LOSSY ⇒ reliable");
    }

    /// (a) A lossy client is NOT self-acked: withholding `Tag::FrameAck` freezes
    /// the diff base while `frame_num` keeps climbing (ack-lag), exactly like the
    /// UDP server. Once the relay forwards an ack the base advances there.
    #[test]
    fn lossy_client_frames_are_not_self_acked_and_base_lags() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);

        // DumpDiff (no CAP_MORPH) so bodies stay decodable and `base` is readable.
        let (mut c, _peer) = lossy_conn(rows, cols, &[]);
        assert!(c.lossy && c.producer.is_some());

        // Frame 1: the attach Full (against the empty frame-0 base). NOT self-acked.
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        assert_eq!(c.producer.as_ref().unwrap().current_num(), 1);
        assert_eq!(
            c.producer.as_ref().unwrap().acked_num(),
            0,
            "a lossy client must NOT self-ack: the base stays at frame 0"
        );

        // The relay forwards an ack for frame 1 ⇒ base advances to 1.
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        assert_eq!(c.producer.as_ref().unwrap().acked_num(), 1);

        // Several visible edits with NO further FrameAck: each frame's number
        // climbs but every body anchors at the FROZEN base 1 (each new frame
        // supersedes the last unacked one — the O(1) relay-buffer property).
        for i in 0..3 {
            term.process(format!("edit {i} ").as_bytes());
            broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");
        }
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(frames.len(), 4, "one attach Full + three lagged edits");
        assert_eq!(frames[0].frame_num, 1);
        assert!(matches!(frames[0].body, FrameBody::Full(_)), "attach ⇒ Full");
        for (offset, f) in frames[1..].iter().enumerate() {
            assert_eq!(f.frame_num, 2 + offset as u64, "frame_num climbs with each edit");
            match &f.body {
                FrameBody::Diff { base, .. } => {
                    assert_eq!(*base, 1, "ack-lag freezes the diff base at the last acked frame")
                }
                other => panic!("expected a Diff anchored at base 1, got {other:?}"),
            }
        }
        assert_eq!(
            c.producer.as_ref().unwrap().acked_num(),
            1,
            "the base is still 1 — no further FrameAck arrived"
        );
    }

    /// (b) A `Tag::FrameAck{acked}` advances the diff base so the next frame
    /// anchors there — the base tracks the acks the relay forwards.
    #[test]
    fn frame_ack_advances_the_diff_base() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = lossy_conn(rows, cols, &[]);

        // Frame 1 (Full), acked ⇒ base 1.
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));

        // Frame 2 diffs against base 1; ack it ⇒ base 2.
        term.process(b"first edit ");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");
        c.apply_frame_ack(&ipc::encode_frame_ack(2, 0));
        assert_eq!(c.producer.as_ref().unwrap().acked_num(), 2);

        // Frame 3 now anchors at the freshly acked base 2.
        term.process(b"second edit ");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");

        let frames = decode_server_frames(&c.write_buf);
        assert!(matches!(frames[1].body, FrameBody::Diff { base: 1, .. }), "got {:?}", frames[1].body);
        assert!(matches!(frames[2].body, FrameBody::Diff { base: 2, .. }), "got {:?}", frames[2].body);
    }

    /// posh#181: a LOSSY client (the relay / M2 bridge) that wants scrollback
    /// must be able to APPLY the scrollback frames the daemon queues. The
    /// client's RFC 0002 §3 rule is `body.base == applied_num`; the daemon
    /// queues the scrollback frame right AFTER the visible frame of the same
    /// broadcast, so its base must be that visible frame — the one the client
    /// has just applied — not the older acked base the visible frame diffed
    /// against. Simulates the client's apply rule over the queued stream.
    #[test]
    fn lossy_scrollback_frame_threads_off_the_visible_frame_it_follows() {
        let (rows, cols) = (5u16, 24u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        let (mut c, _peer) = lossy_conn(
            rows,
            cols,
            &[caps::Cap {
                id: caps::CAP_SCROLLBACK,
                payload: vec![0],
            }],
        );
        assert!(c.lossy && c.wants_scrollback());

        // Attach Full (frame 1), acked by the client via the bridge.
        assert!(c.request_frame_from(&term));
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        c.write_buf.clear();

        // Output scrolls rows off: one broadcast = visible frame + scrollback frame.
        scroll_off(&mut term, 12);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");

        // The client's apply rule (remote/client.rs process_frame, v1 bodies).
        let mut applied_num = 1u64;
        let mut ring_rows = 0usize;
        for f in decode_server_frames(&c.write_buf) {
            match &f.body {
                FrameBody::Empty => {}
                FrameBody::Scrollback { base, rows } => {
                    if *base == applied_num {
                        ring_rows += rows.len();
                        applied_num = f.frame_num;
                    }
                }
                FrameBody::Full(_) => applied_num = f.frame_num,
                FrameBody::Diff { base, .. } | FrameBody::Morph { base, .. } => {
                    if *base == applied_num {
                        applied_num = f.frame_num;
                    }
                }
                other => panic!("unexpected body {other:?}"),
            }
        }
        assert!(
            ring_rows > 0,
            "the lossy client dropped every scrollback frame: its base never matched \
             the visible frame the client had just applied (posh#181)"
        );
        assert_eq!(
            ring_rows,
            term.primary_scrollback_len(),
            "every scrolled-off row landed in the client ring"
        );

        // The client's cumulative ack lands on the scrollback frame: the daemon
        // folds its coverage into acked_sb_total, so the next growth ships ONLY
        // the new rows (no double-append), threaded off the next visible frame.
        c.apply_frame_ack(&ipc::encode_frame_ack(applied_num, 0));
        c.write_buf.clear();
        scroll_off(&mut term, 3);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");
        let frames = decode_server_frames(&c.write_buf);
        let visible_num = frames[0].frame_num;
        assert!(
            matches!(frames[0].body, FrameBody::Diff { base, .. } if base == applied_num),
            "the visible frame diffs against the acked scrollback slot, got {:?}",
            frames[0].body
        );
        match &frames[1].body {
            FrameBody::Scrollback { base, rows } => {
                assert_eq!(*base, visible_num, "threads off the visible frame it follows");
                assert_eq!(rows.len(), 3, "only the rows since the acked coverage");
            }
            other => panic!("expected a scrollback frame, got {other:?}"),
        }
    }

    /// (c) A `Tag::FrameAck` with the RESYNC flag drops the acked base, forcing the
    /// next body to a `Full` keyframe (base-sum divergence recovery).
    #[test]
    fn frame_ack_resync_forces_a_full_keyframe() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = lossy_conn(rows, cols, &[]);

        // Frame 1 (Full) acked ⇒ base 1; frame 2 is a Diff against it.
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        term.process(b"an edit ");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");

        // RESYNC (acking frame 2, then dropping the base): the next frame is a Full.
        c.apply_frame_ack(&ipc::encode_frame_ack(2, ipc::FRAME_ACK_RESYNC));
        assert!(!c.producer.as_ref().unwrap().has_acked_base(), "RESYNC drops the base");
        term.process(b"more ");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");

        let bodies = decode_frame_bodies(&c.write_buf);
        assert!(matches!(bodies[0], FrameBody::Full(_)), "attach ⇒ Full");
        assert!(matches!(bodies[1], FrameBody::Diff { base: 1, .. }), "got {:?}", bodies[1]);
        assert!(
            matches!(bodies[2], FrameBody::Full(_)),
            "RESYNC forces the next body to a Full keyframe, got {:?}",
            bodies[2]
        );
    }

    /// (d) The codec is selected from the negotiated caps: `CAP_MORPH` ⇒ MorphDelta
    /// bodies for a lossy client (a reliable socket client is always DumpDiff).
    #[test]
    fn lossy_client_uses_morph_codec_when_negotiated() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = lossy_conn(
            rows,
            cols,
            &[caps::Cap {
                id: caps::CAP_MORPH,
                payload: vec![],
            }],
        );

        // Frame 1 against the empty base is a Full even under Morph; ack it.
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));

        // A small edit now morphs against the acked base. (The first frame's codec
        // is left unasserted: against the blank frame-0 base MorphDelta may emit
        // either a Full keyframe or a from-blank Morph; the negotiated-codec claim
        // is what the post-ack frame proves.)
        term.process(b"appended");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");

        let bodies = decode_frame_bodies(&c.write_buf);
        assert!(
            matches!(bodies[1], FrameBody::Morph { base: 1, .. }),
            "CAP_MORPH ⇒ a Morph against the acked base, got {:?}",
            bodies[1]
        );
    }

    /// (e) With `CAP_BASE_SUM` the daemon stamps the diff base's checksum on the
    /// Diff so the far client can verify its base before applying (RFC 0006). A
    /// reliable client's Diff carries no base_sum — the contrast.
    #[test]
    fn lossy_client_stamps_base_sum_when_negotiated() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = lossy_conn(
            rows,
            cols,
            &[caps::Cap {
                id: caps::CAP_BASE_SUM,
                payload: vec![],
            }],
        );

        // Frame 1 (Full) over the base bytes we capture, then relay-ack it so
        // frame 2 diffs against that confirmed base.
        let base_dump = term.dump_vt();
        assert!(c.queue_frame(base_dump.clone(), Snapshot::from_term(&term), false, (rows, cols)));
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        term.process(b"appended");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");

        let bodies = decode_frame_bodies(&c.write_buf);
        match &bodies[1] {
            FrameBody::Diff { base, base_sum, .. } => {
                assert_eq!(*base, 1);
                assert_eq!(
                    *base_sum,
                    Some(base_checksum(&base_dump)),
                    "the stamp must checksum the acked diff base bytes"
                );
            }
            other => panic!("expected a checksummed Diff, got {other:?}"),
        }
    }

    /// The mux-session wedge (the `sc list`/vim hang): a `FRAME_ACK_RESYNC`
    /// must ship the recovering `Full` IMMEDIATELY, with NO new PTY output.
    /// `apply_frame_ack` alone only drops the base, so on a static screen the
    /// promised Full never ships: the client already rejected the outstanding
    /// diffs (base-behind basemis, #95), the relay/bridge cleared its held
    /// frame on the RESYNC, and both ends sit silent forever. The single-peer
    /// server ships it via `force_frame` (server.rs, "even if the screen is
    /// static"); the daemon's `handle_frame_ack` must answer equivalently.
    #[test]
    fn frame_ack_resync_ships_recovering_full_without_new_output() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = lossy_conn(rows, cols, &[]);

        // Full #1 acked ⇒ base 1; the echo burst races ahead: #2 diffs vs 1.
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        handle_frame_ack(&mut c, &ipc::encode_frame_ack(1, 0), &term, 0);
        term.process(b"echo burst ");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");

        // The client's #95 resync request arrives; the screen is STATIC from
        // here on (the shell is idle at a prompt).
        handle_frame_ack(&mut c, &ipc::encode_frame_ack(2, ipc::FRAME_ACK_RESYNC), &term, 0);

        // The recovering Full must ALREADY be queued — no output will come to
        // trigger one, and nothing else retransmits (the bridge cleared its
        // held frame on the same RESYNC).
        let bodies = decode_frame_bodies(&c.write_buf);
        assert!(
            matches!(bodies.last(), Some(FrameBody::Full(_))),
            "a RESYNC on a static screen must ship the recovering Full at once, got {:?}",
            bodies.last()
        );
        assert_eq!(
            reconstruct(&c.write_buf, rows, cols),
            Snapshot::from_term(&term),
            "the forced keyframe re-establishes the wedged client at the live screen"
        );
    }

    /// A RELIABLE client (no `CAP_LOSSY`) is unchanged: it self-acks with no
    /// `Tag::FrameAck` and emits DumpDiff Diffs with no base_sum — the byte-for-byte
    /// pre-Task-3.0 behavior the lossy branch must not disturb.
    #[test]
    fn reliable_client_self_acks_and_uses_dumpdiff() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);

        let (mut c, _peer) = frame_capable_conn(rows, cols);
        assert!(!c.lossy, "no CAP_LOSSY ⇒ reliable");

        // Frame 1: the self-ack advances the base to 1 with NO Tag::FrameAck.
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        assert_eq!(
            c.producer.as_ref().unwrap().acked_num(),
            1,
            "a reliable client self-acks: the base advances without any FrameAck"
        );

        // The next frame is a DumpDiff Diff against the self-acked base, no base_sum.
        term.process(b"appended");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");
        let bodies = decode_frame_bodies(&c.write_buf);
        assert!(
            matches!(bodies[1], FrameBody::Diff { base: 1, base_sum: None, .. }),
            "reliable ⇒ DumpDiff Diff against the self-acked base, no base_sum, got {:?}",
            bodies[1]
        );
        assert_eq!(
            reconstruct(&c.write_buf, rows, cols),
            Snapshot::from_term(&term),
            "the reliable client's frames still reconstruct the daemon screen"
        );
    }

    /// A reliable (non-lossy) client's `Tag::FrameAck` is a no-op: it self-acks in
    /// `queue_frame` and never sends the verb, so `apply_frame_ack` must not touch
    /// its producer — even a stray RESYNC must NOT drop its base. Makes the
    /// reliable-path-unchanged guarantee airtight (code-review hardening).
    #[test]
    fn reliable_client_frame_ack_is_ignored() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = frame_capable_conn(rows, cols);
        assert!(!c.lossy);

        // Self-ack advances the base to 1.
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        assert_eq!(c.producer.as_ref().unwrap().acked_num(), 1);

        // A stray FrameAck (even RESYNC) is ignored for a reliable client: its
        // base is neither advanced past nor dropped.
        c.apply_frame_ack(&ipc::encode_frame_ack(1, ipc::FRAME_ACK_RESYNC));
        assert!(
            c.producer.as_ref().unwrap().has_acked_base(),
            "a reliable client's FrameAck is ignored: its base is not dropped"
        );
        assert_eq!(c.producer.as_ref().unwrap().acked_num(), 1);
    }

    // ---- posh#137: local write-buffer coalescing (CAP_COALESCE) ----

    /// A COALESCING local client: `Tag::Init` advertises `CAP_COALESCE`. Like a
    /// lossy client it is NOT self-acked (its base advances only on
    /// `apply_frame_ack`), but it keeps plain local semantics (DumpDiff, no
    /// base_sum) and its queued visible frames coalesce in `write_buf`.
    fn coalesce_conn(rows: u16, cols: u16) -> (ClientConn, UnixStream) {
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut c = ClientConn {
            stream,
            read_buf: FrameBuffer::new(),
            write_buf: Vec::new(),
            rows: 0,
            cols: 0,
            caps: Vec::new(),
            producer: None,
            lossy: false,
            coalesce: false,
            coalesce_off: false,
            pending_frame_start: None,
            sb_floor: 0,
            acked_sb_total: 0,
            bytes_drained: 0,
            last_drain_ms: 0,
            hiwater_mb: 0,
            echo_flag: 0,
            overlay_flag: 0,
            record: introspect::ClientRecord::default(),
            record_at: 0,
            attach_pid: None,
            last_input_ms: 0,
            wants_activity: false,
            activity_now: None,
            activity_sent: None,
            kind: SessionKind::Unknown,
            kind_sent: false,
            wants_push_cmd: false,
            push_offered: false,
            visible_shaped_for: None,
            regeometry_keyframe: None,
            pacing: None,
            init_applied: false,
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[caps::Cap {
            id: caps::CAP_COALESCE,
            payload: vec![],
        }])));
        c.apply_init(&init);
        c.maybe_enable_frames();
        (c, peer)
    }

    /// A CAP_COALESCE client is NOT self-acked: two queued frames without a
    /// `Tag::FrameAck` leave the diff base lagging at the first frame, exactly like
    /// the lossy client (the withhold condition now covers coalescing too).
    #[test]
    fn coalesce_client_is_not_self_acked_and_base_lags() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = coalesce_conn(rows, cols);
        assert!(c.coalesce && !c.lossy && c.producer.is_some());

        // Frame 1 (the attach Full) is NOT self-acked: the base stays at 0.
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        assert_eq!(c.producer.as_ref().unwrap().current_num(), 1);
        assert_eq!(
            c.producer.as_ref().unwrap().acked_num(),
            0,
            "a coalescing client must NOT self-ack: the base stays at frame 0"
        );

        // A relay-style ack advances the base to 1, mirroring the lossy path.
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        assert_eq!(c.producer.as_ref().unwrap().acked_num(), 1);
    }

    /// The coalesce step replaces a still-un-sent trailing visible frame in
    /// `write_buf` rather than appending a second: two frames queued with no drain
    /// leave exactly ONE visible frame in the buffer, and it reconstructs the
    /// LATEST screen (frame B) — the bound that keeps a burst under
    /// MAX_CLIENT_BACKLOG.
    #[test]
    fn coalesce_replaces_unsent_trailing_frame_in_write_buf() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = coalesce_conn(rows, cols);

        // Frame A (attach Full), then ack it so B diffs against the confirmed base.
        // The ack advances the diff base but does NOT drain write_buf, so A's bytes
        // are still un-sent at the tail with the anchor at offset 0.
        let base_dump = term.dump_vt();
        assert!(c.queue_frame(base_dump.clone(), Snapshot::from_term(&term), false, (rows, cols)));
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        assert_eq!(c.pending_frame_start, Some(0), "A is the un-sent trailing frame");
        assert_eq!(decode_server_frames(&c.write_buf).len(), 1, "just A so far");

        // Frame B (an edit) queued with NO drain in between: it truncates A's still
        // un-sent slot and takes its place, so the buffer holds exactly ONE visible
        // frame — the LATEST — instead of growing by a second one.
        term.process(b"coalesced edit ");
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));

        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(
            frames.len(),
            1,
            "the un-sent trailing frame was replaced, not appended: {} records",
            frames.len()
        );
        // The single surviving frame reconstructs frame B's (latest) screen. It
        // diffs against the acked base A (base 1), which the daemon coalesced OUT
        // of the buffer because the client already holds it — so seed the applier
        // with A's dump (what the real client has) before applying B.
        assert_eq!(
            reconstruct_coalesced(&c.write_buf, rows, cols, &base_dump),
            Snapshot::from_term(&term),
            "the coalesced buffer reconstructs the LATEST screen"
        );
    }

    /// The coalesce step never truncates a partially-sent frame: with the anchor
    /// cleared (as the drain loop does once bytes go on the wire), a new frame
    /// APPENDS rather than replacing — the buffer grows, preserving the in-flight
    /// frame's bytes.
    #[test]
    fn coalesce_does_not_truncate_a_partially_sent_frame() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = coalesce_conn(rows, cols);

        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        // Simulate a drain that crossed the pending frame: the anchor is cleared.
        c.pending_frame_start = None;
        let before = c.write_buf.len();

        term.process(b"next edit ");
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        assert!(
            c.write_buf.len() > before,
            "with no clean anchor the new frame appends (grows the buffer), not truncates"
        );
    }

    /// `FRAME_ACK_COALESCE_OFF` reverts a coalescing client to today's behavior:
    /// `coalesce_off` flips true, a subsequent `queue_frame` self-acks (base
    /// advances with no FrameAck) and appends without truncation.
    #[test]
    fn frame_ack_coalesce_off_reverts_to_self_ack() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = coalesce_conn(rows, cols);

        // Toggle coalescing OFF (frame 0, pure toggle — no base advance).
        c.apply_frame_ack(&ipc::encode_frame_ack(0, ipc::FRAME_ACK_COALESCE_OFF));
        assert!(c.coalesce_off, "the toggle bit sets coalesce_off");
        assert!(!c.coalescing(), "coalesce_off ⇒ not coalescing");
        assert_eq!(c.pending_frame_start, None, "turning off clears the anchor");

        // Now queue_frame self-acks (base advances to 1 with no FrameAck).
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        assert_eq!(
            c.producer.as_ref().unwrap().acked_num(),
            1,
            "toggled-off ⇒ self-ack, the base advances like a reliable client"
        );
        let after_one = c.write_buf.len();

        // A second frame APPENDS (no coalescing) — the buffer grows.
        term.process(b"more ");
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        assert!(
            c.write_buf.len() > after_one,
            "toggled-off ⇒ append, not truncate"
        );

        // Toggling back ON clears coalesce_off.
        c.apply_frame_ack(&ipc::encode_frame_ack(0, 0));
        assert!(!c.coalesce_off, "clearing the bit re-enables coalescing");
        assert!(c.coalescing());
    }

    /// Regression guard: a client that did NOT advertise `CAP_COALESCE` still
    /// self-acks and appends exactly as before — neither the withhold condition nor
    /// the coalesce step touches it.
    #[test]
    fn reliable_client_unaffected() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut term);
        let (mut c, _peer) = frame_capable_conn(rows, cols);
        assert!(!c.coalesce && !c.lossy, "no CAP_COALESCE / CAP_LOSSY ⇒ reliable");
        assert!(!c.coalescing());

        // Self-ack advances the base to 1 with NO FrameAck, and the anchor is never
        // set (coalescing is off for this client).
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        assert_eq!(c.producer.as_ref().unwrap().acked_num(), 1);
        assert_eq!(c.pending_frame_start, None, "no anchor for a non-coalescing client");
        let after_one = c.write_buf.len();

        // A second frame APPENDS (no truncation) — today's byte-for-byte behavior.
        term.process(b"appended");
        assert!(c.queue_frame(term.dump_vt(), Snapshot::from_term(&term), false, (rows, cols)));
        assert!(c.write_buf.len() > after_one, "a reliable client appends every frame");
        assert_eq!(decode_server_frames(&c.write_buf).len(), 2, "both frames present");
    }

    /// A COALESCING client that ALSO advertises `CAP_SCROLLBACK` (as the real
    /// local client does): its scrollback frames must NOT be self-acked either.
    /// `maybe_queue_scrollback` self-acks only a reliable client (`!withhold`); a
    /// coalescing client's base advances solely on its own `Tag::FrameAck`.
    fn coalesce_scrollback_conn(rows: u16, cols: u16) -> (ClientConn, UnixStream) {
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut c = ClientConn {
            stream,
            read_buf: FrameBuffer::new(),
            write_buf: Vec::new(),
            rows: 0,
            cols: 0,
            caps: Vec::new(),
            producer: None,
            lossy: false,
            coalesce: false,
            coalesce_off: false,
            pending_frame_start: None,
            sb_floor: 0,
            acked_sb_total: 0,
            bytes_drained: 0,
            last_drain_ms: 0,
            hiwater_mb: 0,
            echo_flag: 0,
            overlay_flag: 0,
            record: introspect::ClientRecord::default(),
            record_at: 0,
            attach_pid: None,
            last_input_ms: 0,
            wants_activity: false,
            activity_now: None,
            activity_sent: None,
            kind: SessionKind::Unknown,
            kind_sent: false,
            wants_push_cmd: false,
            push_offered: false,
            visible_shaped_for: None,
            regeometry_keyframe: None,
            pacing: None,
            init_applied: false,
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[
            caps::Cap { id: caps::CAP_COALESCE, payload: vec![] },
            caps::Cap { id: caps::CAP_SCROLLBACK, payload: vec![0] },
        ])));
        c.apply_init(&init);
        c.maybe_enable_frames();
        (c, peer)
    }

    /// Regression guard (the review finding): `maybe_queue_scrollback` must
    /// withhold the self-ack for a COALESCING client, not just a lossy one. If it
    /// self-acked (the pre-fix `!self.lossy`), the producer base would advance
    /// server-side without the client's `Tag::FrameAck`, defeating CAP_COALESCE.
    #[test]
    fn coalesce_scrollback_frame_is_not_self_acked() {
        let (rows, cols) = (5u16, 24u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        let (mut c, _peer) = coalesce_scrollback_conn(rows, cols);
        assert!(c.coalesce && !c.lossy && c.wants_scrollback());

        // Attach Full (frame 1), acked by the client so a scrollback frame has a
        // confirmed base to thread off (maybe_queue_scrollback gates on has_base).
        assert!(c.queue_frame(
            term.dump_vt(),
            Snapshot::from_term(&term),
            term.is_alt_screen(),
            (rows, cols),
        ));
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        assert_eq!(c.producer.as_ref().unwrap().acked_num(), 1);

        // Scroll rows off, then broadcast: this queues a visible frame (2) plus a
        // scrollback frame. Neither is self-acked for a coalescing client, so the
        // acked base stays at the client-confirmed frame 1.
        scroll_off(&mut term, 12);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"<raw ignored>");

        assert!(
            decode_frame_bodies(&c.write_buf)
                .iter()
                .any(|b| matches!(b, FrameBody::Scrollback { .. })),
            "a scrollback frame must have been queued for this test to be meaningful"
        );
        assert_eq!(
            c.producer.as_ref().unwrap().acked_num(),
            1,
            "a coalescing client's scrollback frame must NOT self-ack: base stays at \
             the client-acked frame 1"
        );
    }

    // ---- posh#225: each viewport's frame is built for its own geometry ----

    /// A 24x80 session whose 2,000-row ring is FULL of distinct, unwrapped
    /// rows: `dump_vt` replays all of them (~150 KB), so any visible frame that
    /// carries the ring is unmistakably over the 64 KiB these tests allow.
    fn full_ring_term() -> Terminal {
        const RING: usize = 2000;
        let mut term = Terminal::with_scrollback(24, 80, RING);
        for i in 0..RING + 200 {
            term.process(format!("{i:06} ring row, distinct and unwrapped, padded to ~70 bytes ......\r\n").as_bytes());
        }
        term.process(b"$ ls");
        assert!(term.dump_vt().len() > 128 * 1024, "the fixture's ring must dwarf the screen");
        term
    }

    /// The one visible frame `broadcast_output` queued for a fresh frame
    /// client — a `Full` (a fresh producer has only the empty frame-0 base) —
    /// as its dump bytes.
    fn only_full_body(write_buf: &[u8]) -> Vec<u8> {
        let mut bodies = decode_frame_bodies(write_buf);
        assert_eq!(bodies.len(), 1, "one broadcast, one visible frame");
        match bodies.remove(0) {
            FrameBody::Full(dump) => dump,
            other => panic!("a fresh producer's first frame must be a Full, got {other:?}"),
        }
    }

    /// `broadcast_output` of `term` to the lone client `c` (the raw bytes are
    /// ignored for a frame client).
    fn broadcast_to(c: &mut ClientConn, term: &Terminal) {
        broadcast_output(std::slice::from_mut(c), term, b"<raw bytes ignored>");
    }

    #[test]
    fn a_same_size_client_gets_screen_sized_frames_from_a_full_ring() {
        let term = full_ring_term();
        let (mut c, _peer) = frame_capable_conn(term.rows(), term.cols());
        broadcast_to(&mut c, &term);
        let dump = only_full_body(&c.write_buf);
        assert!(dump.len() < 64 * 1024, "a same-size frame carried {} bytes of ring", dump.len());
    }

    #[test]
    fn a_taller_client_gets_a_bounded_tail_not_the_ring() {
        let term = full_ring_term();
        let (mut same, _ps) = frame_capable_conn(term.rows(), term.cols());
        let (mut taller, _pt) = frame_capable_conn(term.rows() + 16, term.cols());
        broadcast_to(&mut same, &term);
        broadcast_to(&mut taller, &term);
        let same = only_full_body(&same.write_buf);
        let taller = only_full_body(&taller.write_buf);
        assert!(taller.len() < 64 * 1024, "a taller frame carried {} bytes of ring", taller.len());
        // The taller mirror shows rows above the grid, so its frame carries a
        // scrollback tail the same-size one does not.
        assert!(
            taller.len() > same.len(),
            "a taller frame ({} bytes) must carry a tail a same-size one ({} bytes) does not",
            taller.len(),
            same.len(),
        );
    }

    /// KNOWN LIMITATION (posh#225 Stage 1), pinned so it is retired on purpose:
    /// `dump_vt_mirror` falls back to the full replay wherever a row count
    /// cannot reproduce it, so a WIDER viewport — the larger of two
    /// differently-sized viewports on one session — keeps ring-sized frames.
    /// Paced send-time delivery (Stage 2) bounds its backlog regardless of
    /// frame size, and session geometry on the frame (RFC 0012) would make
    /// every mirror session-sized; either change is where this test moves.
    #[test]
    fn a_wider_client_still_gets_the_full_dump() {
        let term = full_ring_term();
        let (mut c, _peer) = frame_capable_conn(term.rows(), term.cols() + 20);
        broadcast_to(&mut c, &term);
        assert_eq!(only_full_body(&c.write_buf), term.dump_vt());
    }

    #[test]
    fn clients_of_different_sizes_each_get_their_own_dump() {
        let term = full_ring_term();
        let (rows, cols) = (term.rows(), term.cols());
        let (same, _ps) = frame_capable_conn(rows, cols);
        let (taller, _pt) = frame_capable_conn(rows + 16, cols);
        let mut clients = vec![same, taller];
        broadcast_output(&mut clients, &term, b"<raw bytes ignored>");
        let same_dump = only_full_body(&clients[0].write_buf);
        let taller_dump = only_full_body(&clients[1].write_buf);
        assert_ne!(same_dump, taller_dump, "each geometry gets its own dump");
        // Each client's ring-less mirror, at ITS size, shows the session's
        // grid along its bottom rows.
        for c in &clients {
            let mirror = mirror_frames(&c.write_buf, c.rows, c.cols, &[]);
            let offset = c.rows - rows;
            for r in 0..rows {
                assert_eq!(
                    row_text(&mirror, offset + r),
                    row_text(&term, r),
                    "a {}x{} mirror diverged at session row {r}",
                    c.rows,
                    c.cols,
                );
            }
        }
    }

    /// The rendering-unchanged requirement itself: `mirror` (a client's
    /// ring-less mirror with its frames applied) shows exactly what the same
    /// mirror fed the WHOLE-ring `dump_vt` would — every row and the cursor.
    fn assert_renders_as_full_replay(mirror: &Terminal, term: &Terminal) {
        let want = mirror_frames(&[], mirror.rows(), mirror.cols(), &term.dump_vt());
        assert_mirrors(&want, mirror);
    }

    /// [`assert_renders_as_full_replay`] for `c`'s queued frames, applied to a
    /// ring-less mirror of `c`'s own reported size.
    fn assert_client_renders_as_full_replay(c: &ClientConn, term: &Terminal) {
        assert_renders_as_full_replay(&mirror_frames(&c.write_buf, c.rows, c.cols, &[]), term);
    }

    #[test]
    fn a_taller_clients_mirror_renders_exactly_what_the_full_dump_would() {
        let term = full_ring_term();
        let (mut c, _peer) = frame_capable_conn(term.rows() + 16, term.cols());
        broadcast_to(&mut c, &term);
        assert_client_renders_as_full_replay(&c, &term);
    }

    #[test]
    fn same_size_clients_share_one_dump_in_a_mixed_broadcast() {
        let term = full_ring_term();
        let (rows, cols) = (term.rows(), term.cols());
        let (a, _pa) = frame_capable_conn(rows, cols);
        let (taller, _pt) = frame_capable_conn(rows + 16, cols);
        let (b, _pb) = frame_capable_conn(rows, cols);
        let mut clients = vec![a, taller, b];
        broadcast_output(&mut clients, &term, b"<raw bytes ignored>");
        assert_eq!(clients[0].write_buf, clients[2].write_buf, "same geometry, same frame bytes");
        assert_ne!(clients[0].write_buf, clients[1].write_buf, "another geometry, another dump");
    }

    #[test]
    fn a_resync_keyframe_is_built_for_a_non_session_size_client() {
        let term = full_ring_term();
        let (rows, cols) = (term.rows() + 16, term.cols());
        let (mut c, _peer) = lossy_conn(rows, cols, &[]);
        broadcast_to(&mut c, &term);
        c.write_buf.clear();
        handle_frame_ack(&mut c, &ipc::encode_frame_ack(1, ipc::FRAME_ACK_RESYNC), &term, 0);
        assert_eq!(only_full_body(&c.write_buf), term.dump_vt_mirror(rows, cols));
        assert_client_renders_as_full_replay(&c, &term);
    }

    #[test]
    fn an_overlay_source_swap_builds_each_keyframe_for_the_clients_geometry() {
        let session = full_ring_term();
        let (rows, cols) = (session.rows() + 16, session.cols());
        let (mut c, _peer) = frame_capable_conn(rows, cols);
        broadcast_to(&mut c, &session);
        let mut overlay = Terminal::new(session.rows(), session.cols());
        overlay.process(b"overlay$ ");
        // Open: the overlay screen, as a Full keyframe for this client's size.
        c.write_buf.clear();
        broadcast_source_swap(std::slice::from_mut(&mut c), &overlay, &overlay.dump_vt_flat());
        assert_eq!(only_full_body(&c.write_buf), overlay.dump_vt_mirror(rows, cols));
        // Close: back to the full-ring session, bounded, rendering unchanged.
        c.write_buf.clear();
        broadcast_source_swap(std::slice::from_mut(&mut c), &session, &session.dump_vt_flat());
        let dump = only_full_body(&c.write_buf);
        assert!(dump.len() < 64 * 1024, "the swap-back keyframe carried {} bytes of ring", dump.len());
        assert_client_renders_as_full_replay(&c, &session);
    }

    /// A client's `Tag::Resize` to `rows` x `cols`, handled the way the daemon
    /// loop handles it: apply the size, settle the regeometry rule
    /// (`prepare_regeometry_frame`), and when a frame is owed queue the replay
    /// from `src` (`request_frame_from`). Returns whether a frame was owed.
    fn resize_like_the_loop(c: &mut ClientConn, src: &Terminal, rows: u16, cols: u16) -> bool {
        assert!(c.apply_resize(&ipc::encode_resize(rows, cols)));
        let owed = c.prepare_regeometry_frame();
        if owed {
            assert!(c.request_frame_from(src));
        }
        owed
    }

    /// A frame client resizing ITSELF while the session size stays put (it was
    /// not the smallest viewport) gets no PTY output to supersede the frames
    /// shaped for its old size, and neither the local nor the roaming client
    /// requests a resync on resize — so the daemon owes it a frame for its new
    /// geometry, which the loop queues through the replay (`request_frame_from`).
    #[test]
    fn a_frame_clients_own_resize_owes_a_frame_for_its_new_geometry() {
        let mut term = full_ring_term();
        let (rows, cols) = (term.rows(), term.cols());
        let (mut c, _peer) = frame_capable_conn(rows, cols);
        broadcast_to(&mut c, &term);
        assert_eq!(c.visible_shaped_for, Some((rows, cols)), "a same-size dump is bounded");

        assert!(c.apply_resize(&ipc::encode_resize(rows + 16, cols)));
        assert!(
            c.owes_regeometry_frame(),
            "same-size -> taller: the old frame was bounded, so a frame is owed"
        );
        // What the client shows without it: the session-sized frame, applied at
        // the new size, puts the grid at the TOP instead of the bottom.
        let stale = mirror_frames(&c.write_buf, rows + 16, cols, &[]);
        assert_ne!(row_text(&stale, rows + 15), row_text(&term, rows - 1));

        // The replay the loop queues, on top of the old-geometry frame; once it
        // is built the client is owed nothing more.
        assert!(c.request_frame_from(&term));
        assert!(!c.owes_regeometry_frame(), "the replay settles the debt");
        assert_client_renders_as_full_replay(&c, &term);
        // And later output still applies cleanly on that base.
        term.process(b"\r\nmore output\r\n$ ");
        broadcast_to(&mut c, &term);
        assert_client_renders_as_full_replay(&c, &term);

        assert!(!resize_like_the_loop(&mut c, &term, rows + 16, cols), "an unchanged size owes nothing");
        // Taller -> same-size: the taller frame was bounded too.
        assert!(resize_like_the_loop(&mut c, &term, rows, cols), "taller -> same-size owes a frame");
        // Same-width -> wider: owed ONCE (that replay is the full dump); from
        // there on the newest frame is a full dump, so wider -> wider owes
        // nothing.
        assert!(resize_like_the_loop(&mut c, &term, rows, cols + 20), "same-width -> wider owes one frame");
        assert_eq!(c.visible_shaped_for, None, "the wider replay was the full dump");
        assert!(!resize_like_the_loop(&mut c, &term, rows, cols + 30), "wider -> wider owes nothing");
        // Wider -> same-width: the in-flight frames are full dumps, which
        // render at the new size, so nothing is owed.
        assert!(!resize_like_the_loop(&mut c, &term, rows, cols), "wider -> same-width owes nothing");

        // A size reported for the first time: no frame was built yet, so
        // nothing is owed — the Init replay covers attach.
        let (mut fresh, _pf) = frame_capable_conn(rows, cols);
        fresh.rows = 0;
        fresh.cols = 0;
        assert!(!resize_like_the_loop(&mut fresh, &term, rows, cols), "a first size owes nothing");

        // A baseline client is never owed one (it has no producer).
        let mut baseline = test_client_conn();
        baseline.apply_init(&ipc::encode_resize(rows, cols));
        assert!(!resize_like_the_loop(&mut baseline, &term, rows + 16, cols));
    }

    /// The reviewer's scenario: boundedness is a fact of the dump that was
    /// BUILT, not of the client's size at some later moment. A client at an
    /// unbounded size resizes to a bounded one (owed nothing — it holds a full
    /// dump), then a resync keyframe is built at that bounded size, then it
    /// resizes again: that keyframe is shaped for its size, so a frame IS owed.
    #[test]
    fn a_resync_keyframe_at_a_bounded_size_makes_the_next_resize_owe_a_frame() {
        let term = full_ring_term();
        let (rows, cols) = (term.rows(), term.cols());
        let (mut c, _peer) = lossy_conn(rows, cols + 20, &[]);
        broadcast_to(&mut c, &term);
        assert!(!resize_like_the_loop(&mut c, &term, rows, cols), "it held a full dump");
        handle_frame_ack(&mut c, &ipc::encode_frame_ack(1, ipc::FRAME_ACK_RESYNC), &term, 0);
        assert_eq!(c.visible_shaped_for, Some((rows, cols)), "the resync keyframe was bounded");
        c.write_buf.clear();
        assert!(resize_like_the_loop(&mut c, &term, rows + 16, cols), "a bounded keyframe went stale");
        assert_client_renders_as_full_replay(&c, &term);
    }

    /// A MorphDelta client's regeometry frame must be a keyframe: its encoder
    /// judges morph-expressibility on the SESSION's dims and alt flag, which
    /// the client's own resize leaves unchanged, so with its acked base kept
    /// it would morph between two identical snapshots and deliver no dump.
    #[test]
    fn a_morph_clients_regeometry_frame_is_a_keyframe() {
        let term = full_ring_term();
        let (rows, cols) = (term.rows(), term.cols());
        let morph = [caps::Cap {
            id: caps::CAP_MORPH,
            payload: vec![],
        }];
        let attach = |c: &mut ClientConn| {
            broadcast_to(c, &term);
            c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
            c.write_buf.clear();
        };

        // Before: keeping the acked base, the frame for the new size is a Morph.
        let (mut kept, _pk) = lossy_conn(rows, cols, &morph);
        attach(&mut kept);
        assert!(kept.apply_resize(&ipc::encode_resize(rows + 16, cols)));
        assert!(kept.request_frame_from(&term));
        let bodies = decode_frame_bodies(&kept.write_buf);
        assert!(matches!(bodies[..], [FrameBody::Morph { .. }]), "kept base => Morph, got {bodies:?}");

        // The regeometry path drops it: a Full of the new geometry's dump.
        let (mut c, _pc) = lossy_conn(rows, cols, &morph);
        attach(&mut c);
        assert!(resize_like_the_loop(&mut c, &term, rows + 16, cols));
        assert_eq!(only_full_body(&c.write_buf), term.dump_vt_mirror(rows + 16, cols));
    }

    /// The regeometry keyframe must be DURABLE for a morph client: a late ack
    /// for a PRE-resize frame still in the producer's outstanding window would
    /// restore that frame as the acked base (the session dims did not change),
    /// and the next frame would morph from a dump the client's new-size mirror
    /// may never have rebuilt — if the keyframe was lost, nothing would re-owe
    /// it. Until the keyframe (or a later frame) is acked, every visible frame
    /// stays a `Full`.
    #[test]
    fn a_late_pre_resize_ack_does_not_restore_a_morph_clients_base() {
        let mut term = full_ring_term();
        let (rows, cols) = (term.rows(), term.cols());
        let morph = [caps::Cap {
            id: caps::CAP_MORPH,
            payload: vec![],
        }];
        let (mut c, _pc) = lossy_conn(rows, cols, &morph);
        let output = |c: &mut ClientConn, term: &mut Terminal, text: &[u8]| -> FrameBody {
            term.process(text);
            c.write_buf.clear();
            broadcast_to(c, term);
            let mut bodies = decode_frame_bodies(&c.write_buf);
            assert_eq!(bodies.len(), 1, "one broadcast, one visible frame");
            bodies.remove(0)
        };
        broadcast_to(&mut c, &term); // frame 1, acked: the morph base
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        assert!(matches!(output(&mut c, &mut term, b"a"), FrameBody::Morph { .. })); // frame 2, in flight

        // The client grows taller; the owed keyframe is frame 3.
        assert!(resize_like_the_loop(&mut c, &term, rows + 16, cols));
        assert_eq!(c.producer.as_ref().unwrap().current_num(), 3);
        assert_eq!(c.regeometry_keyframe, Some(RegeometryKeyframe::Sent(3)));

        // A late ack for pre-resize frame 2: the next frame must still be a Full.
        c.apply_frame_ack(&ipc::encode_frame_ack(2, 0));
        match output(&mut c, &mut term, b"b") {
            FrameBody::Full(dump) => assert_eq!(dump, term.dump_vt_mirror(rows + 16, cols)),
            other => panic!("a pre-resize ack restored a morph base: got {other:?}"),
        }
        // Once the client acks a new-geometry frame, morphing resumes.
        c.apply_frame_ack(&ipc::encode_frame_ack(4, 0));
        assert_eq!(c.regeometry_keyframe, None, "a held new-geometry base ends it");
        assert!(matches!(output(&mut c, &mut term, b"c"), FrameBody::Morph { .. }));
    }

    /// The loop wiring itself, through the PRODUCTION `daemon_loop`: two frame
    /// clients keep the session at their shared smaller size, so when one of
    /// them grows taller nothing changes session-side and no PTY output
    /// follows — the frame it receives can only be the owed regeometry
    /// replay, queued after `apply_client_size`, and it must render the
    /// session's history above a bottom-anchored grid at the new size.
    #[test]
    fn the_daemon_loop_sends_a_resized_client_a_frame_for_its_new_geometry() {
        use std::io::Read as _;
        let cfg = Config {
            socket_dir: temp_base(),
            group: "default".into(),
        };
        // The shell stays silent until B's `go` line, so the whole screen is
        // drawn with both clients attached. (The guard predates the posh#239
        // fix, when output racing an attach reached that client as raw
        // `Tag::Output`; `the_daemon_loop_sends_no_output_before_a_clients_init`
        // now pins that race.)
        let script =
            "read go; i=1; while [ $i -le 60 ]; do echo row$i; i=$((i+1)); done; printf 'tail$ '; exec sleep 30";
        let handle = spawn_test_daemon(
            &cfg,
            "g1",
            Some(vec!["sh".into(), "-c".into(), script.into()]),
            SessionKind::Anonymous,
        );
        let socket = cfg.socket_path("g1").unwrap();
        let attach = || {
            let s = UnixStream::connect(&socket).unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
            let mut init = ipc::encode_resize(24, 80).to_vec();
            init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
            ipc::send(s.as_raw_fd(), Tag::Init, &init).unwrap();
            s
        };
        let (mut a, mut b) = (attach(), attach());
        // B's Input follows B's Init on B's socket, and A's Init was sent
        // before B connected, so by the time the daemon writes `go` to the
        // PTY both Inits are processed and every output chunk is framed.
        ipc::send(b.as_raw_fd(), Tag::Input, b"go\n").unwrap();
        // Read `s` into `buf` until `done(buf)` holds, failing after ~10 s.
        let read_until = |s: &mut UnixStream, buf: &mut Vec<u8>, done: &dyn Fn(&[u8]) -> bool| {
            let mut tmp = [0u8; 65536];
            for _ in 0..50 {
                if done(buf) {
                    return;
                }
                match s.read(&mut tmp) {
                    Ok(0) => panic!("daemon closed the connection"),
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    Err(e) => panic!("read: {e}"),
                }
            }
            assert!(done(buf), "timed out waiting on the daemon");
        };
        let prompt_at = |buf: &[u8], rows: u16, row: u16| {
            row_text(&mirror_frames(buf, rows, 80, &[]), row).starts_with("tail$")
        };
        let (mut abuf, mut bbuf) = (Vec::new(), Vec::new());
        read_until(&mut a, &mut abuf, &|buf| prompt_at(buf, 24, 23));
        read_until(&mut b, &mut bbuf, &|buf| prompt_at(buf, 24, 23));

        // A grows taller; B keeps the session at 24 rows.
        let frames_before = decode_frame_bodies(&abuf).len();
        ipc::send(a.as_raw_fd(), Tag::Resize, &ipc::encode_resize(40, 80)).unwrap();
        read_until(&mut a, &mut abuf, &|buf| decode_frame_bodies(buf).len() > frames_before);
        let mirror = mirror_frames(&abuf, 40, 80, &[]);
        assert!(row_text(&mirror, 39).starts_with("tail$"), "the grid is bottom-anchored at 40 rows");
        assert_eq!(row_text(&mirror, 38).trim_end(), "row60");
        assert_eq!(row_text(&mirror, 15).trim_end(), "row37", "history fills the rows above the grid");
        drop((a, b));
        assert_eq!(handle.shutdown(), caps::SessionEnd::Killed);
    }

    // ---- posh#239: no output before a client's Init ----

    /// A connection straight from `accept` has not said what it is: a frame
    /// viewport, a baseline one, or a control connection that never Inits.
    /// It is sent nothing until its Init is applied; afterwards a baseline
    /// client takes `Tag::Output` and a frame-capable one a frame.
    #[test]
    fn broadcast_output_sends_nothing_to_a_client_before_its_init() {
        let term = full_ring_term();
        for framed in [false, true] {
            let mut c = test_client_conn();
            broadcast_to(&mut c, &term);
            assert!(c.write_buf.is_empty(), "framed={framed}: a pre-Init client was sent output");

            let mut init = ipc::encode_resize(term.rows(), term.cols()).to_vec();
            if framed {
                init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
            }
            c.apply_init(&init);
            c.maybe_enable_frames();
            broadcast_output(std::slice::from_mut(&mut c), &term, b"raw");
            if framed {
                assert_eq!(only_full_body(&c.write_buf), term.dump_vt_mirror(term.rows(), term.cols()));
            } else {
                let mut fb = FrameBuffer::new();
                fb.feed(&c.write_buf);
                let rec = fb.next().unwrap().expect("one record");
                assert_eq!((rec.tag, rec.payload.as_slice()), (Tag::Output, b"raw".as_slice()));
            }
        }
    }

    /// The race itself, through the PRODUCTION `daemon_loop`: the shell
    /// floods from the start, so the iteration that accepts each connection
    /// also reads PTY output — before that connection's first record can be
    /// read. A control connection (a `posh history`-style probe, which never
    /// Inits) gets only its reply, and a frame client that attaches mid-flood
    /// gets only frames, ending on the full final screen.
    #[test]
    fn the_daemon_loop_sends_no_output_before_a_clients_init() {
        use std::io::Read as _;
        let cfg = Config {
            socket_dir: temp_base(),
            group: "default".into(),
        };
        let script = "(while :; do echo flood; done) & f=$!; read go; kill $f; wait; \
             i=1; while [ $i -le 60 ]; do echo row$i; i=$((i+1)); done; printf 'tail$ '; exec sleep 30";
        let handle = spawn_test_daemon(
            &cfg,
            "r1",
            Some(vec!["sh".into(), "-c".into(), script.into()]),
            SessionKind::Anonymous,
        );
        let socket = cfg.socket_path("r1").unwrap();
        // Read records off `s` until one satisfies `done`, failing after ~10 s.
        let read_until = |s: &mut UnixStream, fb: &mut FrameBuffer, done: &mut dyn FnMut(ipc::Frame) -> bool| {
            let mut tmp = [0u8; 65536];
            for _ in 0..50 {
                while let Some(rec) = fb.next().unwrap() {
                    if done(rec) {
                        return;
                    }
                }
                match s.read(&mut tmp) {
                    Ok(0) => panic!("daemon closed the connection"),
                    Ok(n) => fb.feed(&tmp[..n]),
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    Err(e) => panic!("read: {e}"),
                }
            }
            panic!("timed out waiting on the daemon");
        };
        let connect = || {
            let s = UnixStream::connect(&socket).unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
            s
        };
        // History probes until the flood is visibly running; each probe's
        // first record must be its reply.
        let mut flooding = false;
        for _ in 0..50 {
            let mut probe = connect();
            ipc::send(probe.as_raw_fd(), Tag::History, &ipc::encode_history_format(false)).unwrap();
            read_until(&mut probe, &mut FrameBuffer::new(), &mut |rec| {
                assert_eq!(rec.tag, Tag::History, "a control connection was sent {:?} before its reply", rec.tag);
                flooding = String::from_utf8_lossy(&rec.payload).contains("flood");
                true
            });
            if flooding {
                break;
            }
        }
        assert!(flooding, "the shell never started flooding");

        let mut a = connect();
        let mut init = ipc::encode_resize(24, 80).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        ipc::send(a.as_raw_fd(), Tag::Init, &init).unwrap();
        ipc::send(a.as_raw_fd(), Tag::Input, b"go\n").unwrap();
        // The client's mirror, applied frame by frame (as `mirror_frames` does).
        let mut mirror = Terminal::with_scrollback(24, 80, 0);
        let mut applied: Vec<u8> = Vec::new();
        let mut first = true;
        read_until(&mut a, &mut FrameBuffer::new(), &mut |rec| {
            assert_eq!(rec.tag, Tag::Frame, "a frame client was sent {:?}", rec.tag);
            let body = ServerFrame::decode(&rec.payload).unwrap().body;
            match DumpDiff.apply(24, 80, &applied, &mut mirror, &body) {
                ApplyOutcome::Advanced { dump } => applied = dump,
                ApplyOutcome::AdvancedNoDump | ApplyOutcome::NoChange => {}
                ApplyOutcome::ReackAndWait => panic!("DumpDiff could not apply a frame"),
            }
            // `a` attached mid-flood: its first screen must already show it,
            // or the race window this test exists for was empty.
            if std::mem::take(&mut first) {
                assert!(
                    (0..24).any(|r| row_text(&mirror, r).trim_end() == "flood"),
                    "a's first frame shows no flood rows: the attach did not race output"
                );
            }
            row_text(&mirror, 23).starts_with("tail$")
        });
        assert_eq!(row_text(&mirror, 22).trim_end(), "row60");
        assert_eq!(row_text(&mirror, 0).trim_end(), "row38", "the whole screen, rows up from the bottom");
        drop(a);
        assert_eq!(handle.shutdown(), caps::SessionEnd::Killed);
    }

    /// Why a client whose OLD geometry got the full dump is owed nothing NEW
    /// on its own resize: its frames are `dump_vt`'s full bytes, which still
    /// apply at the new size, and as before posh#225 the next output repaints
    /// it — so a drag-resized wider viewport is never sent a ring-sized frame
    /// per resize event.
    #[test]
    fn a_wider_clients_resize_owes_no_frame() {
        let term = full_ring_term();
        let (rows, cols) = (term.rows(), term.cols());
        let (mut c, _peer) = frame_capable_conn(rows, cols + 20);
        broadcast_to(&mut c, &term);
        assert!(!resize_like_the_loop(&mut c, &term, rows + 6, cols + 40));
        assert_client_renders_as_full_replay(&c, &term);
    }

    #[test]
    fn clients_of_different_fallback_geometries_share_one_full_dump() {
        let term = full_ring_term();
        let (rows, cols) = (term.rows(), term.cols());
        let (wider, _pw) = frame_capable_conn(rows, cols + 20);
        let (narrower, _pn) = frame_capable_conn(rows + 4, cols - 10);
        let mut clients = vec![wider, narrower];
        broadcast_output(&mut clients, &term, b"<raw bytes ignored>");
        assert_eq!(clients[0].write_buf, clients[1].write_buf, "every fallback geometry gets the one full dump");
        assert_eq!(only_full_body(&clients[0].write_buf), term.dump_vt());
    }

    // ---- posh#225: write_buf growth under a newline flood (MEASUREMENT) ----

    /// How the lossy client's `Tag::FrameAck`s arrive during the flood.
    ///
    /// A cadence names the frame the client WOULD ack. Whether it can is the
    /// drain mode's business:
    ///
    /// - under [`FloodDrain::OneWritePerChunk`] acks are DELIVERY-GATED. A
    ///   client can only ack a frame it has received, so the ack applied is
    ///   the newest frame that is both eligible under the cadence and fully
    ///   delivered (every byte of it has left `write_buf` and been read). A
    ///   backlog therefore delays acks, and late acks grow the frames that
    ///   feed the backlog — the loop this mode exists to show.
    /// - under [`FloodDrain::Never`] and [`FloodDrain::Always`] acks are
    ///   deliberately DELIVERY-BLIND: the cadence's frame is acked whether or
    ///   not its bytes ever left the buffer. Those modes isolate frame SIZE
    ///   against an idealised ack stream; a `Never` run with acks describes a
    ///   state production cannot reach (a reader acking what it never read).
    #[derive(Clone, Copy)]
    enum FloodAcks {
        /// No ack after the attach baseline.
        Never,
        /// Every `k` chunks the client acks the NEWEST frame queued so far
        /// (zero-latency but bursty).
        EveryNewest(usize),
        /// After every chunk the client acks the frame that was newest `k`
        /// chunks ago (a constant ack lag of `k` PTY chunks).
        ///
        /// There is a cliff between `k = 4` and `k = 5`.
        /// `FrameProducer::rotate_current` keeps 8 outstanding frames, and
        /// while rows scroll every chunk queues 2 (visible + scrollback), so
        /// the frame from `k` chunks ago sits `2k` frames behind `current`:
        ///
        /// - `k <= 4` is a healthy lagging reader — every ack lands on a
        ///   frame the producer still holds;
        /// - `k >= 5` acks an already-evicted frame and measures the
        ///   LOST-BASE regime instead: `FrameProducer::ack` drops
        ///   `acked_data` to `None` (the next visible frame is a `Full`) and
        ///   returns `None` (so `acked_sb_total` does not advance), and the
        ///   next `maybe_queue_scrollback` fails `has_acked_base` and queues
        ///   nothing. History stops flowing until an ack lands on a held
        ///   frame again.
        ///
        /// "Healthy" here means a LOCAL, zero-round-trip reader, and even
        /// that is better than production does. Under a flood production's
        /// best case is already lag 1–2: `daemon_main` reads an ack in a
        /// later iteration's client pass, and its `POLLOUT` rule (see
        /// [`FloodDrain::OneWritePerChunk`]) makes it one write per two
        /// chunks. A remote viewport, whose acks cross a real round trip,
        /// spends the whole flood in the lost-base regime. So the cadences
        /// the regression test calls healthy are a best case, not what a
        /// remote viewport sees.
        ///
        /// For a PACED run ([`FloodCase::pace`]) `k` is a round trip in clock
        /// steps, not frames: at time `now` the client acks the newest frame
        /// it had received by `now - k * pace` ms, so `Lagged(k)` models an
        /// RTT of `k * pace` ms. (During the flood that is the same frame the
        /// unpaced rule picks — one queueing step per chunk — but a paced
        /// client builds a frame only every [`PACED_FRAME_FLOOR_MS`] or
        /// more, so `k` chunks is far fewer than `k` frames, and the cliff
        /// above sits at an RTT of about five ack waits instead (RTT >
        /// 5 x `PACED_ACK_WAIT_MS` - pace): a pair goes out every wait, the
        /// outstanding window holds 4 pairs, and the acked scrollback slot of
        /// the pair sent at T is still the oldest held after 4 more pairs —
        /// the 5th, at T + 5 waits, evicts it. See
        /// `posh225_flood_backlog_ideal_reader_measurement`. Under
        /// [`FloodDrain::Trickle`] the RTT clock starts at queue time + 1
        /// step, not at delivery, so a late-delivered frame's RTT is
        /// absorbed rather than added.)
        Lagged(usize),
    }

    impl FloodAcks {
        fn label(self) -> String {
            match self {
                FloodAcks::Never => "never".into(),
                FloodAcks::EveryNewest(k) => format!("newest/{k}"),
                FloodAcks::Lagged(k) => format!("lag {k}"),
            }
        }
    }

    /// How the client's `write_buf` drains during the flood — and, with it,
    /// whether acks are delivery-gated (see [`FloodAcks`]).
    #[derive(Clone, Copy)]
    enum FloodDrain {
        /// Nothing is ever drained; the run stops where the daemon would
        /// drop the client (`MAX_CLIENT_BACKLOG`). Acks are delivery-blind.
        Never,
        /// `write_buf` is emptied after every chunk: isolates per-chunk size.
        /// Acks are delivery-blind.
        Always,
        /// The daemon loop's real drain against a real socketpair: ONE
        /// non-blocking `stream.write(&write_buf)` per PTY chunk (`daemon_main`
        /// does exactly one per poll iteration), with an ideal reader that
        /// empties the socket and acks before the next chunk. Acks are
        /// delivery-gated.
        ///
        /// The socket's buffers are pinned to [`FLOOD_SOCKET_BUFFER`] so one
        /// write's capacity does not ride the host default (roughly 200 KiB
        /// on Linux, 8 KiB on macOS for a unix stream socketpair). The pin is
        /// a request, not the capacity — Linux doubles the value it is given
        /// (socket(7)) — so [`FloodRun::socket_write_capacity`] records what
        /// one write into the empty socket actually accepts on this host, and
        /// [`FloodRun::largest_socket_write`] the most a write of the flood
        /// actually moved.
        ///
        /// Two idealisations remain, both in the CLIENT's favour, so a
        /// backlog crossing here is a crossing in production but a clean run
        /// is a best case rather than a proof:
        ///
        /// - `daemon_main` arms `POLLOUT` only when `write_buf` was non-empty
        ///   BEFORE `poll`, so a frame queued into an empty buffer is written
        ///   on the NEXT iteration; the harness writes it in the same one.
        ///   And the delay compounds: that next iteration queues its own
        ///   chunk before writing, the write empties the buffer, and the
        ///   iteration after it arms no `POLLOUT` again — so production's
        ///   steady state under a flood is one write per TWO chunks, and its
        ///   per-write burst is about two chunks' frames, not one.
        /// - The reader's ack is applied before the next chunk. The daemon
        ///   can read it no earlier than the next iteration's client pass,
        ///   which runs AFTER that iteration's PTY chunk was broadcast.
        ///
        /// `FloodRun::peak_undrained == 0` is therefore a statement about
        /// this model — every chunk's frames left in the write that followed
        /// it — not about production, where a frame waits at least one
        /// iteration by construction.
        OneWritePerChunk,
        /// A SLOW reader: as [`FloodDrain::OneWritePerChunk`] (one write per
        /// step, ideal reader, delivery-gated acks), but each write moves at
        /// most `n` bytes. Frames then outlive the step that queued them, so
        /// `write_buf` is non-empty at later send passes — the case the
        /// empty-buffer half of `paced_send_at` exists for.
        Trickle(usize),
    }

    impl FloodDrain {
        fn label(self) -> String {
            match self {
                FloodDrain::Never => "never".into(),
                FloodDrain::Always => "always".into(),
                FloodDrain::OneWritePerChunk => "1write".into(),
                FloodDrain::Trickle(n) => format!("trkl{n}"),
            }
        }

        /// Whether acks are delivery-gated: the drain writes into the real
        /// socket and the reader can ack only what it has received.
        fn is_gated(self) -> bool {
            matches!(self, FloodDrain::OneWritePerChunk | FloodDrain::Trickle(_))
        }
    }

    /// What one flood run queued for the client.
    #[derive(Default)]
    struct FloodRun {
        fed: usize,
        chunks: usize,
        /// The largest backlog seen right after a chunk was queued. Under
        /// `FloodDrain::Always` that is the largest single-chunk burst (one
        /// visible + one scrollback frame).
        peak_write_buf: usize,
        /// The largest backlog LEFT after a chunk's drain step. Zero under
        /// `OneWritePerChunk` means every chunk's frames reached the reader
        /// in the iteration that queued them.
        peak_undrained: usize,
        /// `OneWritePerChunk`: what one write into the EMPTY pinned socket
        /// accepts on this host, probed before the flood.
        socket_write_capacity: usize,
        /// `OneWritePerChunk`: the most one `stream.write` of the flood moved
        /// (the capacity once frames outgrow it, else the largest burst).
        largest_socket_write: usize,
        /// Bytes queued as visible frames (`Full`/`Diff`) vs scrollback frames.
        visible_bytes: u64,
        scrollback_bytes: u64,
        largest_visible: usize,
        largest_scrollback: usize,
        largest_scrollback_rows: usize,
        crossed_backlog_at: Option<usize>,
        /// Visible frames that went out as a `Full` keyframe, not a `Diff`.
        full_frames: usize,
        scrollback_frames: usize,
        /// Scrollback rows shipped (with repeats) vs rows that actually
        /// scrolled off during the flood vs rows the acks confirmed.
        rows_shipped: u64,
        rows_scrolled: u64,
        rows_acked: u64,
        /// Visible frames queued after the attach keyframe: during the flood
        /// plus, for a paced run, its idle tail.
        visible_frames: usize,
        /// Visible frames queued while chunks were still being fed.
        visible_frames_during_flood: usize,
        /// The most visible frames sitting in `write_buf` (whole or partly
        /// written) right after any step that queued frames.
        max_visible_queued: usize,
        /// Paced runs only: whether the client, acking the last visible frame
        /// once the idle tail went quiet, holds the terminal's final screen.
        last_screen_delivered: Option<bool>,
        /// Paced runs only: the client's ack latency at the end of the run
        /// (posh#225 Stage 3.0). The cadence acks go through
        /// `handle_frame_ack` on the harness clock; the attach and final
        /// acks do not, so the t = 0 keyframe is never a sample.
        ack_latency: Option<AckLatency>,
        /// v2 runs ([`FloodCase::v2`], posh#225 Stage 3): `Scrollback2`
        /// bodies queued, the largest's wire size, and `(queued at,
        /// row_offset, rows)` of each. Their rows count in `rows_shipped`
        /// and their bytes in `scrollback_bytes`; `largest_scrollback` and
        /// `scrollback_frames` stay v1-only.
        history_bodies: usize,
        largest_history_body: usize,
        history_sends: Vec<(u64, u64, usize)>,
        /// v2 runs: the viewport model's tally ([`FloodViewport`]) — rows it
        /// appended (each at most once), rows it already held, forward
        /// jumps and the rows they skipped (with where each landed), rows
        /// that differed from the terminal's, rows discarded as another
        /// epoch's, and its final count `t` (its cumulative ack).
        rows_unique: u64,
        rows_repeated: u64,
        forward_jumps: usize,
        rows_jumped: u64,
        jump_ends: Vec<u64>,
        rows_mismatched: u64,
        rows_stale: u64,
        viewport_rows: u64,
        /// Extent runs ([`FloodCase::extent`], posh#225 Stage 4): forward
        /// jumps the viewport's extent did not mark as evicted, its extent
        /// at the end, and the rows that extent says are still arriving
        /// (`avail_rows - max(t, evicted_upto)`); `None` without an extent.
        jumps_unmarked: usize,
        end_extent: Option<caps::Scrollback2Extent>,
        rows_arriving: Option<u64>,
        /// Paced runs: visible frames built while the producer held no acked
        /// base (the lost-base regime, [`FloodAcks::Lagged`]). `full_frames`
        /// cannot show it under a flood: `DumpDiff` also sends a `Full`
        /// whenever the diff is no net win, which a screen the flood
        /// replaced between two paced frames always is.
        baseless_frames: usize,
    }

    /// One flood run's knobs.
    #[derive(Clone, Copy)]
    struct FloodCase {
        /// PTY chunk size fed per `broadcast_output` (production reads 4096).
        chunk: usize,
        acks: FloodAcks,
        drain: FloodDrain,
        /// Rows scrolled into the ring BEFORE the client attaches (a
        /// long-lived session's history); `sb_floor` is anchored past them
        /// exactly as the daemon's Init arm does.
        prefill_rows: usize,
        /// `Some(ms)`: the client advertises `CAP_PACED`, and a fake clock
        /// advances `ms` per chunk, the paced send pass running after each
        /// one (`1` models ~4 MB/s of 4 KiB reads, the `nix gc` shape).
        /// Under it `Lagged(k)` is an RTT of `k * ms` ms ([`FloodAcks::Lagged`]).
        /// `None`: today's per-read-frame client, exactly as before pacing.
        pace: Option<u64>,
        /// Paced runs only: the client's Init also carries
        /// `SCROLLBACK2` (epoch 0: it holds none), so it takes RFC 0009 v2
        /// history (posh#225 Stage 3), modelled by a [`FloodViewport`] whose
        /// cumulative ack follows `acks` on the same RTT clock.
        v2: bool,
        /// v2 runs only: the Init also asks for the v2 extent
        /// (`CAP_SCROLLBACK2_EXTENT`, posh#225 Stage 4), which the
        /// [`FloodViewport`] adopts and judges each forward jump against.
        extent: bool,
    }

    /// A v2 run's tail under [`FloodAcks::Never`]: history cannot progress
    /// without an ack (the window stays full), so the tail runs this much
    /// fake time past the flood — long enough for resends at 1, 2, 4 and
    /// 8 × `HISTORY_RESEND_INITIAL_MS` — and stops.
    const NEVER_ACK_TAIL_MS: u64 = 20_000;

    /// The longest any paced tail may run past the flood (fake time): a
    /// tail that would not terminate fails here instead of hanging.
    const HISTORY_TAIL_BOUND_MS: u64 = 120_000;

    /// The remote viewport's v2 apply rules (RFC 0009 §1.1/§3), exactly as
    /// `remote::client` implements them: adopt the epoch from any frame's
    /// id-10 ack, resetting the count on a change; then, for a
    /// `Scrollback2` body, wrong epoch → discarded (stale); fully covered →
    /// dropped (repeated); partial overlap → only the tail past `t` is
    /// appended (the prefix counts as repeated; posh#225 Stage 3 Part A
    /// changed the viewport to this); `row_offset > t` → a forward jump. Each
    /// appended row is checked against `reference` (relative row r is the
    /// r-th row scrolled after attach). With the extent (posh#225 Stage 4)
    /// it also keeps the newest extent of its epoch (the per-field maximum)
    /// and counts a forward jump NOT marked by it — one landing above the
    /// sender's floor, which only a lost body could produce. The harness
    /// is lossless, so every jump should be marked; the model keeps today's
    /// jump accounting either way.
    #[derive(Default)]
    struct FloodViewport {
        epoch: Option<u8>,
        t: u64,
        extent: Option<caps::Scrollback2Extent>,
        jumps_unmarked: usize,
        /// `(at, t)` after every frame applied: what a `Lagged` ack reads.
        log: Vec<(u64, u64)>,
        reference: Vec<Vec<u8>>,
        rows_unique: u64,
        rows_repeated: u64,
        forward_jumps: usize,
        rows_jumped: u64,
        jump_ends: Vec<u64>,
        rows_mismatched: u64,
        rows_stale: u64,
    }

    impl FloodViewport {
        fn apply(&mut self, frame: &ServerFrame, at: u64) {
            if let Some(epoch) = caps::find(&frame.caps, caps::CAP_SCROLLBACK2)
                .and_then(|cap| caps::decode_scrollback2_ack(&cap.payload).ok())
            {
                if self.epoch != Some(epoch) {
                    self.epoch = Some(epoch);
                    self.t = 0;
                    self.extent = None;
                }
            }
            if let Some(x) = caps::find(&frame.caps, caps::CAP_SCROLLBACK2_EXTENT)
                .and_then(|cap| caps::decode_scrollback2_extent(&cap.payload))
                .filter(|x| self.epoch == Some(x.epoch))
            {
                self.extent = Some(match self.extent {
                    Some(held) => caps::Scrollback2Extent {
                        epoch: x.epoch,
                        avail_rows: held.avail_rows.max(x.avail_rows),
                        evicted_upto: held.evicted_upto.max(x.evicted_upto),
                    },
                    None => x,
                });
            }
            if let FrameBody::Scrollback2 {
                epoch,
                row_offset,
                rows,
            } = &frame.body
            {
                let n = rows.len() as u64;
                let end = row_offset + n;
                if self.epoch != Some(*epoch) {
                    self.rows_stale += n;
                } else if end <= self.t {
                    self.rows_repeated += n;
                } else {
                    let skip = self.t.saturating_sub(*row_offset);
                    self.rows_repeated += skip;
                    if *row_offset > self.t {
                        self.forward_jumps += 1;
                        self.rows_jumped += row_offset - self.t;
                        self.jump_ends.push(*row_offset);
                        if self.extent.is_none_or(|x| *row_offset > x.evicted_upto) {
                            self.jumps_unmarked += 1;
                        }
                    }
                    for (i, row) in rows.iter().enumerate().skip(skip as usize) {
                        if self.reference.get((row_offset + i as u64) as usize) != Some(row) {
                            self.rows_mismatched += 1;
                        }
                    }
                    self.rows_unique += n - skip;
                    self.t = end;
                }
            }
            self.log.push((at, self.t));
        }

        /// Append the rows `term` scrolled since it held `seen` in total.
        fn record_scrolled(&mut self, term: &Terminal, seen: &mut u64) {
            let total = term.primary_scrollback_total();
            let len = term.primary_scrollback_len();
            let k = (total - *seen) as usize;
            assert!(k <= len, "one step scrolled more rows than the ring holds");
            self.reference
                .extend((len - k..len).map(|i| term.dump_scrollback_row(i).unwrap()));
            *seen = total;
        }

        /// The cumulative ack the viewport sends at `now` under `acks`
        /// (`Lagged(k)`: its count as of `now - k * ms`), with its epoch;
        /// `None` before it holds an epoch.
        fn ack(&self, acks: FloodAcks, step: usize, now: u64, ms: u64) -> Option<(u8, u64)> {
            let epoch = self.epoch?;
            let rows = match acks {
                FloodAcks::Never => None,
                FloodAcks::EveryNewest(k) => step.is_multiple_of(k).then_some(self.t),
                FloodAcks::Lagged(k) => {
                    let cutoff = now.checked_sub(k as u64 * ms)?;
                    self.log
                        .partition_point(|&(at, _)| at <= cutoff)
                        .checked_sub(1)
                        .map(|i| self.log[i].1)
                }
            }?;
            Some((epoch, rows))
        }

        /// When the next ack that would move the daemon's `acked` arrives,
        /// after `now` — the v2 tail's wake source beside the frame ack.
        fn next_ack_at(&self, acks: FloodAcks, acked: u64, now: u64, ms: u64) -> Option<u64> {
            match acks {
                FloodAcks::Never => None,
                FloodAcks::EveryNewest(_) => (self.t > acked).then_some(now + ms),
                FloodAcks::Lagged(k) => self
                    .log
                    .iter()
                    .map(|&(at, t)| (t, at + k as u64 * ms))
                    .find(|&(t, arrives)| t > acked && arrives > now)
                    .map(|(_, arrives)| arrives),
            }
        }
    }

    /// Bytes in every [`newline_flood`] line, CRLF included. Each is 100
    /// columns wide — it never wraps at the harness's 200 — so once the grid
    /// is full a line scrolls exactly one row.
    const FLOOD_LINE_LEN: usize = 102;

    /// The `SO_SNDBUF`/`SO_RCVBUF` request [`FloodDrain::OneWritePerChunk`]
    /// pins its socketpair to. Sized so that a host which takes the request
    /// literally is still expected to accept, in one write, the largest
    /// per-chunk burst the regression test's own bounds allow
    /// ([`FLOOD_WRITE_CAPACITY_NEEDED`]). Expected, not known:
    /// [`flood_socket_supports_bounds`] checks what the host actually
    /// granted, and the bounds are skipped where it fell short.
    const FLOOD_SOCKET_BUFFER: libc::c_int = 128 * 1024;

    /// The most rows one PTY chunk of `chunk` flood bytes can scroll off.
    fn flood_rows_per_chunk(chunk: usize) -> u64 {
        chunk.div_ceil(FLOOD_LINE_LEN) as u64
    }

    /// `total` bytes of distinct ~100-byte lines, CRLF-terminated as a PTY in
    /// ONLCR mode delivers them (the `nix gc` "deleting '/nix/store/…'" shape).
    fn newline_flood(total: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(total + 128);
        let mut i: u64 = 0;
        while out.len() < total {
            let hash = u128::from(i).wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835);
            let line =
                format!("{i:08} deleting '/nix/store/{hash:032x}-some-derivation-output-name-{i:08}'\r\n");
            assert_eq!(line.len(), FLOOD_LINE_LEN);
            out.extend_from_slice(line.as_bytes());
            i += 1;
        }
        out.truncate(total);
        out
    }

    /// Request `bytes` for one of a socket's buffers (`SO_SNDBUF` or
    /// `SO_RCVBUF`). The kernel may grant a different size; see
    /// [`FloodDrain::OneWritePerChunk`].
    fn pin_socket_buffer(stream: &UnixStream, option: libc::c_int, bytes: libc::c_int) {
        // SAFETY: setsockopt on a live fd, with a pointer to one `c_int`
        // that outlives the call and that `c_int`'s size alongside it.
        let rc = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                std::ptr::from_ref(&bytes).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "setsockopt({option}): {}", std::io::Error::last_os_error());
    }

    /// The ideal reader: empty the (non-blocking) socket, returning how many
    /// bytes arrived.
    fn read_until_dry(peer: &mut UnixStream, sink: &mut [u8]) -> u64 {
        let mut total = 0u64;
        loop {
            match std::io::Read::read(peer, sink) {
                Ok(0) => return total,
                Ok(n) => total += n as u64,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return total,
                Err(e) => panic!("reader-side read: {e}"),
            }
        }
    }

    /// Drive one flood through the production `broadcast_output` path for a
    /// lossy, scrollback-wanting client shaped like the M2 bridge's daemon
    /// Init (`CAP_LOSSY` + `CAP_SCROLLBACK` + `CAP_BASE_SUM`, relay.rs
    /// `init_payload`/`content_caps`), at the daemon's own ring depth.
    fn measure_flood(flood: &[u8], case: FloodCase) -> FloodRun {
        let (rows, cols) = (50u16, 200u16);
        let mut term = Terminal::with_scrollback(rows, cols, SCROLLBACK);
        for i in 0..case.prefill_rows {
            term.process(format!("{i:08} pre-attach history row, as long as a flood line, padded out to ~100 bytes ........\r\n").as_bytes());
        }
        let mut content = vec![
            caps::Cap { id: caps::CAP_SCROLLBACK, payload: vec![0] },
            caps::Cap { id: caps::CAP_BASE_SUM, payload: vec![] },
        ];
        assert!(!case.v2 || case.pace.is_some(), "v2 history is gated on pacing");
        assert!(!case.extent || case.v2, "the extent rides v2 history");
        if case.v2 {
            content.push(sb2_entry(0, 0));
        }
        if case.extent {
            content.push(caps::encode_scrollback2_extent_request());
        }
        let (mut c, mut peer) = match case.pace {
            Some(_) => paced_conn(rows, cols, &content),
            None => lossy_conn(rows, cols, &content),
        };
        assert!(c.lossy && c.wants_scrollback());
        assert_eq!(c.is_paced(), case.pace.is_some());
        let gated = case.drain.is_gated();
        if gated {
            pin_socket_buffer(&c.stream, libc::SO_SNDBUF, FLOOD_SOCKET_BUFFER);
            pin_socket_buffer(&peer, libc::SO_RCVBUF, FLOOD_SOCKET_BUFFER);
        }
        c.stream.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut sink = vec![0u8; 1 << 20];
        let mut run = FloodRun::default();
        if gated {
            // Probe what one write into the empty socket accepts here, then
            // hand the socket back empty.
            let probe = vec![0u8; 4 << 20];
            run.socket_write_capacity = std::io::Write::write(&mut c.stream, &probe).unwrap();
            read_until_dry(&mut peer, &mut sink);
        }
        // Forward-only scrollback from attach, as the daemon's Init arm sets it.
        c.sb_floor = term.primary_scrollback_total();
        let mut viewport = case.v2.then(|| {
            c.open_history(&term);
            assert!(c.has_history(), "a paced SCROLLBACK2 Init opens a cursor");
            FloodViewport::default()
        });
        // The v2 reference's row count so far (relative row 0 is sb_floor).
        let mut seen_total = c.sb_floor;
        // The attach keyframe, acked: the baseline scrollback frames gate on.
        // It never touches the socket, so it counts as delivered. A paced
        // client's goes through the real path — owed, then built by the send
        // pass at t = 0 — so its first fresh frame is on the clock.
        match case.pace {
            Some(_) => {
                assert!(c.request_frame_from(&term));
                send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), 0);
                assert_eq!(c.producer.as_ref().unwrap().last_visible_num(), 1, "the attach keyframe is frame 1");
            }
            None => assert!(c.request_frame_from(&term)),
        }
        // A v2 viewport adopts its epoch from the keyframe's id-10 ack.
        if let Some(v) = viewport.as_mut() {
            for frame in decode_server_frames(&c.write_buf) {
                v.apply(&frame, 0);
            }
            assert_eq!(v.epoch, Some(1), "the attach keyframe carries the epoch");
        }
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        c.write_buf.clear();

        // newest[i] = the newest frame number after queueing step i
        // (newest[0] = attach). A non-paced client's queueing step is the
        // chunk's broadcast; a paced client's is the send pass after it.
        let mut newest: Vec<u64> = vec![1];
        // Paced runs: newest_avail[i] = when newest[i] reached the reader (the
        // attach keyframe at t = 0; a send pass's frame one clock step after
        // the pass) — what a `Lagged` round trip counts from.
        let mut newest_avail: Vec<u64> = vec![0];
        let mut ledger = FloodLedger::new(gated, case.drain, viewport);
        // The paced client's clock: chunk i is fed at `i * ms`.
        let mut now: u64 = 0;
        for piece in flood.chunks(case.chunk) {
            term.process(piece);
            if let Some(v) = ledger.viewport.as_mut() {
                v.record_scrolled(&term, &mut seen_total);
            }
            let before = c.write_buf.len();
            broadcast_output(std::slice::from_mut(&mut c), &term, piece);
            run.fed += piece.len();
            run.chunks += 1;

            if case.pace.is_some() {
                assert_eq!(c.write_buf.len(), before, "a paced client is only marked dirty");
            } else {
                ledger.account(&mut run, &c.write_buf[before..], true, now);
                run.peak_write_buf = run.peak_write_buf.max(c.write_buf.len());
                newest.push(c.producer.as_ref().unwrap().current_num());
            }

            // Drain BEFORE the ack: what the reader can ack this chunk is
            // decided by what this chunk's write delivered.
            ledger.drain(&mut run, &mut c, case.drain, &mut peer, &mut sink, now);
            let eligible = match case.pace {
                Some(ms) => paced_ack_eligible(case.acks, run.chunks, &newest, &newest_avail, now, ms),
                None => flood_ack_eligible(case.acks, run.chunks, &newest),
            };
            if let Some(n) = ledger.deliverable_ack(eligible) {
                handle_frame_ack(&mut c, &ipc::encode_frame_ack(n, 0), &term, now);
            }
            if let Some(ms) = case.pace {
                ledger.ack_history(&mut c, case.acks, run.chunks, now, ms);
            }

            if let Some(ms) = case.pace {
                paced_flood_send_pass(&mut c, &term, now, &mut ledger, &mut run, &mut newest, true);
                newest_avail.push(now + ms);
                now += ms;
            }

            if c.write_buf.len() > MAX_CLIENT_BACKLOG {
                // The daemon loop's `clients.retain` drops the client here.
                run.crossed_backlog_at = Some(run.fed);
                break;
            }
        }

        // A paced client's idle tail: the PTY has gone quiet, but the loop
        // keeps draining, acking and running the send pass until the client
        // owes nothing and has nothing queued. The clock moves the way the
        // daemon loop wakes: one step while bytes are queued (`POLLOUT`), and
        // otherwise straight to the nearest event — the send opportunity
        // `paced_poll_timeout` names, or the next ack's arrival — so the tail
        // is a quiescence proof of the production wake path, not a scan.
        //
        // The stale-screen bound is on the IDLE time — the clock spent with
        // an empty buffer while a frame was owed, which is pacing's own
        // waiting — and is a few ack waits. Time spent draining queued bytes
        // is the reader's (a `Trickle` reader can take seconds over one
        // ring-sized scrollback frame); each such step must make progress.
        //
        // A v2 run's tail also lasts until its history is done: no fresh rows
        // and nothing in flight — or, under `Never`, `NEVER_ACK_TAIL_MS`
        // (nothing in flight is ever acked). It also wakes for the next v2
        // ack, and asks the poll about history (`Some(&term)`), as the daemon
        // loop does while no overlay is up.
        if let (Some(ms), None) = (case.pace, run.crossed_backlog_at) {
            let flood_end = now;
            let never_tail_end = flood_end + NEVER_ACK_TAIL_MS;
            let mut step = run.chunks;
            let mut idle: u64 = 0;
            let what = format!(
                "acks={} drain={} v2={} prefill={}",
                case.acks.label(),
                case.drain.label(),
                case.v2,
                case.prefill_rows
            );
            let history_pending = |c: &ClientConn, now: u64| match history_of(c) {
                None => false,
                Some(_) if matches!(case.acks, FloodAcks::Never) => now < never_tail_end,
                // Fresh rows or rows in flight, whatever the window.
                Some(h) => h.next_due(term.primary_scrollback_total(), 0, u64::MAX).is_some(),
            };
            while c.owes_paced_frame() || !c.write_buf.is_empty() || history_pending(&c, now) {
                assert!(
                    now - flood_end <= HISTORY_TAIL_BOUND_MS,
                    "{what}: the tail ran {} ms past the flood without settling",
                    now - flood_end,
                );
                let queued = c.write_buf.len();
                ledger.drain(&mut run, &mut c, case.drain, &mut peer, &mut sink, now);
                assert!(
                    queued == 0 || c.write_buf.len() < queued,
                    "{what}: a write at t={now} moved nothing: the tail would never end",
                );
                step += 1;
                let eligible = paced_ack_eligible(case.acks, step, &newest, &newest_avail, now, ms);
                if let Some(n) = ledger.deliverable_ack(eligible) {
                    handle_frame_ack(&mut c, &ipc::encode_frame_ack(n, 0), &term, now);
                }
                ledger.ack_history(&mut c, case.acks, step, now, ms);
                paced_flood_send_pass(&mut c, &term, now, &mut ledger, &mut run, &mut newest, false);
                newest_avail.push(now + ms);
                if !c.write_buf.is_empty() || !(c.owes_paced_frame() || history_pending(&c, now)) {
                    now += ms;
                    continue;
                }
                let timeout = paced_poll_timeout(std::slice::from_ref(&c), Some(&term), now);
                assert!(
                    timeout >= 0 || !c.owes_paced_frame(),
                    "{what}: dirty and idle at t={now} but the poll would block"
                );
                assert!(timeout != 0, "{what}: due at t={now} yet the send pass sent nothing: a busy loop");
                let acked = c.producer.as_ref().unwrap().acked_num();
                let history_ack = history_of(&c).zip(ledger.viewport.as_ref()).and_then(|(h, v)| {
                    v.next_ack_at(case.acks, h.acked_rows(), now, ms)
                });
                let never_end = (case.v2 && matches!(case.acks, FloodAcks::Never) && now < never_tail_end)
                    .then_some(never_tail_end);
                let wake = [
                    u64::try_from(timeout).ok().map(|t| now + t),
                    paced_next_ack_at(case.acks, &newest, &newest_avail, acked, now, ms),
                    history_ack,
                    never_end,
                ]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or_else(|| panic!("{what}: nothing would ever wake the tail at t={now}"));
                if c.owes_paced_frame() {
                    idle += wake - now;
                }
                now = wake;
                assert!(
                    idle <= 4 * PACED_ACK_WAIT_MS,
                    "{what}: the last screen was owed with nothing queued for {idle} ms (t={now})",
                );
            }
            let last = c.producer.as_ref().unwrap().last_visible_num();
            c.apply_frame_ack(&ipc::encode_frame_ack(last, 0));
            run.last_screen_delivered = Some(
                c.producer.as_ref().unwrap().acked_dump() == Some(&term.dump_vt_mirror(rows, cols)[..]),
            );
        }
        run.rows_scrolled = term.primary_scrollback_total() - c.sb_floor;
        run.rows_acked = match history_of(&c) {
            Some(h) => h.acked_rows(),
            None => c.acked_sb_total.max(c.sb_floor) - c.sb_floor,
        };
        run.ack_latency = c.pacing.as_ref().map(|p| p.acks);
        if let Some(v) = ledger.viewport {
            run.rows_unique = v.rows_unique;
            run.rows_repeated = v.rows_repeated;
            run.forward_jumps = v.forward_jumps;
            run.rows_jumped = v.rows_jumped;
            run.jump_ends = v.jump_ends;
            run.rows_mismatched = v.rows_mismatched;
            run.rows_stale = v.rows_stale;
            run.viewport_rows = v.t;
            run.jumps_unmarked = v.jumps_unmarked;
            run.end_extent = v.extent;
            run.rows_arriving = v.extent.map(|x| x.avail_rows - v.t.max(x.evicted_upto));
        }
        run
    }

    /// One paced send pass of a flood run at `now`, its queued frames
    /// accounted like a non-paced chunk's.
    fn paced_flood_send_pass(
        c: &mut ClientConn,
        term: &Terminal,
        now: u64,
        ledger: &mut FloodLedger,
        run: &mut FloodRun,
        newest: &mut Vec<u64>,
        during_flood: bool,
    ) {
        let before = c.write_buf.len();
        let producer = c.producer.as_ref().unwrap();
        let (baseless, sent) = (producer.acked_dump().is_none(), producer.last_visible_num());
        send_paced_frames(std::slice::from_mut(c), term, Some(term), now);
        if baseless && c.producer.as_ref().unwrap().last_visible_num() > sent {
            run.baseless_frames += 1;
        }
        ledger.account(run, &c.write_buf[before..], during_flood, now);
        run.peak_write_buf = run.peak_write_buf.max(c.write_buf.len());
        newest.push(c.producer.as_ref().unwrap().current_num());
    }

    /// The frame the cadence would ack at queueing step `step` (1-based),
    /// given `newest` (see [`FloodAcks`]).
    fn flood_ack_eligible(acks: FloodAcks, step: usize, newest: &[u64]) -> Option<u64> {
        match acks {
            FloodAcks::Never => None,
            FloodAcks::EveryNewest(k) => step.is_multiple_of(k).then(|| *newest.last().unwrap()),
            FloodAcks::Lagged(k) => newest.len().checked_sub(1 + k).map(|i| newest[i]),
        }
    }

    /// A paced run's [`flood_ack_eligible`]: `Lagged(k)` is an RTT of
    /// `k * ms` — the newest frame that had reached the reader by then
    /// (`avail`, nondecreasing, parallel to `newest`).
    fn paced_ack_eligible(
        acks: FloodAcks,
        step: usize,
        newest: &[u64],
        avail: &[u64],
        now: u64,
        ms: u64,
    ) -> Option<u64> {
        match acks {
            FloodAcks::Lagged(k) => {
                let cutoff = now.checked_sub(k as u64 * ms)?;
                avail.partition_point(|&at| at <= cutoff).checked_sub(1).map(|i| newest[i])
            }
            other => flood_ack_eligible(other, step, newest),
        }
    }

    /// When the next ack that would move a paced client's base arrives,
    /// after `now` — the tail's other wake source besides the send
    /// opportunity. `None` when none is coming.
    fn paced_next_ack_at(
        acks: FloodAcks,
        newest: &[u64],
        avail: &[u64],
        acked: u64,
        now: u64,
        ms: u64,
    ) -> Option<u64> {
        match acks {
            FloodAcks::Never => None,
            FloodAcks::EveryNewest(_) => (*newest.last().unwrap() > acked).then_some(now + ms),
            FloodAcks::Lagged(k) => newest
                .iter()
                .zip(avail)
                .map(|(&num, &at)| (num, at + k as u64 * ms))
                .find(|&(num, arrives)| num > acked && arrives > now)
                .map(|(_, arrives)| arrives),
        }
    }

    /// A flood run's bookkeeping on its client's stream, in bytes queued
    /// since attach: what each queueing step put into `write_buf`, what has
    /// left it, and — for a delivery-gated run — what the reader received.
    struct FloodLedger {
        gated: bool,
        queued_total: u64,
        drained_total: u64,
        delivered_total: u64,
        /// Gated runs: where each not-yet-delivered frame ends in the stream.
        undelivered: std::collections::VecDeque<(u64, u64)>,
        /// Gated runs: the newest frame the reader has received in full.
        delivered_frontier: u64,
        /// Where each frame still (at least partly) in `write_buf` ends, and
        /// whether it is a visible frame.
        in_buf: std::collections::VecDeque<(u64, bool)>,
        /// How many of `in_buf`'s frames are visible.
        visible_in_buf: usize,
        /// v2 runs: the viewport model, and — gated runs — each decoded
        /// frame not yet delivered, with where it ends in the stream. A
        /// gated drain hands the frames it delivered to the model at that
        /// step's clock; `FloodDrain::Always` hands them over as they are
        /// queued; `FloodDrain::Never` never does.
        viewport: Option<FloodViewport>,
        pending: std::collections::VecDeque<(u64, ServerFrame)>,
        deliver_on_queue: bool,
    }

    impl FloodLedger {
        fn new(gated: bool, drain: FloodDrain, viewport: Option<FloodViewport>) -> Self {
            FloodLedger {
                gated,
                queued_total: 0,
                drained_total: 0,
                delivered_total: 0,
                undelivered: std::collections::VecDeque::new(),
                delivered_frontier: 1,
                in_buf: std::collections::VecDeque::new(),
                visible_in_buf: 0,
                viewport,
                pending: std::collections::VecDeque::new(),
                deliver_on_queue: matches!(drain, FloodDrain::Always),
            }
        }

        /// Account the frames one queueing step (at `now`) appended to
        /// `write_buf`.
        fn account(&mut self, run: &mut FloodRun, queued: &[u8], during_flood: bool, now: u64) {
            let mut fb = FrameBuffer::new();
            fb.feed(queued);
            while let Some(rec) = fb.next().unwrap() {
                assert_eq!(rec.tag, Tag::Frame);
                let wire = ipc::HEADER_LEN + rec.payload.len();
                let frame = ServerFrame::decode(&rec.payload).unwrap();
                self.queued_total += wire as u64;
                if self.gated {
                    self.undelivered.push_back((frame.frame_num, self.queued_total));
                }
                let visible = !matches!(frame.body, FrameBody::Scrollback { .. } | FrameBody::Scrollback2 { .. });
                self.in_buf.push_back((self.queued_total, visible));
                self.visible_in_buf += usize::from(visible);
                match &frame.body {
                    FrameBody::Scrollback { rows, .. } => {
                        run.scrollback_frames += 1;
                        run.scrollback_bytes += wire as u64;
                        run.rows_shipped += rows.len() as u64;
                        if wire > run.largest_scrollback {
                            run.largest_scrollback = wire;
                            run.largest_scrollback_rows = rows.len();
                        }
                    }
                    FrameBody::Scrollback2 { row_offset, rows, .. } => {
                        run.history_bodies += 1;
                        run.largest_history_body = run.largest_history_body.max(wire);
                        run.history_sends.push((now, *row_offset, rows.len()));
                        run.scrollback_bytes += wire as u64;
                        run.rows_shipped += rows.len() as u64;
                    }
                    body => {
                        if matches!(body, FrameBody::Full(_)) {
                            run.full_frames += 1;
                        }
                        run.visible_bytes += wire as u64;
                        run.largest_visible = run.largest_visible.max(wire);
                        run.visible_frames += 1;
                        if during_flood {
                            run.visible_frames_during_flood += 1;
                        }
                    }
                }
                if let Some(v) = self.viewport.as_mut() {
                    if self.deliver_on_queue {
                        v.apply(&frame, now);
                    } else if self.gated {
                        self.pending.push_back((self.queued_total, frame));
                    }
                }
            }
            run.max_visible_queued = run.max_visible_queued.max(self.visible_in_buf);
        }

        /// The v2 viewport's cumulative ack at `now` under `acks`, applied
        /// through `Tag::ClientCaps`'s path (`absorb_client_caps`). A no-op
        /// for a v1 run, and for an ack the cursor already has.
        fn ack_history(&self, c: &mut ClientConn, acks: FloodAcks, step: usize, now: u64, ms: u64) {
            if let Some((epoch, rows)) = self.viewport.as_ref().and_then(|v| v.ack(acks, step, now, ms)) {
                c.absorb_client_caps(&[sb2_entry(epoch, rows)], now, false);
            }
        }

        /// The drain step, per [`FloodDrain`].
        fn drain(
            &mut self,
            run: &mut FloodRun,
            c: &mut ClientConn,
            mode: FloodDrain,
            peer: &mut UnixStream,
            sink: &mut [u8],
            now: u64,
        ) {
            let before = c.write_buf.len();
            match mode {
                FloodDrain::Always => c.write_buf.clear(),
                FloodDrain::Never => {}
                FloodDrain::OneWritePerChunk | FloodDrain::Trickle(_) => {
                    let limit = match mode {
                        FloodDrain::Trickle(n) => n.min(c.write_buf.len()),
                        _ => c.write_buf.len(),
                    };
                    match std::io::Write::write(&mut c.stream, &c.write_buf[..limit]) {
                        Ok(n) => {
                            c.write_buf.drain(..n);
                            run.largest_socket_write = run.largest_socket_write.max(n);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => panic!("daemon-side write: {e}"),
                    }
                    self.delivered_total += read_until_dry(peer, sink);
                    while self.undelivered.front().is_some_and(|&(_, end)| end <= self.delivered_total) {
                        self.delivered_frontier = self.undelivered.pop_front().unwrap().0;
                    }
                    while self.pending.front().is_some_and(|(end, _)| *end <= self.delivered_total) {
                        let (_, frame) = self.pending.pop_front().unwrap();
                        if let Some(v) = self.viewport.as_mut() {
                            v.apply(&frame, now);
                        }
                    }
                }
            }
            self.drained_total += (before - c.write_buf.len()) as u64;
            while self.in_buf.front().is_some_and(|&(end, _)| end <= self.drained_total) {
                let (_, visible) = self.in_buf.pop_front().unwrap();
                self.visible_in_buf -= usize::from(visible);
            }
            run.peak_undrained = run.peak_undrained.max(c.write_buf.len());
        }

        /// The ack the client sends for the cadence's `eligible` frame:
        /// under a gated run, the newest frame that is both eligible and
        /// delivered (frames are numbered and delivered in order, so the
        /// smaller of the two). An ack at or below the last one is a no-op
        /// (`FrameProducer::ack`).
        fn deliverable_ack(&self, eligible: Option<u64>) -> Option<u64> {
            if self.gated {
                eligible.map(|n| n.min(self.delivered_frontier))
            } else {
                eligible
            }
        }
    }

    fn print_flood_header() {
        println!(
            "{:>6} {:>10} {:>7} | {:>8} {:>6} {:>9} | {:>10} {:>7} | {:>11} {:>11} {:>7} | {:>9} {:>9} {:>6} | {:>5} {:>5} {:>9} {:>8} {:>8} | {:>5} {:>6} {:>7} {:>5} {:>4} | {:>6} | {:>2} {:>5} {:>6} {:>6} {:>5} {:>3} {:>6} {:>4} | {:>4} {:>5}",
            "chunk", "acks", "drain", "fed", "chunks", "cross@fed", "peak_wbuf", "max_wr", "vis_bytes", "sb_bytes", "q/fed",
            "max_vis", "max_sb", "sbrows", "full", "sbfrm", "rows_sent", "scrolled", "sb_acked",
            "paced", "visfrm", "inflood", "maxvq", "last", "nobase",
            "v2", "hbody", "max_hb", "uniq", "rep", "jmp", "jumped", "mism",
            "unmk", "arr",
        );
    }

    fn print_flood_row(case: FloodCase, r: &FloodRun) {
        let v2 = |n: String| if case.v2 { n } else { "-".to_string() };
        let ext = |n: String| if case.extent { n } else { "-".to_string() };
        println!(
            "{:>6} {:>10} {:>7} | {:>8} {:>6} {:>9} | {:>10} {:>7} | {:>11} {:>11} {:>7.1} | {:>9} {:>9} {:>6} | {:>5} {:>5} {:>9} {:>8} {:>8} | {:>5} {:>6} {:>7} {:>5} {:>4} | {:>6} | {:>2} {:>5} {:>6} {:>6} {:>5} {:>3} {:>6} {:>4} | {:>4} {:>5}",
            case.chunk,
            case.acks.label(),
            case.drain.label(),
            r.fed,
            r.chunks,
            r.crossed_backlog_at.map_or_else(|| "never".to_string(), |at| at.to_string()),
            r.peak_write_buf,
            r.largest_socket_write,
            r.visible_bytes,
            r.scrollback_bytes,
            (r.visible_bytes + r.scrollback_bytes) as f64 / r.fed as f64,
            r.largest_visible,
            r.largest_scrollback,
            r.largest_scrollback_rows,
            r.full_frames,
            r.scrollback_frames,
            r.rows_shipped,
            r.rows_scrolled,
            r.rows_acked,
            case.pace.map_or_else(|| "-".to_string(), |ms| format!("{ms}ms")),
            r.visible_frames,
            r.visible_frames_during_flood,
            r.max_visible_queued,
            r.last_screen_delivered.map_or("-", |ok| if ok { "yes" } else { "NO" }),
            case.pace.map_or_else(|| "-".to_string(), |_| r.baseless_frames.to_string()),
            if case.v2 { "v2" } else { "-" },
            v2(r.history_bodies.to_string()),
            v2(r.largest_history_body.to_string()),
            v2(r.rows_unique.to_string()),
            v2(r.rows_repeated.to_string()),
            v2(r.forward_jumps.to_string()),
            v2(r.rows_jumped.to_string()),
            v2(r.rows_mismatched.to_string()),
            ext(r.jumps_unmarked.to_string()),
            ext(r.rows_arriving.map_or_else(|| "none".to_string(), |n| n.to_string())),
        );
    }

    /// posh#225 MEASUREMENT (assertion-free; `#[ignore]`d because it is slow
    /// and proves nothing by passing): how much the daemon queues for a lossy
    /// scrollback client during a newline flood, by PTY-chunk size and ack
    /// cadence. Prints one table row per run. Debug builds take many minutes;
    /// run it optimized:
    /// `just debug-cargo test --release -p posh --bin posh posh225_flood_backlog_measurement -- --ignored --nocapture`.
    ///
    /// The daemon's PTY read buffer is 4096 bytes, so 4 KiB is the largest
    /// chunk production can produce; 64 KiB is included only for the shape.
    #[test]
    #[ignore = "posh#225 measurement: slow, prints a table, asserts nothing"]
    fn posh225_flood_backlog_measurement() {
        const KIB: usize = 1024;
        let flood = newline_flood(2 * KIB * KIB);
        let cadences = [
            FloodAcks::Never,
            FloodAcks::EveryNewest(1),
            FloodAcks::EveryNewest(10),
            FloodAcks::EveryNewest(100),
            FloodAcks::Lagged(1),
            FloodAcks::Lagged(4),
            FloodAcks::Lagged(5),
            FloodAcks::Lagged(10),
            FloodAcks::Lagged(100),
        ];
        println!(
            "\nposh#225 flood: 50x200, ring {SCROLLBACK} rows, ~102-byte lines, {} bytes offered per run",
            flood.len()
        );
        println!("\n== A: client attaches to an EMPTY ring ==");
        print_flood_header();
        for chunk in [KIB, 4 * KIB, 64 * KIB] {
            for acks in cadences {
                for drain in [FloodDrain::Never, FloodDrain::Always] {
                    let case =
                        FloodCase { chunk, acks, drain, prefill_rows: 0, pace: None, v2: false, extent: false };
                    print_flood_row(case, &measure_flood(&flood, case));
                }
            }
        }
        println!("\n== B: client attaches to a FULL ring ({SCROLLBACK} rows of history), production chunk size ==");
        print_flood_header();
        for acks in cadences {
            for drain in [FloodDrain::Never, FloodDrain::Always] {
                let case =
                    FloodCase {
                        chunk: 4 * KIB,
                        acks,
                        drain,
                        prefill_rows: SCROLLBACK + 200,
                        pace: None,
                        v2: false,
                        extent: false,
                    };
                print_flood_row(case, &measure_flood(&flood, case));
            }
        }
    }

    /// posh#225 MEASUREMENT, the socket-accurate companion to
    /// [`posh225_flood_backlog_measurement`]: the same flood, but `write_buf`
    /// drains the way `daemon_main` drains it — one non-blocking
    /// `stream.write` per PTY chunk into a real socketpair — against a reader
    /// that empties the socket instantly. Shows whether the backlog reaches
    /// `MAX_CLIENT_BACKLOG` with a healthy reader. Acks are delivery-gated
    /// here ([`FloodAcks`]): the reader acks only what the one write per
    /// chunk has actually delivered.
    /// `just debug-cargo test --release -p posh --bin posh posh225_flood_backlog_ideal_reader_measurement -- --ignored --nocapture`.
    #[test]
    #[ignore = "posh#225 measurement: slow, prints a table, asserts nothing"]
    fn posh225_flood_backlog_ideal_reader_measurement() {
        const KIB: usize = 1024;
        let flood = newline_flood(2 * KIB * KIB);
        println!(
            "\nposh#225 flood, one socket write per chunk + ideal reader, delivery-gated acks: 50x200, ring {SCROLLBACK} rows, {} bytes offered per run",
            flood.len()
        );
        let probe = measure_flood(
            &flood[..KIB],
            FloodCase {
                chunk: KIB,
                acks: FloodAcks::Never,
                drain: FloodDrain::OneWritePerChunk,
                prefill_rows: 0,
                pace: None,
                v2: false,
                extent: false,
            },
        );
        println!(
            "socket buffers pinned to {FLOOD_SOCKET_BUFFER} bytes; one write into the empty socket accepts {} bytes on this host",
            probe.socket_write_capacity
        );
        print_flood_header();
        // Per-read frames first, then the same runs for a paced client whose
        // fake clock advances 1 ms per chunk (see `FloodCase::pace`), plus
        // real round trips for it — `lag k` is then a k ms RTT: between the
        // floor and the ack wait, past the wait, and past about five waits
        // (RTT > 5 x PACED_ACK_WAIT_MS - pace): the 8-frame outstanding window
        // holds 4 visible + scrollback pairs at one pair per wait, and the
        // acked scrollback slot of the pair sent at T is still the oldest held
        // after 4 more pairs, so the 5th (T + 1250 ms) evicts it. `lag 1500`
        // is past that cliff. Only the 1 KiB-chunk flood (2048 ms) is long
        // enough for that RTT; the 4 KiB (512 ms) row is uninformative.
        for pace in [None, Some(1)] {
            let mut cadences = vec![
                FloodAcks::EveryNewest(1),
                FloodAcks::Lagged(1),
                FloodAcks::Lagged(4),
                FloodAcks::Lagged(5),
                FloodAcks::Never,
            ];
            if pace.is_some() {
                cadences.extend([FloodAcks::Lagged(50), FloodAcks::Lagged(300), FloodAcks::Lagged(1500)]);
            }
            for prefill_rows in [0, SCROLLBACK + 200] {
                for chunk in [KIB, 4 * KIB] {
                    for acks in cadences.iter().copied() {
                        let case = FloodCase {
                            chunk,
                            acks,
                            drain: FloodDrain::OneWritePerChunk,
                            prefill_rows,
                            pace,
                            v2: false,
                            extent: false,
                        };
                        let r = measure_flood(&flood, case);
                        print!("{}", if prefill_rows == 0 { "empty ring " } else { "FULL ring  " });
                        print_flood_row(case, &r);
                    }
                }
            }
        }
        // The same paced flood for a v2 viewport (posh#225 Stage 3): RFC 0009
        // history addressed and acked, at the same RTTs plus `lag 2500` (past
        // the 8-frame outstanding window at one visible frame per ack wait:
        // where a visible base could next be lost), then a slow reader. The
        // flood runs 2048 ms at 1 KiB chunks and 512 ms at 4 KiB; history
        // keeps flowing in the tail.
        for prefill_rows in [0, SCROLLBACK + 200] {
            let label = if prefill_rows == 0 { "empty ring " } else { "FULL ring  " };
            for chunk in [KIB, 4 * KIB] {
                for acks in [
                    FloodAcks::EveryNewest(1),
                    FloodAcks::Lagged(50),
                    FloodAcks::Lagged(300),
                    FloodAcks::Lagged(1500),
                    FloodAcks::Lagged(2500),
                    FloodAcks::Never,
                ] {
                    let case = FloodCase {
                        chunk,
                        ..paced_v2_flood_case(acks, prefill_rows)
                    };
                    print!("{label}");
                    print_flood_row(case, &measure_flood(&flood, case));
                }
            }
            for acks in [FloodAcks::EveryNewest(1), FloodAcks::Lagged(50)] {
                let case = FloodCase {
                    drain: FloodDrain::Trickle(KIB),
                    ..paced_v2_flood_case(acks, prefill_rows)
                };
                print!("{label}");
                print_flood_row(case, &measure_flood(&flood, case));
            }
        }
        // The same v2 viewport asking for the extent (posh#225 Stage 4):
        // `unmk` counts forward jumps it did not mark as evicted (the
        // harness is lossless, so every jump should be marked), `arr` the
        // rows its final extent says are still arriving.
        for prefill_rows in [0, SCROLLBACK + 200] {
            let label = if prefill_rows == 0 { "empty ring " } else { "FULL ring  " };
            for chunk in [KIB, 4 * KIB] {
                for acks in [FloodAcks::EveryNewest(1), FloodAcks::Lagged(300)] {
                    let case = FloodCase {
                        chunk,
                        ..paced_v2x_flood_case(acks, prefill_rows)
                    };
                    print!("{label}");
                    print_flood_row(case, &measure_flood(&flood, case));
                }
            }
            for acks in [FloodAcks::EveryNewest(1), FloodAcks::Lagged(50)] {
                let case = FloodCase {
                    drain: FloodDrain::Trickle(KIB),
                    ..paced_v2x_flood_case(acks, prefill_rows)
                };
                print!("{label}");
                print_flood_row(case, &measure_flood(&flood, case));
            }
        }
    }

    /// posh#225 regression: a newline flood into a session whose ring is
    /// already full must not push a healthy lossy client toward
    /// `MAX_CLIENT_BACKLOG`. For a viewport of the session's own size, the
    /// visible frame must stay screen-sized — it may not carry the scrollback
    /// ring — whatever the ack cadence. (A wider viewport still gets
    /// ring-sized frames: `a_wider_client_still_gets_the_full_dump`.)
    ///
    /// Scope: this pins the VISIBLE frame. With acks withheld entirely the
    /// v1 scrollback frame still re-carries every un-acked row (the second
    /// amplifier); that case is bounded by the v2 send cursor, not here.
    ///
    /// Nor may it pass by STARVING scrollback: a visible frame is trivially
    /// small, and the backlog trivially flat, if history simply stops being
    /// sent. The healthy cadences therefore also assert that every row the
    /// flood scrolled off was shipped and acked ([`assert_history_flowed`]).
    #[test]
    fn posh225_full_ring_flood_keeps_visible_frames_screen_sized() {
        const KIB: usize = 1024;
        let flood = newline_flood(256 * KIB);
        for acks in [
            FloodAcks::EveryNewest(1),
            FloodAcks::Lagged(2),
            FloodAcks::Lagged(5),
            FloodAcks::Never,
        ] {
            let case = FloodCase {
                chunk: 4 * KIB,
                acks,
                drain: FloodDrain::OneWritePerChunk,
                prefill_rows: SCROLLBACK + 200,
                pace: None,
                v2: false,
                extent: false,
            };
            // `Never` is quadratic — every chunk's scrollback frame re-carries
            // every un-acked row — and its only assertion is the visible-frame
            // bound, which half the flood shows as well as all of it.
            let offered = match acks {
                FloodAcks::Never => &flood[..128 * KIB],
                _ => &flood[..],
            };
            let r = measure_flood(offered, case);
            // Checked FIRST and unconditionally for every cadence: it is what
            // the test is named for, and everything below presumes it.
            assert!(
                r.largest_visible < 64 * KIB,
                "acks={}: a visible frame was {} bytes — it is carrying the scrollback ring",
                acks.label(),
                r.largest_visible,
            );
            // `Never` stops here. No delivery assertion, and none on the
            // backlog: with acks withheld the v1 scrollback frame re-carries
            // every un-acked row on every chunk (the second amplifier, out of
            // scope above), and nothing is ever confirmed. Only the visible
            // frame is pinned.
            if matches!(acks, FloodAcks::Never) {
                continue;
            }
            // Everything below depends on what one write moves on this host.
            if !flood_socket_supports_bounds(case, &r) {
                continue;
            }
            match acks {
                // A reader that keeps up, and one that lags inside the
                // producer's outstanding window: the backlog stays flat AND
                // history flows.
                FloodAcks::EveryNewest(1) | FloodAcks::Lagged(2) => {
                    assert_backlog_bounded(case, &r);
                    assert_history_flowed(case, &r);
                }
                // The lost-base regime (see `FloodAcks::Lagged`): the backlog
                // must still stay flat, but there is no delivery assertion.
                // Every ack that lands on an evicted frame returns `None`
                // without advancing `acked_sb_total` and suppresses the next
                // scrollback frame, so how much history flows depends on how
                // often an ack happens to land on a held frame — the v1
                // scrollback protocol makes no promise here, and the v2 send
                // cursor is what bounds it.
                FloodAcks::Lagged(5) => assert_backlog_bounded(case, &r),
                other => unreachable!("no assertions defined for acks={}", other.label()),
            }
        }
    }

    /// posh#225 harness self-check: the SAME helpers
    /// [`posh225_full_ring_flood_keeps_visible_frames_screen_sized`] uses, run
    /// against a short flood into an EMPTY ring. It guards the empty-ring path
    /// — a client attached to a young session keeps screen-sized frames and
    /// its history flows — and the harness's delivery-gated ack model and the
    /// assertions' slack, independently of the full-ring fixture.
    #[test]
    fn posh225_flood_harness_delivery_assertions_hold_while_frames_are_small() {
        const KIB: usize = 1024;
        let flood = newline_flood(64 * KIB);
        print_flood_header();
        for acks in [FloodAcks::EveryNewest(1), FloodAcks::Lagged(2), FloodAcks::Lagged(5)] {
            let case = FloodCase {
                chunk: 4 * KIB,
                acks,
                drain: FloodDrain::OneWritePerChunk,
                prefill_rows: 0,
                pace: None,
                v2: false,
                extent: false,
            };
            let r = measure_flood(&flood, case);
            print_flood_row(case, &r);
            assert!(r.rows_scrolled > 0, "the flood must scroll rows off for this to mean anything");
            // Every cadence, the lost-base `Full`s included: a client of the
            // session's own size gets screen-sized frames whatever its acks.
            assert!(
                r.largest_visible < 64 * KIB,
                "acks={}: a visible frame was {} bytes — this scenario no longer keeps frames small",
                acks.label(),
                r.largest_visible,
            );
            if !flood_socket_supports_bounds(case, &r) {
                continue;
            }
            assert_backlog_bounded(case, &r);
            // `Lagged(5)` is the lost-base regime: no delivery contract.
            if !matches!(acks, FloodAcks::Lagged(5)) {
                assert_history_flowed(case, &r);
            }
        }
    }

    /// The least one write into the empty pinned socket must move for the
    /// backlog and delivery bounds to mean anything: a whole chunk's frames,
    /// as the regression test bounds them (a 64 KiB visible frame plus a
    /// 32 KiB scrollback frame).
    const FLOOD_WRITE_CAPACITY_NEEDED: usize = 96 * 1024;

    /// Whether this host's socketpair gives [`assert_backlog_bounded`] and
    /// [`assert_history_flowed`] their footing. A caller that gets `false`
    /// must SKIP them — they would fail for the host's reason, not posh's.
    ///
    /// macOS gap (posh#214): the backlog/delivery bounds assume one write
    /// moves a whole chunk's frames (>= 96 KiB into an empty socketpair). A
    /// host whose pinned buffer grants less — macOS `AF_UNIX`, or a Linux
    /// with a low `net.core.wmem_max`, which is why this is a runtime check
    /// and not a `cfg` — skips them; macOS would need a measured capacity, or
    /// a drain loop instead of one write per chunk.
    fn flood_socket_supports_bounds(case: FloodCase, r: &FloodRun) -> bool {
        let supported = r.socket_write_capacity >= FLOOD_WRITE_CAPACITY_NEEDED;
        if !supported {
            eprintln!(
                "acks={}: skipping backlog/delivery bounds: socket write capacity {} < {} bytes",
                case.acks.label(),
                r.socket_write_capacity,
                FLOOD_WRITE_CAPACITY_NEEDED,
            );
        }
        supported
    }

    /// A draining client's backlog stayed far below `MAX_CLIENT_BACKLOG`.
    /// Presumes [`flood_socket_supports_bounds`]: how much backlog one write
    /// per chunk leaves behind is a function of what one write moves.
    fn assert_backlog_bounded(case: FloodCase, r: &FloodRun) {
        let acks = case.acks.label();
        assert_eq!(
            r.crossed_backlog_at, None,
            "acks={acks}: a draining client crossed MAX_CLIENT_BACKLOG after {} bytes",
            r.fed,
        );
        assert!(
            r.peak_write_buf < 1024 * 1024,
            "acks={acks}: backlog peaked at {} bytes against an ideal reader",
            r.peak_write_buf,
        );
    }

    /// History actually reached the reader: every row the flood scrolled off
    /// was shipped, and acked up to the cadence's own lag. For a
    /// [`FloodDrain::OneWritePerChunk`] run under `EveryNewest(1)` or a
    /// `Lagged(k)` with `k <= 4`.
    ///
    /// Two preconditions are the CALLER's to establish first, and both
    /// callers do: it has asserted `largest_visible < 64 KiB`, and it has
    /// checked [`flood_socket_supports_bounds`] (skipping this otherwise).
    ///
    /// The argument, by induction over chunks. Suppose every earlier chunk's
    /// frames were delivered in the iteration that queued them. Then every
    /// earlier ack was on schedule and landed on a held frame (`current`
    /// itself, or at most 8 frames behind it), so the base is held and the
    /// scrollback floor trails by at most `k` chunks. This chunk therefore
    /// queues one visible frame (< 64 KiB, the caller's) and one scrollback
    /// frame carrying at most `k + 1` chunks of rows (< 32 KiB, asserted
    /// here) — a burst within the socket's per-write capacity (>= 96 KiB,
    /// the caller's), which the one write moves whole. So this chunk's
    /// frames are delivered in its own iteration too, and its ack is on
    /// schedule.
    fn assert_history_flowed(case: FloodCase, r: &FloodRun) {
        const KIB: usize = 1024;
        let acks = case.acks.label();
        let lag_chunks = match case.acks {
            FloodAcks::EveryNewest(1) => 0,
            FloodAcks::Lagged(k) if k <= 4 => k as u64,
            _ => panic!("acks={acks}: no delivery contract for this cadence"),
        };
        debug_assert!(
            r.socket_write_capacity >= FLOOD_WRITE_CAPACITY_NEEDED,
            "the caller must check flood_socket_supports_bounds before asserting delivery",
        );
        assert!(
            r.largest_scrollback < 32 * KIB,
            "acks={acks}: a scrollback frame was {} bytes ({} rows) — more than the {} chunks \
             of rows this cadence can leave un-acked",
            r.largest_scrollback,
            r.largest_scrollback_rows,
            lag_chunks + 1,
        );
        assert_eq!(
            r.peak_undrained, 0,
            "acks={acks}: one write (capacity {} bytes) left bytes undelivered to an ideal reader",
            r.socket_write_capacity,
        );
        // The last chunk queued a scrollback frame (the base was held), and
        // it carried every row above the floor; every row at or below the
        // floor was carried by the scrollback frame whose ack put the floor
        // there. So each scrolled row was shipped at least once.
        assert!(
            r.rows_shipped >= r.rows_scrolled,
            "acks={acks}: {} rows scrolled off but only {} were shipped — scrollback is starved",
            r.rows_scrolled,
            r.rows_shipped,
        );
        // The drain step precedes the ack within a chunk, so the final
        // chunk's frames ARE delivered and ackable when the loop ends: under
        // `EveryNewest(1)` the last ack is on the last scrollback frame and
        // there is no slack at all. Under `Lagged(k)` the last ack names the
        // frame from `k` chunks back, leaving exactly the rows those `k`
        // chunks scrolled — at most `flood_rows_per_chunk` each — un-acked.
        let slack = lag_chunks * flood_rows_per_chunk(case.chunk);
        let unacked = r.rows_scrolled.saturating_sub(r.rows_acked);
        assert!(
            unacked <= slack,
            "acks={acks}: {unacked} of {} scrolled rows were never acked (this cadence allows {slack})",
            r.rows_scrolled,
        );
    }

    // ---- posh#225 Stage 2: paced delivery ----

    /// A lossy client shaped like an M2 bridge's daemon Init with the
    /// viewport's CAP_PACED, plus any `extra` content caps.
    fn paced_conn(rows: u16, cols: u16, extra: &[caps::Cap]) -> (ClientConn, UnixStream) {
        let mut table = vec![caps::encode_paced()];
        table.extend_from_slice(extra);
        lossy_conn(rows, cols, &table)
    }

    #[test]
    fn backlog_log_fields_say_whether_the_client_is_paced() {
        let (paced, _p) = paced_conn(24, 80, &[]);
        let (plain, _q) = lossy_conn(24, 80, &[]);
        let now = util::now_ms();
        for (c, want) in [(&paced, "paced=1"), (&plain, "paced=0")] {
            let fields = backlog_log_fields(c, now);
            assert!(fields.contains(&format!(" {want} ")), "{fields}");
            for key in ["fd=", "backlog=", "drained_total=", "last_drain_age_ms="] {
                assert!(fields.contains(key), "{fields} lacks {key}");
            }
        }
    }

    #[test]
    fn paced_cap_on_init_makes_a_paced_client_and_a_bare_reinit_keeps_it() {
        let (mut c, _peer) = paced_conn(24, 80, &[]);
        assert_eq!(c.pacing, Some(Pacing::default()));
        assert!(c.is_paced());
        c.pacing.as_mut().unwrap().dirty = true;
        c.apply_init(&ipc::encode_resize(30, 100));
        assert_eq!(
            c.pacing,
            Some(Pacing {
                dirty: true,
                last_fresh: None,
                ..Pacing::default()
            }),
            "a bare re-Init keeps the paced state"
        );

        let (plain, _p) = lossy_conn(24, 80, &[]);
        assert_eq!(plain.pacing, None, "no CAP_PACED: not paced");
        assert!(!plain.is_paced());

        let (empty, _p) = lossy_conn(
            24,
            80,
            &[caps::Cap {
                id: caps::CAP_PACED,
                payload: vec![],
            }],
        );
        assert_eq!(empty.pacing, None, "a malformed CAP_PACED entry is ignored");
    }

    #[test]
    fn broadcast_output_only_marks_a_paced_client_dirty() {
        let (rows, cols) = (5u16, 24u16);
        let mut term = Terminal::with_scrollback(rows, cols, 100);
        let (mut c, _peer) = paced_conn(rows, cols, &[]);
        let shaped = c.visible_shaped_for;
        term.process(b"a line\r\n");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        assert!(c.write_buf.is_empty(), "no frame and no Tag::Output is queued");
        assert!(c.owes_paced_frame());
        assert_eq!(c.visible_shaped_for, shaped, "no dump was built for it");
        assert_eq!(c.producer.as_ref().unwrap().current_num(), 0, "no frame was produced");
    }

    #[test]
    fn paced_send_at_waits_for_an_empty_buffer_then_an_ack_or_the_wait() {
        let (rows, cols) = (5u16, 24u16);
        let mut term = Terminal::with_scrollback(rows, cols, 100);
        let (mut c, _peer) = paced_conn(rows, cols, &[]);
        assert_eq!(c.paced_send_at(), None, "owes nothing");
        term.process(b"one");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        assert_eq!(c.paced_send_at(), Some(0), "never sent: due at once");

        send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), 100);
        assert_eq!(c.paced_send_at(), None, "sent: owes nothing");

        term.process(b" two");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        c.write_buf.clear();
        assert_eq!(c.paced_send_at(), Some(100 + PACED_ACK_WAIT_MS), "unacked: waits for the ack");

        let sent = c.producer.as_ref().unwrap().last_visible_num();
        c.apply_frame_ack(&ipc::encode_frame_ack(sent, 0));
        assert_eq!(c.paced_send_at(), Some(100 + PACED_FRAME_FLOOR_MS), "acked: the floor");

        c.write_buf.push(0);
        assert_eq!(c.paced_send_at(), None, "bytes still queued: POLLOUT wakes the loop");
    }

    #[test]
    fn paced_poll_timeout_is_the_nearest_deadline_and_never_negative() {
        let now = 100;
        assert_eq!(paced_poll_timeout(&[], None, now), -1, "no clients");
        let (mut plain, _p0) = frame_capable_conn(5, 24);
        assert_eq!(paced_poll_timeout(std::slice::from_ref(&plain), None, now), -1, "no paced client");
        let (clean, _p1) = paced_conn(5, 24, &[]);
        assert_eq!(paced_poll_timeout(std::slice::from_ref(&clean), None, now), -1, "a clean paced client");

        // Each produces a real frame 1; its send time is then set directly
        // (the clock is the test's). `a` acked it — due at the floor; `b`
        // did not — due at the ack wait.
        let term = Terminal::with_scrollback(5, 24, 0);
        let (mut a, _p2) = paced_conn(5, 24, &[]);
        assert!(a.build_frame_from(&term));
        a.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        a.write_buf.clear();
        a.pacing = Some(Pacing {
            dirty: true,
            last_fresh: Some(100),
            ..Pacing::default()
        });
        let (mut b, _p3) = paced_conn(5, 24, &[]);
        assert!(b.build_frame_from(&term));
        b.write_buf.clear();
        b.pacing = Some(Pacing {
            dirty: true,
            last_fresh: Some(100),
            ..Pacing::default()
        });
        let mut clients = vec![a, b];
        assert_eq!(paced_poll_timeout(&clients, None, now), 20);

        plain.write_buf.resize(1024, 0);
        clients.push(plain);
        assert_eq!(paced_poll_timeout(&clients, None, now), 20, "a non-paced backlog changes nothing");

        clients[0].pacing = Some(Pacing {
            dirty: true,
            last_fresh: Some(50),
            ..Pacing::default()
        });
        assert_eq!(paced_poll_timeout(&clients, None, now), 0, "overdue: never negative");
    }

    #[test]
    fn send_paced_frames_builds_one_frame_from_the_terminal_as_it_is_then() {
        let (rows, cols) = (5u16, 24u16);
        let mut term = Terminal::with_scrollback(rows, cols, 100);
        let (mut c, _peer) = paced_conn(rows, cols, &[]);
        term.process(b"first\r\n");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        // Three more screens, each marked dirty by its own broadcast: the
        // marks collapse into the one frame the pass builds.
        for line in ["second\r\n", "third\r\n", "fourth"] {
            term.process(line.as_bytes());
            broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
            assert!(c.write_buf.is_empty(), "a broadcast queues nothing for it");
        }
        send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), 40);
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(frames.len(), 1, "one frame however many screens went by");
        assert!(matches!(frames[0].body, FrameBody::Full(_)), "got {:?}", frames[0].body);
        let num = frames[0].frame_num;
        c.apply_frame_ack(&ipc::encode_frame_ack(num, 0));
        assert_eq!(
            c.producer.as_ref().unwrap().acked_dump(),
            Some(&term.dump_vt_mirror(rows, cols)[..]),
            "the latest screen, not an intermediate one"
        );
        assert_eq!(c.pacing.as_ref().map(|p| (p.dirty, p.last_fresh)), Some((false, Some(40))));
        assert_eq!(c.producer.as_ref().unwrap().last_visible_num(), num, "the producer names it");
        c.write_buf.clear();
        send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), 40);
        assert!(c.write_buf.is_empty(), "a clean client is sent nothing");
    }

    #[test]
    fn send_paced_frames_carries_scrollback_right_behind_the_visible_frame() {
        let (rows, cols) = (5u16, 24u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        let (mut c, _peer) = paced_conn(
            rows,
            cols,
            &[caps::Cap {
                id: caps::CAP_SCROLLBACK,
                payload: vec![0],
            }],
        );
        // The attach keyframe (frame 1), acked through the bridge.
        assert!(c.build_frame_from(&term));
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        c.write_buf.clear();

        let before = term.primary_scrollback_total();
        scroll_off(&mut term, 14);
        let scrolled = term.primary_scrollback_total() - before;
        assert_eq!(scrolled, 10);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        assert!(c.write_buf.is_empty());
        send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), 0);

        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(frames.len(), 2, "the visible frame, then its scrollback");
        assert!(
            matches!(
                frames[0].body,
                FrameBody::Full(_) | FrameBody::Diff { .. } | FrameBody::Morph { .. }
            ),
            "a visible body first, got {:?}",
            frames[0].body
        );
        match &frames[1].body {
            FrameBody::Scrollback { base, rows } => {
                assert_eq!(*base, frames[0].frame_num, "threads off the visible frame (posh#181)");
                assert_eq!(rows.len() as u64, scrolled);
            }
            other => panic!("expected a scrollback frame, got {other:?}"),
        }
    }

    #[test]
    fn flush_paced_frames_sends_a_dirty_clients_last_screen() {
        let (rows, cols) = (5u16, 24u16);
        let mut term = Terminal::with_scrollback(rows, cols, 100);
        let (mut owing, _p0) = paced_conn(rows, cols, &[]);
        let (clean, _p1) = paced_conn(rows, cols, &[]);
        let (plain, _p2) = frame_capable_conn(rows, cols);
        term.process(b"one");
        broadcast_output(std::slice::from_mut(&mut owing), &term, b"x");
        send_paced_frames(std::slice::from_mut(&mut owing), &term, Some(&term), 100);
        owing.write_buf.clear();
        term.process(b" last");
        broadcast_output(std::slice::from_mut(&mut owing), &term, b"x");
        assert!(owing.paced_send_at().is_some_and(|at| at > 101), "not due yet");

        let mut clients = vec![owing, clean, plain];
        flush_paced_frames(&mut clients, &term, 101);
        assert_eq!(decode_server_frames(&clients[0].write_buf).len(), 1, "the owed screen");
        assert!(clients[1].write_buf.is_empty(), "a clean paced client owes nothing");
        assert!(clients[2].write_buf.is_empty(), "a non-paced client is not flushed");
    }

    /// CAP_PACED without CAP_PROTOCOL_VERSION: the pacing state is recorded,
    /// but with no producer the client is not paced and keeps raw output.
    #[test]
    fn paced_without_protocol_version_is_unpaced_and_still_gets_raw_output() {
        let mut c = test_client_conn();
        let mut init = ipc::encode_resize(5, 24).to_vec();
        init.extend_from_slice(&caps::encode_table(&[caps::encode_paced()]));
        c.apply_init(&init);
        c.maybe_enable_frames();
        assert!(c.pacing.is_some() && c.producer.is_none());
        assert!(!c.is_paced());

        let mut term = Terminal::with_scrollback(5, 24, 0);
        term.process(b"hi");
        broadcast_output(std::slice::from_mut(&mut c), &term, b"hi");
        let mut fb = FrameBuffer::new();
        fb.feed(&c.write_buf);
        let frame = fb.next().unwrap().expect("one record");
        assert_eq!((frame.tag, frame.payload.as_slice()), (Tag::Output, &b"hi"[..]));
        assert_eq!(c.paced_send_at(), None);
    }

    /// A paced client's first fresh frame, sent at `now` and acked, with its
    /// queued bytes cleared: the state every event-site test below starts from.
    fn paced_conn_with_an_acked_frame(
        term: &Terminal,
        extra: &[caps::Cap],
        now: u64,
    ) -> (ClientConn, UnixStream) {
        let (mut c, peer) = paced_conn(term.rows(), term.cols(), extra);
        broadcast_output(std::slice::from_mut(&mut c), term, b"x");
        send_paced_frames(std::slice::from_mut(&mut c), term, Some(term), now);
        assert!(!c.write_buf.is_empty(), "the setup frame was sent");
        let sent = c.producer.as_ref().unwrap().last_visible_num();
        c.apply_frame_ack(&ipc::encode_frame_ack(sent, 0));
        c.write_buf.clear();
        (c, peer)
    }

    /// Runs the paced send pass at this client's send opportunity and
    /// returns the frames it queued.
    fn send_at_the_opportunity(c: &mut ClientConn, term: &Terminal) -> Vec<ServerFrame> {
        let at = c.paced_send_at().expect("a frame is owed and due");
        send_paced_frames(std::slice::from_mut(c), term, Some(term), at);
        decode_server_frames(&c.write_buf)
    }

    #[test]
    fn a_paced_attach_replay_is_built_by_the_send_pass() {
        let mut term = Terminal::with_scrollback(5, 24, 100);
        term.process(b"hello");
        let (mut c, _peer) = paced_conn(5, 24, &[]);
        queue_replay(&mut c, &term);
        assert!(c.write_buf.is_empty(), "the replay queues nothing at once");
        assert!(c.owes_paced_frame());
        send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), 0);
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(frames.len(), 1);
        assert!(matches!(frames[0].body, FrameBody::Full(_)), "got {:?}", frames[0].body);
    }

    #[test]
    fn a_paced_resync_releases_the_ack_wait() {
        let mut term = Terminal::with_scrollback(5, 24, 100);
        term.process(b"hello");
        let (mut c, _peer) = paced_conn(5, 24, &[]);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), 100);
        let sent = c.producer.as_ref().unwrap().last_visible_num();
        c.write_buf.clear();

        handle_frame_ack(&mut c, &ipc::encode_frame_ack(sent, ipc::FRAME_ACK_RESYNC), &term, 100);
        assert!(c.write_buf.is_empty(), "the recovering frame waits for the pass");
        assert_eq!(c.paced_send_at(), Some(0), "the RESYNC released the ack wait");
        send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), 101);
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(frames.len(), 1);
        assert!(matches!(frames[0].body, FrameBody::Full(_)), "got {:?}", frames[0].body);
    }

    #[test]
    fn a_paced_regeometry_frame_is_the_next_paced_frame() {
        let mut term = Terminal::with_scrollback(24, 80, 100);
        term.process(b"hello\r\n$ ");
        let (mut c, _peer) = paced_conn_with_an_acked_frame(&term, &[], 0);
        assert!(c.apply_resize(&ipc::encode_resize(30, 80)));
        assert!(c.prepare_regeometry_frame());
        queue_replay(&mut c, &term);
        assert!(c.write_buf.is_empty(), "the regeometry frame waits for the pass");
        assert!(c.owes_paced_frame());
        assert!(c.paced_send_at().is_some_and(|at| at > 0), "it keeps the last fresh frame (no release)");

        let frames = send_at_the_opportunity(&mut c, &term);
        assert_eq!(frames.len(), 1);
        assert_eq!(c.visible_shaped_for, Some((30, 80)));
        assert!(!c.owes_regeometry_frame(), "the paced frame ended the debt");
    }

    #[test]
    fn a_paced_morph_regeometry_frame_is_a_full() {
        let mut term = Terminal::with_scrollback(24, 80, 100);
        term.process(b"hello\r\n$ ");
        let morph = [caps::Cap {
            id: caps::CAP_MORPH,
            payload: vec![],
        }];
        let (mut c, _peer) = paced_conn_with_an_acked_frame(&term, &morph, 0);
        assert!(c.uses_morph());
        assert!(c.apply_resize(&ipc::encode_resize(30, 80)));
        assert!(c.prepare_regeometry_frame());
        queue_replay(&mut c, &term);
        assert!(c.write_buf.is_empty());

        let frames = send_at_the_opportunity(&mut c, &term);
        assert_eq!(frames.len(), 1);
        assert!(matches!(frames[0].body, FrameBody::Full(_)), "got {:?}", frames[0].body);
        assert_eq!(c.regeometry_keyframe, Some(RegeometryKeyframe::Sent(frames[0].frame_num)));
    }

    #[test]
    fn a_paced_activity_answer_rides_the_next_paced_frame() {
        let label = |process: &str| caps::SessionActivity {
            process: process.into(),
            title: String::new(),
        };
        let mut term = Terminal::with_scrollback(24, 80, 100);
        term.process(b"hello");
        let (mut c, _peer) = paced_conn_with_an_acked_frame(&term, &[], 100);
        c.absorb_client_caps(
            &[caps::Cap {
                id: caps::CAP_SESSION_ACTIVITY,
                payload: vec![],
            }],
            0,
            false,
        );
        c.activity_now = Some(label("vim"));
        queue_due_answers(std::slice::from_mut(&mut c), &term);
        assert!(c.write_buf.is_empty(), "the answer waits for the next paced frame");
        assert!(c.answer_due(), "the answer was not consumed early");
        assert_eq!(c.activity_sent, None);
        c.activity_now = Some(label("less"));
        queue_due_answers(std::slice::from_mut(&mut c), &term);
        assert!(c.write_buf.is_empty());

        let frames = send_at_the_opportunity(&mut c, &term);
        assert_eq!(frames.len(), 1, "one frame for both changes");
        let got = caps::find(&frames[0].caps, caps::CAP_SESSION_ACTIVITY).expect("the answer rides it");
        assert_eq!(caps::decode_session_activity(&got.payload).unwrap(), label("less"));
    }

    #[test]
    fn a_source_swap_marks_a_paced_client_dirty_and_its_next_frame_is_full() {
        let mut session = Terminal::with_scrollback(24, 80, 100);
        session.process(b"hello");
        let (mut c, _peer) = paced_conn_with_an_acked_frame(&session, &[], 0);
        let mut overlay = Terminal::new(24, 80);
        overlay.process(b"overlay$ ");
        broadcast_source_swap(std::slice::from_mut(&mut c), &overlay, &overlay.dump_vt_flat());
        assert!(c.write_buf.is_empty(), "the swap queues nothing for a paced client");
        assert!(c.owes_paced_frame());

        // The overlay is up: the pass gets no history terminal.
        let at = c.paced_send_at().expect("a frame is owed and due");
        send_paced_frames(std::slice::from_mut(&mut c), &overlay, None, at);
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(frames.len(), 1);
        assert!(matches!(frames[0].body, FrameBody::Full(_)), "got {:?}", frames[0].body);
    }

    const SCROLLBACK_CAP: [caps::Cap; 1] = [caps::Cap {
        id: caps::CAP_SCROLLBACK,
        payload: Vec::new(),
    }];

    fn scrollback_rows_in(frames: &[ServerFrame]) -> Option<usize> {
        frames.iter().find_map(|f| match &f.body {
            FrameBody::Scrollback { rows, .. } => Some(rows.len()),
            _ => None,
        })
    }

    /// A paced client holds every row scrolled since its last paced pair. An
    /// event that runs the loop's resize path without changing the session
    /// width (another client attaching or leaving, a height-only resize, a
    /// mux reconnect) must not move its floor past those rows: the viewport
    /// keeps its ring, so they would be a silent hole in its history.
    #[test]
    fn a_same_width_attach_does_not_skip_a_paced_clients_unshipped_history() {
        let mut term = Terminal::with_scrollback(5, 24, 1000);
        let (mut c, _peer) = paced_conn_with_an_acked_frame(&term, &SCROLLBACK_CAP, 0);
        let before = term.primary_scrollback_total();
        scroll_off(&mut term, 14);
        let scrolled = term.primary_scrollback_total() - before;
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        assert!(c.write_buf.is_empty(), "the rows wait for the paced pair");

        let cols = term.cols();
        reset_scrollback_floors_on_reflow(std::slice::from_mut(&mut c), &term, cols);
        let frames = send_at_the_opportunity(&mut c, &term);
        assert_eq!(scrollback_rows_in(&frames), Some(scrolled as usize), "every unshipped row ships");
    }

    /// RFC 0002 §4: a width change still restarts every framed client's
    /// counting at the reflowed total, paced or not.
    #[test]
    fn a_width_change_still_resets_every_framed_clients_scrollback_floor() {
        let mut term = Terminal::with_scrollback(5, 24, 1000);
        let (paced, _p0) = paced_conn_with_an_acked_frame(&term, &SCROLLBACK_CAP, 0);
        let (plain, _p1) = scrollback_capable_conn(5, 24);
        let mut clients = vec![paced, plain];
        scroll_off(&mut term, 14);
        broadcast_output(&mut clients, &term, b"x");
        term.resize(5, 30);
        reset_scrollback_floors_on_reflow(&mut clients, &term, 24);
        let total = term.primary_scrollback_total();
        assert!(clients.iter().all(|c| c.sb_floor == total));
        clients[0].write_buf.clear();
        let frames = send_at_the_opportunity(&mut clients[0], &term);
        assert_eq!(scrollback_rows_in(&frames), None, "no pre-reflow rows ship");
    }

    /// A non-paced client ships its rows in the broadcast that scrolled them,
    /// so a same-width resize path leaves it exactly where it was.
    #[test]
    fn a_same_width_attach_leaves_a_non_paced_clients_scrollback_unchanged() {
        let mut term = Terminal::with_scrollback(5, 24, 1000);
        let (mut c, _peer) = scrollback_capable_conn(5, 24);
        assert!(c.request_frame_from(&term));
        scroll_off(&mut term, 14);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        let shipped = c.acked_sb_total;
        assert_eq!(shipped, term.primary_scrollback_total(), "shipped with the broadcast");

        let cols = term.cols();
        reset_scrollback_floors_on_reflow(std::slice::from_mut(&mut c), &term, cols);
        c.write_buf.clear();
        scroll_off(&mut term, 3);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(scrollback_rows_in(&frames), Some(3), "only the new rows");
    }

    // ---- posh#225 Stage 2: the flood, paced ----

    /// The ack cadences the paced flood tests sweep, at `pace: Some(1)` so a
    /// `Lagged(k)` is a `k` ms round trip ([`FloodAcks::Lagged`]): a prompt
    /// reader, an RTT between the frame floor and the ack wait (its acks
    /// land before the wait expires, so frames go out an RTT apart), one past
    /// the ack wait (every frame goes out on the wait, unacked), and a reader
    /// that never acks.
    const PACED_FLOOD_CADENCES: [FloodAcks; 4] = [
        FloodAcks::Never,
        FloodAcks::EveryNewest(1),
        FloodAcks::Lagged(50),
        FloodAcks::Lagged(300),
    ];

    /// A paced, socket-drained flood case at the production chunk size.
    fn paced_flood_case(acks: FloodAcks, prefill_rows: usize) -> FloodCase {
        FloodCase {
            chunk: 4 * 1024,
            acks,
            drain: FloodDrain::OneWritePerChunk,
            prefill_rows,
            pace: Some(1),
            v2: false,
            extent: false,
        }
    }

    /// [`paced_flood_case`] for a v2 viewport (posh#225 Stage 3).
    fn paced_v2_flood_case(acks: FloodAcks, prefill_rows: usize) -> FloodCase {
        FloodCase {
            v2: true,
            ..paced_flood_case(acks, prefill_rows)
        }
    }

    /// [`paced_v2_flood_case`] whose viewport asks for the v2 extent
    /// (posh#225 Stage 4).
    fn paced_v2x_flood_case(acks: FloodAcks, prefill_rows: usize) -> FloodCase {
        FloodCase {
            extent: true,
            ..paced_v2_flood_case(acks, prefill_rows)
        }
    }

    /// With no ack after attach and a buffer that always drains (isolating
    /// pacing from the socket), a paced client is sent one visible frame per
    /// ack wait however many screens the flood produces.
    ///
    /// The count, from the harness's clock: the attach keyframe is sent and
    /// acked at t = 0, so the first opportunity is the FLOOR after it; that
    /// frame is never acked, so each later one waits a whole ack wait. The
    /// 512 chunks are fed at t = 0, pace, …, pace * 511 (the send pass right
    /// after each), giving frames at FLOOR, FLOOR + WAIT, … up to the last
    /// chunk's t: `1 + (pace * 511 - FLOOR) / WAIT` (20 and 270 today).
    #[test]
    fn posh225_paced_flood_without_acks_sends_one_visible_frame_per_ack_wait() {
        const KIB: usize = 1024;
        const PACE_MS: u64 = 1;
        let flood = newline_flood(2 * KIB * KIB);
        let case = FloodCase {
            chunk: 4 * KIB,
            acks: FloodAcks::Never,
            drain: FloodDrain::Always,
            prefill_rows: 0,
            pace: Some(PACE_MS),
            v2: false,
            extent: false,
        };
        let r = measure_flood(&flood, case);
        assert_eq!(r.chunks, 512);
        let flood_end = PACE_MS * (r.chunks as u64 - 1);
        let expected = 1 + (flood_end - PACED_FRAME_FLOOR_MS) / PACED_ACK_WAIT_MS;
        assert_eq!(r.visible_frames_during_flood as u64, expected);
        assert_eq!(r.last_screen_delivered, Some(true), "the idle tail delivered the last screen");
    }

    /// A prompt reader on a real socket: at most one visible frame is ever
    /// queued, at most one per frame floor is built, and each stays
    /// screen-sized though the ring is full.
    #[test]
    fn posh225_paced_flood_with_prompt_acks_never_queues_two_visible_frames() {
        const KIB: usize = 1024;
        let flood = newline_flood(256 * KIB);
        let case = paced_flood_case(FloodAcks::EveryNewest(1), SCROLLBACK + 200);
        let r = measure_flood(&flood, case);
        print_flood_header();
        print_flood_row(case, &r);
        assert!(r.max_visible_queued <= 1, "{} visible frames were queued at once", r.max_visible_queued);
        let bound = r.chunks / PACED_FRAME_FLOOR_MS as usize + 2;
        assert!(
            r.visible_frames <= bound,
            "{} visible frames for {} chunks (at most {bound} at one per {PACED_FRAME_FLOOR_MS} ms)",
            r.visible_frames,
            r.chunks,
        );
        assert!(r.largest_visible < 64 * KIB, "a visible frame was {} bytes", r.largest_visible);
    }

    /// Whatever the ack cadence, a paced client's backlog is at most one
    /// visible frame and its scrollback frame. Against this ideal reader that
    /// holds by timing alone (every burst leaves in the write after it); the
    /// empty-buffer gate that makes it hold for a slow reader is pinned by
    /// [`posh225_paced_flood_with_a_slow_reader_queues_one_frame_pair`].
    /// (The v1 scrollback frame is not capped here — with acks withheld it
    /// re-carries every un-acked row; posh#225 Task 3.3 bounds it.)
    #[test]
    fn posh225_paced_flood_backlog_is_one_frame_pair_for_every_cadence() {
        const KIB: usize = 1024;
        let flood = newline_flood(256 * KIB);
        print_flood_header();
        for prefill_rows in [0, SCROLLBACK + 200] {
            for acks in PACED_FLOOD_CADENCES {
                let case = paced_flood_case(acks, prefill_rows);
                let r = measure_flood(&flood, case);
                print_flood_row(case, &r);
                let what = format!("acks={} prefill={prefill_rows}", acks.label());
                assert_eq!(r.crossed_backlog_at, None, "{what}: crossed MAX_CLIENT_BACKLOG");
                assert!(r.max_visible_queued <= 1, "{what}: {} visible frames queued at once", r.max_visible_queued);
                assert!(
                    r.peak_write_buf <= r.largest_visible + r.largest_scrollback,
                    "{what}: backlog peaked at {} bytes, more than one frame pair ({} + {})",
                    r.peak_write_buf,
                    r.largest_visible,
                    r.largest_scrollback,
                );
                assert!(r.largest_visible < 64 * KIB, "{what}: a visible frame was {} bytes", r.largest_visible);
            }
        }
    }

    /// No stale screen at quiescence: whatever the cadence, once the flood
    /// stops the paced client is sent the terminal's final screen.
    #[test]
    fn posh225_paced_flood_ends_on_the_last_screen() {
        const KIB: usize = 1024;
        let flood = newline_flood(256 * KIB);
        for prefill_rows in [0, SCROLLBACK + 200] {
            for acks in PACED_FLOOD_CADENCES {
                let r = measure_flood(&flood, paced_flood_case(acks, prefill_rows));
                assert_eq!(
                    r.last_screen_delivered,
                    Some(true),
                    "acks={} prefill={prefill_rows}: the client was left on a stale screen",
                    acks.label(),
                );
            }
        }
    }

    /// Pacing must not starve history: with a prompt reader every row the
    /// flood scrolled off is acked once the tail goes quiet.
    #[test]
    fn posh225_paced_flood_delivers_history_with_prompt_acks() {
        const KIB: usize = 1024;
        let flood = newline_flood(256 * KIB);
        let case = paced_flood_case(FloodAcks::EveryNewest(1), SCROLLBACK + 200);
        let r = measure_flood(&flood, case);
        if !flood_socket_supports_bounds(case, &r) {
            return;
        }
        assert!(r.rows_scrolled > 0, "the flood must scroll rows off for this to mean anything");
        assert_eq!(r.rows_acked, r.rows_scrolled, "every scrolled-off row reached the reader and was acked");
    }

    /// A SLOW reader (1 KiB per 1 ms step, against frame pairs of ~5 KB to
    /// ~1 MB): frames outlive the step that queued them, so later send
    /// passes find bytes still queued. The empty-buffer gate in
    /// `paced_send_at` must hold them off — at most one visible frame and
    /// one frame pair is ever queued — and the tail must still end on the
    /// last screen. Verified by hand: deleting that gate
    /// (`if !self.write_buf.is_empty() { return None; }`) fails THIS test on
    /// `max_visible_queued`, while every other `posh225_paced_*` test, whose
    /// ideal reader empties each burst in one write, still passes.
    #[test]
    fn posh225_paced_flood_with_a_slow_reader_queues_one_frame_pair() {
        const KIB: usize = 1024;
        let flood = newline_flood(2 * KIB * KIB);
        print_flood_header();
        for prefill_rows in [0, SCROLLBACK + 200] {
            for acks in [FloodAcks::EveryNewest(1), FloodAcks::Lagged(50), FloodAcks::Lagged(300)] {
                let case = FloodCase {
                    drain: FloodDrain::Trickle(KIB),
                    ..paced_flood_case(acks, prefill_rows)
                };
                let r = measure_flood(&flood, case);
                print_flood_row(case, &r);
                let what = format!("acks={} prefill={prefill_rows}", acks.label());
                assert!(r.peak_undrained > 0, "{what}: the reader never fell behind — this proves nothing");
                assert!(r.max_visible_queued <= 1, "{what}: {} visible frames queued at once", r.max_visible_queued);
                assert!(
                    r.peak_write_buf <= r.largest_visible + r.largest_scrollback,
                    "{what}: backlog peaked at {} bytes, more than one frame pair ({} + {})",
                    r.peak_write_buf,
                    r.largest_visible,
                    r.largest_scrollback,
                );
                assert_eq!(r.last_screen_delivered, Some(true), "{what}: left on a stale screen");
            }
        }
    }

    /// The existing path untouched: a non-paced client's byte stream is the
    /// same whether or not a paced client of the same geometry is attached
    /// beside it. The paced client comes FIRST in the slice, so a paced arm
    /// in `broadcast_output` that stopped the loop (`break` for `continue`)
    /// would starve the plain client behind it. This pins the STREAM only:
    /// with equal geometry a shared dump and a separately built one are
    /// byte-identical, so whether the paced client takes part in the dump
    /// cache is pinned by `broadcast_output_only_marks_a_paced_client_dirty`.
    ///
    /// A third run (posh#225 Stage 3) adds a v2 paced client, acked each
    /// step through both `handle_frame_ack` and its history ack: neither the
    /// non-paced stream nor the v1 paced client's stream may change.
    #[test]
    fn non_paced_and_v1_paced_streams_are_identical_beside_a_v2_client() {
        const KIB: usize = 1024;
        let flood = newline_flood(64 * KIB);
        // (non-paced stream, v1 paced stream) with `paced` paced clients
        // ahead of the plain one: 0, the v1 one, or the v1 and a v2 one.
        let streams = |paced: usize| -> (Vec<u8>, Vec<u8>) {
            let mut term = Terminal::with_scrollback(24, 80, 1000);
            let (plain, _plain_peer) = scrollback_capable_conn(24, 80);
            let mut clients = Vec::new();
            let mut _peers = Vec::new();
            if paced >= 1 {
                let (v1, peer) = paced_conn(24, 80, &[]);
                clients.push(v1);
                _peers.push(peer);
            }
            if paced >= 2 {
                let (v2, peer) = paced_v2_conn(&term, sb2_entry(0, 0));
                clients.push(v2);
                _peers.push(peer);
            }
            clients.push(plain);
            let plain_at = clients.len() - 1;
            let (mut stream, mut v1_stream) = (Vec::new(), Vec::new());
            for (i, piece) in flood.chunks(4 * KIB).enumerate() {
                term.process(piece);
                broadcast_output(&mut clients, &term, piece);
                // Every chunk is a paced send opportunity: the paced clients
                // ack each frame at once and the clock steps a frame floor.
                let now = i as u64 * PACED_FRAME_FLOOR_MS;
                send_paced_frames(&mut clients, &term, Some(&term), now);
                stream.append(&mut clients[plain_at].write_buf);
                for (k, c) in clients[..plain_at].iter_mut().enumerate() {
                    let sent = c.producer.as_ref().unwrap().last_visible_num();
                    if k == 0 {
                        v1_stream.extend_from_slice(&c.write_buf);
                    }
                    handle_frame_ack(c, &ipc::encode_frame_ack(sent, 0), &term, now);
                    if let Some(h) = history_of(c) {
                        ack_history(c, h.epoch().unwrap(), h.sent_upto());
                    }
                    c.write_buf.clear();
                }
            }
            for c in &clients[..plain_at] {
                let sent = c.producer.as_ref().unwrap().last_visible_num();
                assert!(sent > 1, "a paced client must be sent frames for this to mean anything");
            }
            if paced >= 2 {
                let h = history_of(&clients[1]).expect("the v2 client's cursor");
                assert!(h.acked_rows() > 0, "the v2 client must be sent history for this to mean anything");
            }
            (stream, v1_stream)
        };
        let (alone, _) = streams(0);
        let (beside_v1, v1_alone) = streams(1);
        let (beside_both, v1_beside_v2) = streams(2);
        assert!(!alone.is_empty() && !v1_alone.is_empty());
        assert_eq!(alone.len(), beside_v1.len(), "the non-paced stream changed length");
        assert!(alone == beside_v1, "the non-paced stream changed beside a paced client");
        assert!(alone == beside_both, "the non-paced stream changed beside a v2 client");
        assert!(v1_alone == v1_beside_v2, "the v1 paced stream changed beside a v2 client");
    }

    // ---- posh#225 Stage 3: ack latency ----

    /// Feed `bytes` to `term`, mark the paced client dirty and run the send
    /// pass at `now` (its buffer cleared first, as if the reader drained
    /// it). Returns the visible frame that pass sent; panics when it sent
    /// none, so a test cannot sample a frame that was never queued.
    fn send_paced_screen_at(c: &mut ClientConn, term: &mut Terminal, bytes: &[u8], now: u64) -> u64 {
        let before = c.producer.as_ref().unwrap().last_visible_num();
        term.process(bytes);
        broadcast_output(std::slice::from_mut(c), term, bytes);
        c.write_buf.clear();
        send_paced_frames(std::slice::from_mut(c), term, Some(term), now);
        let sent = c.producer.as_ref().unwrap().last_visible_num();
        assert!(sent > before, "the send pass at t={now} sent no frame");
        sent
    }

    fn acks_of(c: &ClientConn) -> AckLatency {
        c.pacing.as_ref().expect("a paced client").acks
    }

    /// A paced client whose first frame, sent at 100, was acked at 340: one
    /// 240 ms sample.
    fn paced_conn_with_a_240ms_sample(term: &mut Terminal) -> (ClientConn, UnixStream) {
        let (mut c, peer) = paced_conn(term.rows(), term.cols(), &[]);
        let sent = send_paced_screen_at(&mut c, term, b"a line\r\n", 100);
        handle_frame_ack(&mut c, &ipc::encode_frame_ack(sent, 0), term, 340);
        (c, peer)
    }

    #[test]
    fn a_paced_clients_ack_of_its_newest_frame_is_a_latency_sample() {
        let mut term = Terminal::with_scrollback(24, 80, 100);
        let (c, _peer) = paced_conn_with_a_240ms_sample(&mut term);
        let acks = acks_of(&c);
        assert_eq!(acks.last_ms, Some(240));
        assert_eq!(acks.srtt_ms, Some(240));
        assert_eq!(acks.min_ms, Some(240));
        assert_eq!(acks.max_ms, 240);
        assert_eq!(acks.samples, 1);
        assert_eq!(acks.last_ack_at, Some(340));
    }

    #[test]
    fn ack_latency_smooths_and_keeps_min_and_max() {
        let mut term = Terminal::with_scrollback(24, 80, 100);
        let (mut c, _peer) = paced_conn_with_a_240ms_sample(&mut term);
        let sent = send_paced_screen_at(&mut c, &mut term, b"another\r\n", 360);
        handle_frame_ack(&mut c, &ipc::encode_frame_ack(sent, 0), &term, 400);
        let acks = acks_of(&c);
        assert_eq!(acks.last_ms, Some(40));
        assert_eq!(acks.srtt_ms, Some((7 * 240 + 40) / 8));
        assert_eq!(acks.min_ms, Some(40));
        assert_eq!(acks.max_ms, 240);
        assert_eq!(acks.samples, 2);
    }

    /// With a newer frame already queued (an RTT past the ack wait), the ack
    /// of the older one is still that frame's own round trip.
    #[test]
    fn an_ack_of_an_older_frame_samples_that_frames_own_round_trip() {
        let mut term = Terminal::with_scrollback(24, 80, 100);
        let (mut c, _peer) = paced_conn(24, 80, &[]);
        let first = send_paced_screen_at(&mut c, &mut term, b"one\r\n", 100);
        let second = send_paced_screen_at(&mut c, &mut term, b"two\r\n", 100 + PACED_ACK_WAIT_MS);
        handle_frame_ack(&mut c, &ipc::encode_frame_ack(first, 0), &term, 400);
        let acks = acks_of(&c);
        assert_eq!(acks.last_ack_at, Some(400));
        assert_eq!(acks.samples, 1);
        assert_eq!(acks.last_ms, Some(300));

        handle_frame_ack(&mut c, &ipc::encode_frame_ack(second, 0), &term, 700);
        let acks = acks_of(&c);
        assert_eq!(acks.last_ms, Some(350));
        assert_eq!(acks.samples, 2);
    }

    /// A paced frame with history behind it is visible N plus scrollback
    /// N+1; the viewport's ack names N+1, which confirms N: its sample.
    #[test]
    fn an_ack_of_the_scrollback_slot_samples_its_visible_frame() {
        let (rows, cols) = (5u16, 24u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);
        let scrollback = caps::Cap {
            id: caps::CAP_SCROLLBACK,
            payload: vec![0],
        };
        let (mut c, _peer) = paced_conn(rows, cols, &[scrollback]);
        assert!(c.build_frame_from(&term));
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        c.write_buf.clear();

        scroll_off(&mut term, 14);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), 100);
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(frames.len(), 2, "the visible frame, then its scrollback");
        assert!(matches!(frames[1].body, FrameBody::Scrollback { .. }), "got {:?}", frames[1].body);
        let visible = c.producer.as_ref().unwrap().last_visible_num();
        let slot = frames[1].frame_num;
        assert_eq!(slot, visible + 1);

        handle_frame_ack(&mut c, &ipc::encode_frame_ack(slot, 0), &term, 340);
        assert_eq!(acks_of(&c).last_ms, Some(240));
        assert_eq!(acks_of(&c).samples, 1);
        handle_frame_ack(&mut c, &ipc::encode_frame_ack(slot, 0), &term, 400);
        assert_eq!(acks_of(&c).samples, 1, "a repeated ack of the slot adds no sample");
    }

    #[test]
    fn a_repeated_or_resync_ack_is_no_sample() {
        let mut term = Terminal::with_scrollback(24, 80, 100);
        let (mut c, _peer) = paced_conn_with_a_240ms_sample(&mut term);
        let acked = c.producer.as_ref().unwrap().acked_num();
        handle_frame_ack(&mut c, &ipc::encode_frame_ack(acked, 0), &term, 360);
        assert_eq!(acks_of(&c).samples, 1, "a repeated ack confirms nothing new");
        assert_eq!(acks_of(&c).last_ack_at, Some(360), "but it did arrive");

        let sent = send_paced_screen_at(&mut c, &mut term, b"another\r\n", 400);
        handle_frame_ack(&mut c, &ipc::encode_frame_ack(sent, ipc::FRAME_ACK_RESYNC), &term, 450);
        assert_eq!(acks_of(&c).samples, 1, "a RESYNC rejected the frame: no round trip");
        assert_eq!(acks_of(&c).last_ack_at, Some(450));
    }

    #[test]
    fn an_unpaced_client_records_no_ack_latency() {
        let term = Terminal::with_scrollback(24, 80, 100);
        let (mut c, _peer) = lossy_conn(24, 80, &[]);
        assert!(c.request_frame_from(&term));
        handle_frame_ack(&mut c, &ipc::encode_frame_ack(1, 0), &term, 340);
        assert_eq!(c.pacing, None);
    }

    #[test]
    fn backlog_log_fields_carry_the_ack_latency() {
        let mut term = Terminal::with_scrollback(24, 80, 100);
        let (fresh, _p) = paced_conn(24, 80, &[]);
        let fields = backlog_log_fields(&fresh, 400);
        assert!(fields.ends_with(" paced=1 ack_ms=none ack_n=0 ack_age_ms=none"), "{fields}");

        let (sampled, _q) = paced_conn_with_a_240ms_sample(&mut term);
        let fields = backlog_log_fields(&sampled, 400);
        assert!(fields.ends_with(" paced=1 ack_ms=240/240/240/240 ack_n=1 ack_age_ms=60"), "{fields}");

        let (plain, _r) = lossy_conn(24, 80, &[]);
        let fields = backlog_log_fields(&plain, 400);
        assert!(fields.ends_with(" paced=0 ack_ms=none ack_n=0 ack_age_ms=none"), "{fields}");
    }

    #[test]
    fn the_ack_latency_line_is_due_once_per_interval_with_new_samples() {
        let mut term = Terminal::with_scrollback(24, 80, 100);
        let (mut idle, _p) = paced_conn(24, 80, &[]);
        assert_eq!(ack_latency_log_line(&mut idle, 340), None, "no samples");

        let (mut c, _q) = paced_conn_with_a_240ms_sample(&mut term);
        let line = ack_latency_log_line(&mut c, 340).expect("the first sample is due");
        assert!(line.starts_with("paced ack latency fd="), "{line}");
        assert!(line.ends_with(" new=1"), "{line}");
        assert_eq!(ack_latency_log_line(&mut c, 341), None, "no new sample");

        let sent = send_paced_screen_at(&mut c, &mut term, b"another\r\n", 360);
        handle_frame_ack(&mut c, &ipc::encode_frame_ack(sent, 0), &term, 400);
        assert_eq!(ack_latency_log_line(&mut c, 400), None, "a new sample, but within the interval");
        let line = ack_latency_log_line(&mut c, 340 + ACK_LOG_INTERVAL_MS).expect("the interval passed");
        assert!(line.ends_with(" new=1"), "{line}");
    }

    /// The flood harness's `Lagged(300)` acks each frame 300 ms after it
    /// reached the reader, one pace step after it was queued: the daemon's
    /// measured round trip is that RTT plus the step (≈ 301 ms). Past the
    /// ack wait, so each ack lands after a newer frame was queued — the
    /// regime a newest-frame-only sample would never measure. The flood
    /// must outlast one RTT for an ack to land at all: 2 MiB is 512 chunks
    /// (~512 ms), where 256 KiB (~64 ms) ends before the first ack and its
    /// tail exits without waiting for one.
    #[test]
    fn posh225_paced_flood_measures_the_round_trip_as_ack_latency() {
        const KIB: usize = 1024;
        let flood = newline_flood(2 * KIB * KIB);
        let r = measure_flood(&flood, paced_flood_case(FloodAcks::Lagged(300), 0));
        let acks = r.ack_latency.expect("a paced run reports its ack latency");
        eprintln!("paced flood ack latency: {acks:?}");
        assert!(acks.samples > 0, "no frame's round trip was sampled");
        assert!(acks.min_ms >= Some(300), "min {:?}", acks.min_ms);
        let srtt = acks.srtt_ms.expect("a sample");
        assert!((300..=310).contains(&srtt), "srtt {srtt} ms ({acks:?})");
    }

    // ---- posh#225 Stage 3: v2 history ----

    fn sb2_entry(epoch: u8, acked_rows: u64) -> caps::Cap {
        caps::encode_scrollback2_client(&caps::Scrollback2Client {
            ring_depth: 0,
            epoch,
            acked_rows,
        })
    }

    /// A paced client whose Init carried SCROLLBACK + `entry`, opened the
    /// way the daemon's Init arm opens it.
    fn paced_v2_conn(term: &Terminal, entry: caps::Cap) -> (ClientConn, UnixStream) {
        let (mut c, peer) = paced_conn(term.rows(), term.cols(), &[SCROLLBACK_CAP[0].clone(), entry]);
        c.sb_floor = term.primary_scrollback_total();
        c.open_history(term);
        (c, peer)
    }

    /// What the viewport's next message would forward: its cumulative ack.
    fn ack_history(c: &mut ClientConn, epoch: u8, rows: u64) {
        c.absorb_client_caps(&[sb2_entry(epoch, rows)], 0, false);
    }

    /// `(epoch, row_offset, rows)` of every v2 body among `frames`.
    fn history_bodies(frames: &[ServerFrame]) -> Vec<(u8, u64, usize)> {
        frames
            .iter()
            .filter_map(|f| match &f.body {
                FrameBody::Scrollback2 {
                    epoch,
                    row_offset,
                    rows,
                } => Some((*epoch, *row_offset, rows.len())),
                _ => None,
            })
            .collect()
    }

    fn history_of(c: &ClientConn) -> Option<HistoryCursor> {
        c.history().copied()
    }

    /// A terminal whose screen is full, so every further line scrolls
    /// exactly one row into its `ring`-row scrollback.
    fn v2_term(rows: u16, cols: u16, ring: usize) -> Terminal {
        let mut t = Terminal::with_scrollback(rows, cols, ring);
        for _ in 1..rows {
            t.process(b"-\r\n");
        }
        assert_eq!(t.primary_scrollback_total(), 0);
        t
    }

    /// Scroll `n` distinct rows into the ring.
    fn scroll_rows(term: &mut Terminal, n: u64) {
        let first = term.primary_scrollback_total();
        for i in first..first + n {
            term.process(format!("history row {i:05}\r\n").as_bytes());
        }
        assert_eq!(term.primary_scrollback_total(), first + n);
    }

    /// The newest `n` ring rows, oldest first.
    fn newest_ring_rows(term: &Terminal, n: usize) -> Vec<Vec<u8>> {
        let len = term.primary_scrollback_len();
        (len - n..len).map(|i| term.dump_scrollback_row(i).unwrap()).collect()
    }

    /// Runs the send pass at this client's history opportunity — its buffer
    /// cleared first, as if the reader drained it — and returns the frames
    /// it queued.
    fn pass_at_the_history_opportunity(c: &mut ClientConn, term: &Terminal) -> Vec<ServerFrame> {
        c.write_buf.clear();
        let at = c.history_send_at(term).expect("a history body is due");
        send_paced_frames(std::slice::from_mut(c), term, Some(term), at);
        decode_server_frames(&c.write_buf)
    }

    fn last_history_send(c: &ClientConn) -> u64 {
        history_of(c).expect("a cursor").last_send()
    }

    /// A v2 viewport whose attach keyframe (frame 1) was built and acked
    /// through the bridge, its buffer cleared. No paced frame went out, so
    /// its next screen is due at once while history waits for its floor.
    fn v2_conn_with_an_acked_attach(term: &Terminal) -> (ClientConn, UnixStream) {
        let (mut c, peer) = paced_v2_conn(term, sb2_entry(0, 0));
        assert!(c.build_frame_from(term));
        c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
        c.write_buf.clear();
        (c, peer)
    }

    #[test]
    fn a_paced_scrollback2_init_opens_a_history_cursor_and_nothing_else_does() {
        let term = v2_term(5, 24, 100);
        let (c, _p0) = paced_v2_conn(&term, sb2_entry(0, 0));
        assert_eq!(history_of(&c).and_then(|h| h.epoch()), Some(1), "epoch 0: a fresh epoch");

        let (mut v1, _p1) = paced_conn(5, 24, &SCROLLBACK_CAP);
        v1.open_history(&term);
        assert!(v1.is_paced());
        assert_eq!(history_of(&v1), None, "no id 10: v1 history");

        let (mut unpaced, _p2) = lossy_conn(5, 24, &[SCROLLBACK_CAP[0].clone(), sb2_entry(0, 0)]);
        unpaced.open_history(&term);
        assert_eq!(unpaced.pacing, None, "v2 is gated on pacing");

        let short = caps::Cap {
            id: caps::CAP_SCROLLBACK2,
            payload: vec![0; 9],
        };
        let (malformed, _p3) = paced_v2_conn(&term, short);
        assert!(malformed.is_paced());
        assert_eq!(history_of(&malformed), None, "a malformed entry is ignored");
    }

    #[test]
    fn a_viewport_holding_an_epoch_continues_it_at_its_count() {
        let mut term = v2_term(5, 24, 100);
        scroll_rows(&mut term, 20);
        let (mut c, _peer) = paced_v2_conn(&term, sb2_entry(7, 40));
        scroll_rows(&mut term, 3);
        let frames = pass_at_the_history_opportunity(&mut c, &term);
        assert_eq!(history_bodies(&frames), vec![(7, 40, 3)]);
    }

    #[test]
    fn a_bare_reinit_keeps_the_history_cursor() {
        let mut term = v2_term(5, 24, 100);
        let (mut c, _peer) = paced_v2_conn(&term, sb2_entry(0, 0));
        scroll_rows(&mut term, 8);
        pass_at_the_history_opportunity(&mut c, &term);
        ack_history(&mut c, 1, 5);
        let before = history_of(&c).expect("a cursor");
        assert_eq!(before.acked_rows(), 5);
        c.apply_init(&ipc::encode_resize(5, 24));
        c.open_history(&term);
        assert_eq!(history_of(&c), Some(before));
    }

    #[test]
    fn every_frame_to_a_v2_viewport_carries_the_scrollback2_ack() {
        let mut term = v2_term(5, 24, 100);
        let (mut v2, _p0) = paced_v2_conn(&term, sb2_entry(0, 0));
        let (mut v1, _p1) = paced_conn(5, 24, &SCROLLBACK_CAP);
        term.process(b"a line\r\n");
        for c in [&mut v2, &mut v1] {
            broadcast_output(std::slice::from_mut(c), &term, b"x");
            let frames = send_at_the_opportunity(c, &term);
            assert!(
                matches!(frames[0].body, FrameBody::Full(_) | FrameBody::Diff { .. }),
                "a visible frame first, got {:?}",
                frames[0].body
            );
        }
        let ack = |c: &ClientConn| {
            let frames = decode_server_frames(&c.write_buf);
            caps::find(&frames[0].caps, caps::CAP_SCROLLBACK2).map(|cap| cap.payload.clone())
        };
        assert_eq!(ack(&v2), Some(vec![0x02, 1]), "the server's {{0x02, epoch}} entry");
        assert_eq!(ack(&v1), None, "a paced non-v2 client's frame carries no id 10");

        // A non-paced client that advertised id 10 is byte-identical whether
        // or not `open_history` ran for it: it is a no-op without pacing.
        let stream = |open: bool| -> Vec<u8> {
            let mut term = v2_term(5, 24, 100);
            let (mut c, _peer) = lossy_conn(5, 24, &[SCROLLBACK_CAP[0].clone(), sb2_entry(0, 0)]);
            c.sb_floor = term.primary_scrollback_total();
            if open {
                c.open_history(&term);
            }
            assert!(c.request_frame_from(&term));
            c.apply_frame_ack(&ipc::encode_frame_ack(1, 0));
            for n in [1, 3, 7] {
                scroll_rows(&mut term, n);
                broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
                let newest = c.producer.as_ref().unwrap().current_num();
                c.apply_frame_ack(&ipc::encode_frame_ack(newest, 0));
            }
            let frames = decode_server_frames(&c.write_buf);
            assert!(frames.iter().any(|f| matches!(f.body, FrameBody::Scrollback { .. })), "v1 history");
            assert!(frames.iter().all(|f| caps::find(&f.caps, caps::CAP_SCROLLBACK2).is_none()));
            c.write_buf
        };
        let (plain, opened) = (stream(false), stream(true));
        assert!(!plain.is_empty());
        assert!(plain == opened, "open_history changed a non-paced client's stream");
    }

    #[test]
    fn a_v2_viewport_gets_scrollback2_bodies_and_never_v1() {
        let mut term = v2_term(5, 24, 1000);
        let (mut c, _peer) = v2_conn_with_an_acked_attach(&term);
        scroll_rows(&mut term, 10);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");

        let frames = send_at_the_opportunity(&mut c, &term);
        assert_eq!(frames.len(), 1, "one visible frame and nothing behind it: {frames:?}");
        assert!(
            matches!(frames[0].body, FrameBody::Full(_) | FrameBody::Diff { .. }),
            "a visible body, got {:?}",
            frames[0].body
        );

        let visible = c.producer.as_ref().unwrap().last_visible_num();
        let current = c.producer.as_ref().unwrap().current_num();
        let frames = pass_at_the_history_opportunity(&mut c, &term);
        assert_eq!(frames.len(), 1);
        match &frames[0].body {
            FrameBody::Scrollback2 {
                epoch,
                row_offset,
                rows,
            } => {
                assert_eq!((*epoch, *row_offset), (1, 0));
                assert_eq!(*rows, newest_ring_rows(&term, 10));
            }
            other => panic!("expected a v2 body, got {other:?}"),
        }
        assert_eq!(frames[0].frame_num, visible, "it rides the newest visible number");
        assert_eq!(
            c.producer.as_ref().unwrap().current_num(),
            current,
            "and takes no producer slot"
        );
    }

    #[test]
    fn the_screen_takes_the_first_tie_then_screen_and_history_alternate() {
        let mut term = v2_term(5, 24, 1000);
        let (mut c, _peer) = paced_v2_conn(&term, sb2_entry(0, 0));
        scroll_rows(&mut term, 10);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        assert_eq!(c.paced_send_at(), Some(0));
        assert_eq!(c.history_send_at(&term), Some(0));
        send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), 0);
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(frames.len(), 1, "one body per opportunity");
        assert_eq!(history_bodies(&frames), vec![], "the first tie goes to the screen");
        let sent = c.producer.as_ref().unwrap().last_visible_num();
        c.apply_frame_ack(&ipc::encode_frame_ack(sent, 0));
        c.write_buf.clear();

        scroll_rows(&mut term, 5);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        let now = PACED_FRAME_FLOOR_MS;
        assert_eq!(c.paced_send_at(), Some(now));
        assert_eq!(c.history_send_at(&term), Some(0), "history has no floor");
        send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), now);
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(history_bodies(&frames), vec![(1, 0, 15)], "after a visible send, history");
        assert_eq!(frames.len(), 1, "one body per opportunity");

        c.write_buf.clear();
        scroll_rows(&mut term, 5);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        let now = 2 * PACED_FRAME_FLOOR_MS;
        assert!(c.paced_send_at().is_some_and(|at| at <= now));
        assert_eq!(c.history_send_at(&term), Some(PACED_FRAME_FLOOR_MS), "due since its last body");
        send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), now);
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(frames.len(), 1, "one body per opportunity");
        assert_eq!(history_bodies(&frames), vec![], "after history, the screen");
        assert!(!c.owes_paced_frame());
    }

    #[test]
    fn a_history_body_carries_at_most_sb2_rows_per_body() {
        let mut term = v2_term(5, 24, 1000);
        let (mut c, _peer) = paced_v2_conn(&term, sb2_entry(0, 0));
        scroll_rows(&mut term, 600);
        let mut lens = Vec::new();
        while c.history_send_at(&term).is_some() {
            let bodies = history_bodies(&pass_at_the_history_opportunity(&mut c, &term));
            let &[(epoch, row_offset, rows)] = bodies.as_slice() else {
                panic!("one body per pass, got {bodies:?}");
            };
            lens.push(rows);
            ack_history(&mut c, epoch, row_offset + rows as u64);
            c.write_buf.clear();
        }
        assert_eq!(lens, vec![256, 256, 88]);
    }

    #[test]
    fn history_waits_for_room_in_the_window() {
        let mut term = v2_term(5, 24, 1000);
        let (mut c, _peer) = paced_v2_conn(&term, sb2_entry(0, 0));
        scroll_rows(&mut term, 600);
        let first = history_bodies(&pass_at_the_history_opportunity(&mut c, &term));
        let after_first = last_history_send(&c);
        let second = history_bodies(&pass_at_the_history_opportunity(&mut c, &term));
        let last = last_history_send(&c);
        assert_eq!((first, second), (vec![(1, 0, 256)], vec![(1, 256, 256)]));
        assert_eq!(last, after_first, "no floor between bodies: due as soon as the buffer drains");
        c.write_buf.clear();
        assert_eq!(
            c.history_send_at(&term),
            Some(last + HISTORY_RESEND_INITIAL_MS),
            "88 fresh rows wait: the window is full"
        );
        ack_history(&mut c, 1, 256);
        assert_eq!(c.history_send_at(&term), Some(last), "room again: due at once");
        let third = history_bodies(&pass_at_the_history_opportunity(&mut c, &term));
        assert_eq!(third, vec![(1, 512, 88)]);
    }

    #[test]
    fn a_withheld_ack_is_resent_from_the_ack_only_after_the_floor() {
        let mut term = v2_term(5, 24, 1000);
        let (mut c, _peer) = paced_v2_conn(&term, sb2_entry(0, 0));
        scroll_rows(&mut term, 512);
        pass_at_the_history_opportunity(&mut c, &term);
        pass_at_the_history_opportunity(&mut c, &term);
        ack_history(&mut c, 1, 100);
        let last = last_history_send(&c);
        c.write_buf.clear();
        send_paced_frames(std::slice::from_mut(&mut c), &term, Some(&term), last + PACED_FRAME_FLOOR_MS);
        assert!(c.write_buf.is_empty(), "nothing is resent at the floor");
        assert_eq!(c.history_send_at(&term), Some(last + HISTORY_RESEND_INITIAL_MS));
        let frames = pass_at_the_history_opportunity(&mut c, &term);
        assert_eq!(history_bodies(&frames), vec![(1, 100, 256)], "resent from the ack");
    }

    #[test]
    fn resends_back_off_while_acks_stay_withheld() {
        let mut term = v2_term(5, 24, 1000);
        let (mut c, _peer) = paced_v2_conn(&term, sb2_entry(0, 0));
        scroll_rows(&mut term, 10);
        pass_at_the_history_opportunity(&mut c, &term);
        let base = HISTORY_RESEND_INITIAL_MS;
        for doublings in [1, 2, 4, 8, 8] {
            c.write_buf.clear();
            let last = last_history_send(&c);
            assert_eq!(c.history_send_at(&term), Some(last + doublings * base));
            let frames = pass_at_the_history_opportunity(&mut c, &term);
            assert_eq!(history_bodies(&frames), vec![(1, 0, 10)], "resent from the ack");
        }
        c.write_buf.clear();
        ack_history(&mut c, 1, 5);
        let last = last_history_send(&c);
        assert_eq!(c.history_send_at(&term), Some(last + base), "an advancing ack resets the backoff");
    }

    #[test]
    fn the_resend_floor_follows_the_measured_ack_latency() {
        let term = v2_term(5, 24, 100);
        let (mut c, _peer) = paced_v2_conn(&term, sb2_entry(0, 0));
        let mut resend_after = |srtt: Option<u64>| {
            c.pacing.as_mut().unwrap().acks.srtt_ms = srtt;
            c.history_resend_after()
        };
        assert_eq!(resend_after(Some(600)), 1200);
        assert_eq!(resend_after(Some(50)), PACED_ACK_WAIT_MS, "never under the ack wait");
        assert_eq!(resend_after(None), HISTORY_RESEND_INITIAL_MS, "before any sample");
    }

    #[test]
    fn a_stale_epoch_or_backward_ack_is_ignored() {
        let term = v2_term(5, 24, 100);
        let (mut c, _peer) = paced_v2_conn(&term, sb2_entry(0, 0));
        ack_history(&mut c, 1, 5);
        ack_history(&mut c, 2, 9);
        assert_eq!(history_of(&c).unwrap().acked_rows(), 5, "another epoch's ack");
        ack_history(&mut c, 1, 3);
        assert_eq!(history_of(&c).unwrap().acked_rows(), 5, "a backward ack");
    }

    #[test]
    fn a_stalled_v2_viewport_gets_one_forward_jump_of_the_evicted_span() {
        let mut term = v2_term(5, 20, 50);
        let (mut c, _peer) = paced_v2_conn(&term, sb2_entry(0, 0));
        scroll_rows(&mut term, 10);
        let first = history_bodies(&pass_at_the_history_opportunity(&mut c, &term));
        assert_eq!(first, vec![(1, 0, 10)]);
        ack_history(&mut c, 1, 10);
        c.write_buf.clear();
        scroll_rows(&mut term, 200);
        let avail = 210;

        // (row_offset, rows) of every body, the first included; the rows
        // delivered after it.
        let mut spans: Vec<(u64, u64)> = vec![(0, 10)];
        let mut delivered: Vec<Vec<u8>> = Vec::new();
        while c.history_send_at(&term).is_some() {
            for f in pass_at_the_history_opportunity(&mut c, &term) {
                if let FrameBody::Scrollback2 {
                    epoch,
                    row_offset,
                    rows,
                } = f.body
                {
                    ack_history(&mut c, epoch, row_offset + rows.len() as u64);
                    spans.push((row_offset, rows.len() as u64));
                    delivered.extend(rows);
                }
            }
            c.write_buf.clear();
        }
        let jumps: Vec<(u64, u64)> = spans
            .windows(2)
            .map(|w| (w[0].0 + w[0].1, w[1].0))
            .filter(|(end, next)| end != next)
            .collect();
        let f = avail - 50;
        assert_eq!(jumps, vec![(10, f)], "one forward jump of the {} evicted rows", f - 10);
        assert_eq!(delivered, newest_ring_rows(&term, 50), "the whole retained ring, in order");
    }

    #[test]
    fn a_viewports_own_resize_bumps_its_epoch_and_a_width_change_reanchors_the_others() {
        let mut term = v2_term(5, 24, 100);
        let (a, _pa) = paced_v2_conn(&term, sb2_entry(0, 0));
        let (b, _pb) = paced_v2_conn(&term, sb2_entry(0, 0));
        let mut clients = vec![a, b];
        let epochs = |cs: &[ClientConn]| -> Vec<Option<u8>> {
            cs.iter().map(|c| history_of(c).and_then(|h| h.epoch())).collect()
        };
        // `b` holds rows 0..8, acked to 5.
        scroll_rows(&mut term, 8);
        assert_eq!(history_bodies(&pass_at_the_history_opportunity(&mut clients[1], &term)), vec![(1, 0, 8)]);
        ack_history(&mut clients[1], 1, 5);
        clients[1].write_buf.clear();
        // Rows 8..10 scroll but are not sent before the reflow.
        scroll_rows(&mut term, 2);

        assert!(clients[0].apply_resize(&ipc::encode_resize(6, 24)));
        let size = (term.rows(), term.cols());
        reset_history_on_resize(&mut clients, &term, size);
        assert_eq!(epochs(&clients), vec![Some(2), Some(1)], "only the resized viewport");

        // A session width change: `a`'s size is unchanged since, so neither
        // bumps; `b` keeps its epoch, its ack, and its count.
        term.resize(5, 30);
        reset_history_on_resize(&mut clients, &term, (5, 24));
        assert_eq!(epochs(&clients), vec![Some(2), Some(1)], "a reflow bumps no epoch");
        let b = history_of(&clients[1]).unwrap();
        assert_eq!((b.acked_rows(), b.sent_upto()), (5, 8), "the viewport's ring stays valid");
        assert_eq!(b.avail(term.primary_scrollback_total()), 10, "re-anchored at the count");

        for (c, want) in clients.iter_mut().zip([2u8, 1]) {
            broadcast_output(std::slice::from_mut(c), &term, b"x");
            let frames = send_at_the_opportunity(c, &term);
            let ack = caps::find(&frames[0].caps, caps::CAP_SCROLLBACK2).expect("the ack rides it");
            assert_eq!(caps::decode_scrollback2_ack(&ack.payload).unwrap(), want);
            c.write_buf.clear();
        }

        // `b`'s next fresh body starts at the count at the reflow: a forward
        // jump over the 2 unsent rows, never a silent seam.
        scroll_rows(&mut term, 3);
        ack_history(&mut clients[1], 1, 8);
        let frames = pass_at_the_history_opportunity(&mut clients[1], &term);
        assert_eq!(history_bodies(&frames), vec![(1, 10, 3)]);
    }

    /// posh#225 Stage 3: a session HEIGHT grow pops ring rows back onto the
    /// grid without moving the total, renumbering the ring under every
    /// viewport whose own size did not change — unless it is re-anchored,
    /// its next body offers rows it already holds under newer numbers.
    #[test]
    fn a_session_height_grow_reanchors_the_other_viewports() {
        let mut term = v2_term(5, 24, 100);
        let (short, _ps) = paced_v2_conn(&term, sb2_entry(0, 0));
        let (mut tall, _pt) = paced_conn(8, 24, &[SCROLLBACK_CAP[0].clone(), sb2_entry(0, 0)]);
        tall.sb_floor = term.primary_scrollback_total();
        tall.open_history(&term);
        let mut clients = vec![short, tall];
        // The tall viewport holds rows 0..6; rows 6..10 are not sent yet.
        scroll_rows(&mut term, 6);
        assert_eq!(history_bodies(&pass_at_the_history_opportunity(&mut clients[1], &term)), vec![(1, 0, 6)]);
        ack_history(&mut clients[1], 1, 6);
        clients[1].write_buf.clear();
        let held = newest_ring_rows(&term, 6);
        scroll_rows(&mut term, 4);

        // The short viewport leaves: the session grows to the tall one's
        // height, popping 3 ring rows back onto the grid.
        clients.remove(0);
        let before = (term.rows(), term.cols());
        term.resize(8, 24);
        assert_eq!(term.primary_scrollback_len(), 7, "3 ring rows popped onto the grid");
        reset_history_on_resize(&mut clients, &term, before);
        assert_eq!(history_of(&clients[0]).and_then(|h| h.epoch()), Some(1), "no new epoch");

        scroll_rows(&mut term, 1);
        let mut delivered: Vec<Vec<u8>> = Vec::new();
        while clients[0].history_send_at(&term).is_some() {
            for f in pass_at_the_history_opportunity(&mut clients[0], &term) {
                if let FrameBody::Scrollback2 {
                    epoch,
                    row_offset,
                    rows,
                } = f.body
                {
                    ack_history(&mut clients[0], epoch, row_offset + rows.len() as u64);
                    delivered.extend(rows);
                }
            }
            clients[0].write_buf.clear();
        }
        assert!(!delivered.is_empty(), "the new row is delivered");
        for row in &delivered {
            assert!(!held.contains(row), "re-delivered a held row: {}", String::from_utf8_lossy(row));
        }
    }

    #[test]
    fn no_history_body_while_the_overlay_is_up() {
        let mut term = v2_term(5, 24, 100);
        let (mut c, _peer) = paced_v2_conn(&term, sb2_entry(0, 0));
        scroll_rows(&mut term, 10);
        let now = 10 * HISTORY_RESEND_INITIAL_MS;
        assert!(c.history_send_at(&term).is_some_and(|at| at <= now), "rows are pending");
        send_paced_frames(std::slice::from_mut(&mut c), &term, None, now);
        assert!(c.write_buf.is_empty(), "no body while the overlay is up");
        assert_eq!(paced_poll_timeout(std::slice::from_ref(&c), None, now), -1);
    }

    #[test]
    fn the_poll_wakes_for_pending_history() {
        let mut term = v2_term(5, 24, 100);
        let (c, _peer) = paced_v2_conn(&term, sb2_entry(0, 0));
        scroll_rows(&mut term, 10);
        assert!(!c.owes_paced_frame(), "a clean screen");
        let now = 5;
        assert_eq!(c.history_send_at(&term), Some(0), "due at once: no floor for history");
        assert_eq!(paced_poll_timeout(std::slice::from_ref(&c), Some(&term), now), 0);
    }

    #[test]
    fn the_exit_flush_sends_only_the_screen() {
        let mut term = v2_term(5, 24, 1000);
        let (mut c, _peer) = v2_conn_with_an_acked_attach(&term);
        scroll_rows(&mut term, 10);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        assert!(c.history_send_at(&term).is_some(), "rows are pending");
        flush_paced_frames(std::slice::from_mut(&mut c), &term, 0);
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(frames.len(), 1, "the screen and nothing else: {frames:?}");
        assert!(
            matches!(frames[0].body, FrameBody::Full(_) | FrameBody::Diff { .. }),
            "got {:?}",
            frames[0].body
        );
    }

    // ---- posh#225 Stage 3: the flood, addressed (v2) ----

    /// The posh#240 witness for v2: at RTTs below the first resend floor
    /// (`HISTORY_RESEND_INITIAL_MS`), and with the measured floor (≥ 2 RTT)
    /// afterwards, every row the flood scrolled reaches the viewport exactly
    /// once, in order, equal to the terminal's, and is acked.
    #[test]
    fn posh225_v2_flood_ships_every_scrolled_row_exactly_once() {
        const KIB: usize = 1024;
        let flood = newline_flood(256 * KIB);
        print_flood_header();
        for prefill_rows in [0, SCROLLBACK + 200] {
            for acks in [FloodAcks::EveryNewest(1), FloodAcks::Lagged(50), FloodAcks::Lagged(300)] {
                let case = paced_v2_flood_case(acks, prefill_rows);
                let r = measure_flood(&flood, case);
                print_flood_row(case, &r);
                let what = format!("acks={} prefill={prefill_rows}", acks.label());
                assert!(r.rows_scrolled > 0, "{what}: the flood must scroll rows off");
                assert_eq!(r.rows_shipped, r.rows_scrolled, "{what}: rows shipped (with repeats)");
                assert_eq!(r.rows_unique, r.rows_scrolled, "{what}: rows the viewport appended");
                assert_eq!(r.rows_repeated, 0, "{what}: a row reached the viewport twice");
                assert_eq!(r.forward_jumps, 0, "{what}: a forward jump");
                assert_eq!(r.rows_mismatched, 0, "{what}: a row differed from the terminal's");
                assert_eq!(r.rows_acked, r.rows_scrolled, "{what}: rows acked");
            }
        }
    }

    /// The bound Task 2.5 deferred: with v2 history the backlog is ONE body
    /// — a visible frame or a ≤ 256-row history body — for every cadence,
    /// a never-acking reader included, so it stays under 64 KiB.
    #[test]
    fn posh225_v2_flood_backlog_is_one_body_for_every_cadence() {
        const KIB: usize = 1024;
        let flood = newline_flood(256 * KIB);
        print_flood_header();
        for prefill_rows in [0, SCROLLBACK + 200] {
            for acks in PACED_FLOOD_CADENCES.into_iter().chain([FloodAcks::Lagged(1500)]) {
                let case = paced_v2_flood_case(acks, prefill_rows);
                let r = measure_flood(&flood, case);
                print_flood_row(case, &r);
                let what = format!("acks={} prefill={prefill_rows}", acks.label());
                assert_eq!(r.crossed_backlog_at, None, "{what}: crossed MAX_CLIENT_BACKLOG");
                assert!(r.max_visible_queued <= 1, "{what}: {} visible frames queued at once", r.max_visible_queued);
                assert!(
                    r.peak_write_buf <= r.largest_visible.max(r.largest_history_body),
                    "{what}: backlog peaked at {} bytes, more than one body (visible {}, history {})",
                    r.peak_write_buf,
                    r.largest_visible,
                    r.largest_history_body,
                );
                assert!(r.peak_write_buf < 64 * KIB, "{what}: backlog peaked at {} bytes", r.peak_write_buf);
                assert!(r.largest_visible < 64 * KIB, "{what}: a visible frame was {} bytes", r.largest_visible);
                assert_eq!(r.last_screen_delivered, Some(true), "{what}: left on a stale screen");
            }
        }
    }

    /// Past Stage 2's v1 cliff (≈ 5 ack waits), where v1 history acked 0
    /// rows: a 1.5 s RTT still delivers every row, at most one window of
    /// spurious resends repeated before the first latency sample (×2 for
    /// the two bodies of a round), and the visible base survives — every
    /// visible frame is built against an acked base. (The plan's witness was
    /// `full_frames == 0`; under a flood `DumpDiff` sends a `Full` whenever
    /// the diff is no net win, so every paced flood frame is a `Full` even at
    /// prompt acks. `baseless_frames` is the direct measure.)
    #[test]
    fn posh225_v2_flood_at_a_1500_ms_rtt_delivers_its_history() {
        const KIB: usize = 1024;
        let flood = newline_flood(256 * KIB);
        print_flood_header();
        for prefill_rows in [0, SCROLLBACK + 200] {
            let case = paced_v2_flood_case(FloodAcks::Lagged(1500), prefill_rows);
            let r = measure_flood(&flood, case);
            print_flood_row(case, &r);
            let what = format!("prefill={prefill_rows}");
            assert_eq!(r.rows_unique, r.rows_scrolled, "{what}: rows the viewport appended");
            assert_eq!(r.rows_acked, r.rows_scrolled, "{what}: rows acked");
            assert_eq!(r.forward_jumps, 0, "{what}: a forward jump");
            assert_eq!(r.rows_mismatched, 0, "{what}: a row differed from the terminal's");
            assert!(
                r.rows_repeated <= 2 * HISTORY_WINDOW_ROWS,
                "{what}: {} rows repeated (spurious resends before the first sample)",
                r.rows_repeated,
            );
            assert_eq!(r.baseless_frames, 0, "{what}: the visible base was lost");
        }
    }

    /// A viewport that never acks history: it receives the first window and
    /// nothing beyond (no ack, no room), and the window is re-sent once per
    /// resend floor, backing off 1, 2, 4, 8, 8 … × `HISTORY_RESEND_INITIAL_MS`
    /// — against Stage 2's one ring per ack wait.
    #[test]
    fn posh225_v2_flood_without_acks_resends_one_window_per_backed_off_floor() {
        const KIB: usize = 1024;
        let flood = newline_flood(256 * KIB);
        let case = FloodCase {
            drain: FloodDrain::Always,
            ..paced_v2_flood_case(FloodAcks::Never, 0)
        };
        let r = measure_flood(&flood, case);
        print_flood_header();
        print_flood_row(case, &r);
        assert!(r.rows_scrolled > HISTORY_WINDOW_ROWS, "the flood must outrun one window");
        assert_eq!(r.rows_unique, HISTORY_WINDOW_ROWS, "the first window and nothing beyond");
        assert_eq!((r.forward_jumps, r.rows_mismatched), (0, 0));

        // A resend round starts with a body from the ack (row 0); its gap
        // from the body before is the backed-off floor.
        let gaps: Vec<u64> = r
            .history_sends
            .windows(2)
            .filter(|w| w[1].1 == 0)
            .map(|w| w[1].0 - w[0].0)
            .collect();
        let rounds = gaps.len() as u64;
        assert!(rounds >= 4, "resends at 1, 2, 4 and 8 s fit the tail; got gaps {gaps:?}");
        let backoff = |i: usize| HISTORY_RESEND_INITIAL_MS << i.min(HISTORY_RESEND_MAX_DOUBLINGS as usize);
        let expected: Vec<u64> = (0..gaps.len()).map(backoff).collect();
        assert_eq!(gaps, expected, "resend rounds back off from the initial floor");
        assert!(
            r.rows_shipped <= (1 + rounds) * HISTORY_WINDOW_ROWS,
            "{} rows shipped over {rounds} resend rounds: more than one window per round",
            r.rows_shipped,
        );
    }

    /// A reader slower than the flood (1 KiB per 1 ms step against a 4 KiB/ms
    /// flood): it loses only the rows the ring evicted before their turn,
    /// each loss a forward jump of exactly that span, and ends holding every
    /// row the ring still holds — with at most one body queued throughout.
    #[test]
    fn posh225_v2_slow_reader_loses_only_rows_evicted_before_their_turn() {
        const KIB: usize = 1024;
        let flood = newline_flood(2 * KIB * KIB);
        print_flood_header();
        for prefill_rows in [0, SCROLLBACK + 200] {
            for acks in [FloodAcks::EveryNewest(1), FloodAcks::Lagged(50)] {
                let case = FloodCase {
                    drain: FloodDrain::Trickle(KIB),
                    ..paced_v2_flood_case(acks, prefill_rows)
                };
                let r = measure_flood(&flood, case);
                print_flood_row(case, &r);
                let what = format!("acks={} prefill={prefill_rows}", acks.label());
                let avail = r.rows_scrolled;
                assert!(r.peak_undrained > 0, "{what}: the reader never fell behind — this proves nothing");
                assert_eq!(
                    r.rows_unique + r.rows_jumped,
                    r.rows_scrolled,
                    "{what}: a row was neither delivered nor inside a forward jump"
                );
                assert_eq!(r.rows_mismatched, 0, "{what}: a row differed from the terminal's");
                assert_eq!(r.rows_repeated, 0, "{what}: a row reached the viewport twice");
                assert_eq!(r.viewport_rows, avail, "{what}: the viewport's final count");
                if let Some(&last) = r.jump_ends.last() {
                    assert!(
                        last <= avail - SCROLLBACK as u64,
                        "{what}: the last jump ended at {last}, inside the retained ring (from {})",
                        avail - SCROLLBACK as u64,
                    );
                }
                assert!(r.max_visible_queued <= 1, "{what}: {} visible frames queued at once", r.max_visible_queued);
                assert!(
                    r.peak_write_buf <= r.largest_visible.max(r.largest_history_body),
                    "{what}: backlog peaked at {} bytes, more than one body (visible {}, history {})",
                    r.peak_write_buf,
                    r.largest_visible,
                    r.largest_history_body,
                );
                assert_eq!(r.last_screen_delivered, Some(true), "{what}: left on a stale screen");
            }
        }
    }

    // ---- posh#225 Stage 4: the v2 extent ----

    /// [`paced_v2_conn`] (a fresh epoch) whose Init also asks for the v2
    /// extent (RFC 0009 §3.1).
    fn paced_v2x_conn(term: &Terminal) -> (ClientConn, UnixStream) {
        let (mut c, peer) = paced_conn(
            term.rows(),
            term.cols(),
            &[
                SCROLLBACK_CAP[0].clone(),
                sb2_entry(0, 0),
                caps::encode_scrollback2_extent_request(),
            ],
        );
        c.sb_floor = term.primary_scrollback_total();
        c.open_history(term);
        (c, peer)
    }

    /// The id-24 server entry of each frame, decoded.
    fn extents(frames: &[ServerFrame]) -> Vec<Option<caps::Scrollback2Extent>> {
        frames
            .iter()
            .map(|f| {
                caps::find(&f.caps, caps::CAP_SCROLLBACK2_EXTENT)
                    .map(|c| caps::decode_scrollback2_extent(&c.payload).expect("a well-formed extent"))
            })
            .collect()
    }

    fn extent(epoch: u8, avail_rows: u64, evicted_upto: u64) -> Option<caps::Scrollback2Extent> {
        Some(caps::Scrollback2Extent {
            epoch,
            avail_rows,
            evicted_upto,
        })
    }

    #[test]
    fn a_v2_viewport_that_asks_gets_the_extent_on_every_frame() {
        let mut term = v2_term(5, 24, 1000);
        let (mut c, _peer) = paced_v2x_conn(&term);
        scroll_rows(&mut term, 10);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        let visible = send_at_the_opportunity(&mut c, &term);
        assert_eq!(history_bodies(&visible), vec![], "the screen goes first");
        assert_eq!(extents(&visible), vec![extent(1, 10, 0)]);
        let history = pass_at_the_history_opportunity(&mut c, &term);
        assert_eq!(history_bodies(&history), vec![(1, 0, 10)]);
        assert_eq!(extents(&history), vec![extent(1, 10, 0)]);
    }

    #[test]
    fn a_v2_viewport_that_does_not_ask_gets_no_extent() {
        let mut term = v2_term(5, 24, 1000);
        let (mut c, _peer) = paced_v2_conn(&term, sb2_entry(0, 0));
        scroll_rows(&mut term, 10);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        let mut frames = send_at_the_opportunity(&mut c, &term);
        frames.extend(pass_at_the_history_opportunity(&mut c, &term));
        assert_eq!(frames.len(), 2);
        assert_eq!(history_bodies(&frames), vec![(1, 0, 10)]);
        assert_eq!(c.pacing.as_ref().unwrap().extent, None);
        for f in &frames {
            let ids: Vec<u8> = f.caps.iter().map(|c| c.id).collect();
            assert_eq!(ids, vec![caps::CAP_PROTOCOL_VERSION, caps::CAP_SCROLLBACK2], "as before Stage 4");
        }
    }

    #[test]
    fn a_request_without_scrollback2_gets_no_extent() {
        let mut term = v2_term(5, 24, 1000);
        let request = caps::encode_scrollback2_extent_request();
        let (mut paced, _p0) = paced_conn(5, 24, &[SCROLLBACK_CAP[0].clone(), request.clone()]);
        paced.sb_floor = term.primary_scrollback_total();
        paced.open_history(&term);
        assert!(paced.is_paced() && !paced.has_history(), "paced, v1 history");
        scroll_rows(&mut term, 10);
        broadcast_output(std::slice::from_mut(&mut paced), &term, b"x");
        let frames = send_at_the_opportunity(&mut paced, &term);
        assert!(!frames.is_empty());
        assert!(extents(&frames).iter().all(Option::is_none), "no cursor: no extent");

        let (mut lossy, _p1) = lossy_conn(5, 24, &[SCROLLBACK_CAP[0].clone(), sb2_entry(0, 0), request]);
        lossy.sb_floor = term.primary_scrollback_total();
        lossy.open_history(&term);
        assert!(lossy.request_frame_from(&term));
        scroll_rows(&mut term, 3);
        broadcast_output(std::slice::from_mut(&mut lossy), &term, b"x");
        let frames = decode_server_frames(&lossy.write_buf);
        assert!(!frames.is_empty());
        assert!(extents(&frames).iter().all(Option::is_none), "not paced: no extent");
    }

    /// The marker and the jump agree on the wire: a body after an eviction
    /// starts at its own frame's `evicted_upto`.
    #[test]
    fn the_extent_floor_is_the_daemons_eviction_floor() {
        let mut term = v2_term(5, 20, 50);
        let (mut c, _peer) = paced_v2x_conn(&term);
        scroll_rows(&mut term, 10);
        let first = pass_at_the_history_opportunity(&mut c, &term);
        assert_eq!(history_bodies(&first), vec![(1, 0, 10)]);
        ack_history(&mut c, 1, 10);
        c.write_buf.clear();
        scroll_rows(&mut term, 200);
        let frames = pass_at_the_history_opportunity(&mut c, &term);
        let &[(_, row_offset, _)] = history_bodies(&frames).as_slice() else {
            panic!("one body, got {frames:?}");
        };
        assert_eq!(extents(&frames), vec![extent(1, 210, 210 - 50)]);
        assert_eq!(row_offset, 210 - 50, "the body starts at its frame's floor");
    }

    /// `a_session_height_grow_reanchors_the_other_viewports`' setup: after
    /// the re-anchor, the floor is the viewport's count as of the resize.
    #[test]
    fn the_extent_floor_is_the_count_at_a_session_resize() {
        let mut term = v2_term(5, 24, 100);
        let (short, _ps) = paced_v2x_conn(&term);
        let (mut tall, _pt) = paced_conn(
            8,
            24,
            &[
                SCROLLBACK_CAP[0].clone(),
                sb2_entry(0, 0),
                caps::encode_scrollback2_extent_request(),
            ],
        );
        tall.sb_floor = term.primary_scrollback_total();
        tall.open_history(&term);
        let mut clients = vec![short, tall];
        scroll_rows(&mut term, 6);
        assert_eq!(history_bodies(&pass_at_the_history_opportunity(&mut clients[1], &term)), vec![(1, 0, 6)]);
        ack_history(&mut clients[1], 1, 6);
        clients[1].write_buf.clear();
        scroll_rows(&mut term, 4);

        clients.remove(0);
        let before = (term.rows(), term.cols());
        term.resize(8, 24);
        reset_history_on_resize(&mut clients, &term, before);
        scroll_rows(&mut term, 1);
        let frames = pass_at_the_history_opportunity(&mut clients[0], &term);
        assert_eq!(history_bodies(&frames), vec![(1, 10, 1)]);
        assert_eq!(extents(&frames), vec![extent(1, 11, 10)], "the floor is the count at the resize");
    }

    /// Under the escape overlay the send pass has no session terminal: a
    /// visible frame carries the extent of the newest pass that had one.
    #[test]
    fn the_extent_freezes_while_the_overlay_is_up() {
        let mut term = v2_term(5, 24, 1000);
        let (mut c, _peer) = paced_v2x_conn(&term);
        scroll_rows(&mut term, 5);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        let frames = send_at_the_opportunity(&mut c, &term);
        assert_eq!(extents(&frames), vec![extent(1, 5, 0)]);
        let sent = c.producer.as_ref().unwrap().last_visible_num();
        c.apply_frame_ack(&ipc::encode_frame_ack(sent, 0));
        c.write_buf.clear();

        scroll_rows(&mut term, 5);
        broadcast_output(std::slice::from_mut(&mut c), &term, b"x");
        let at = c.paced_send_at().expect("a frame is owed");
        send_paced_frames(std::slice::from_mut(&mut c), &term, None, at);
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(history_bodies(&frames), vec![], "no history under the overlay");
        assert_eq!(extents(&frames), vec![extent(1, 5, 0)], "the cached extent, not avail 10");
    }

    /// The id-10 entry of each frame, decoded to its epoch.
    fn sb2_epochs(frames: &[ServerFrame]) -> Vec<Option<u8>> {
        frames
            .iter()
            .map(|f| {
                caps::find(&f.caps, caps::CAP_SCROLLBACK2).map(|c| caps::decode_scrollback2_ack(&c.payload).unwrap())
            })
            .collect()
    }

    /// RFC 0009 §3.1: the extent names the epoch of the id-10 entry beside
    /// it — even when the viewport's own resize bumps the epoch while the
    /// overlay is up, so no send pass refreshes the cache before the frame.
    #[test]
    fn a_resize_under_the_overlay_keeps_the_extent_in_the_frames_epoch() {
        let mut term = v2_term(5, 24, 1000);
        let (c, _peer) = paced_v2x_conn(&term);
        let mut clients = vec![c];
        scroll_rows(&mut term, 5);
        broadcast_output(&mut clients, &term, b"x");
        let frames = send_at_the_opportunity(&mut clients[0], &term);
        assert_eq!(extents(&frames), vec![extent(1, 5, 0)]);
        let sent = clients[0].producer.as_ref().unwrap().last_visible_num();
        clients[0].apply_frame_ack(&ipc::encode_frame_ack(sent, 0));
        clients[0].write_buf.clear();

        assert!(clients[0].apply_resize(&ipc::encode_resize(6, 24)));
        let size = (term.rows(), term.cols());
        reset_history_on_resize(&mut clients, &term, size);
        broadcast_output(&mut clients, &term, b"x");
        let at = clients[0].paced_send_at().expect("a frame is owed");
        send_paced_frames(&mut clients, &term, None, at);
        let frames = decode_server_frames(&clients[0].write_buf);
        assert_eq!(frames.len(), 1, "one visible frame: {frames:?}");
        assert_eq!(sb2_epochs(&frames), vec![Some(2)], "the bumped epoch");
        assert_eq!(extents(&frames), vec![extent(2, 0, 0)], "an extent of the same epoch");
    }

    /// The cache is seeded when the cursor opens: a visible frame built
    /// before any send pass already carries the extent.
    #[test]
    fn the_first_visible_frame_after_open_history_carries_the_extent() {
        let mut term = v2_term(5, 24, 1000);
        scroll_rows(&mut term, 7);
        let (mut c, _peer) = paced_v2x_conn(&term);
        assert!(c.build_frame_from(&term));
        let frames = decode_server_frames(&c.write_buf);
        assert_eq!(sb2_epochs(&frames), vec![Some(1)]);
        assert_eq!(extents(&frames), vec![extent(1, 0, 0)]);
    }

    /// The harness is lossless, so every forward jump in a flood that
    /// outruns the window is an eviction — and the extent riding with the
    /// body must mark it (`evicted_upto` at or past where it lands).
    #[test]
    fn posh225_v2_extent_marks_every_flood_jump_as_evicted() {
        const KIB: usize = 1024;
        let flood = newline_flood(2 * KIB * KIB);
        print_flood_header();
        for prefill_rows in [0, SCROLLBACK + 200] {
            let slow = [FloodAcks::EveryNewest(1), FloodAcks::Lagged(50)].map(|acks| FloodCase {
                drain: FloodDrain::Trickle(KIB),
                ..paced_v2x_flood_case(acks, prefill_rows)
            });
            let lagged = FloodCase {
                chunk: KIB,
                ..paced_v2x_flood_case(FloodAcks::Lagged(300), prefill_rows)
            };
            for case in slow.into_iter().chain([lagged]) {
                let r = measure_flood(&flood, case);
                print_flood_row(case, &r);
                let what = format!(
                    "acks={} drain={} prefill={prefill_rows}",
                    case.acks.label(),
                    case.drain.label()
                );
                assert!(r.forward_jumps > 0, "{what}: the flood never outran the window — this proves nothing");
                assert_eq!(r.jumps_unmarked, 0, "{what}: a jump the extent did not mark as evicted");
                assert_eq!(
                    r.end_extent.map(|x| x.avail_rows),
                    Some(r.rows_scrolled),
                    "{what}: the final extent's rows"
                );
            }
        }
    }

    /// A reader that keeps up ends with nothing arriving.
    #[test]
    fn posh225_v2_extent_counts_nothing_arriving_once_caught_up() {
        const KIB: usize = 1024;
        let flood = newline_flood(256 * KIB);
        print_flood_header();
        for acks in [FloodAcks::EveryNewest(1), FloodAcks::Lagged(50), FloodAcks::Lagged(300)] {
            let case = paced_v2x_flood_case(acks, 0);
            let r = measure_flood(&flood, case);
            print_flood_row(case, &r);
            assert_eq!(r.rows_arriving, Some(0), "acks={}", acks.label());
        }
    }

    /// Without acks the window never reopens: the extent says rows are
    /// still arriving.
    #[test]
    fn posh225_v2_extent_without_acks_reports_rows_still_arriving() {
        const KIB: usize = 1024;
        let flood = newline_flood(256 * KIB);
        let case = paced_v2x_flood_case(FloodAcks::Never, 0);
        let r = measure_flood(&flood, case);
        print_flood_header();
        print_flood_row(case, &r);
        assert!(r.rows_arriving > Some(0), "rows still arriving: {:?}", r.rows_arriving);
    }
}
