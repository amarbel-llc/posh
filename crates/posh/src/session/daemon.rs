//! Per-session daemon: owns the PTY and broadcasts output to attached
//! clients over a Unix socket (zmx daemonLoop port).

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

use posh_term::{ScreenSwitch, Terminal};

use crate::overlay::{close_overlay, escape_command, Overlay};
use crate::pty::{self, PtyChild};
use crate::remote::caps;
use crate::remote::display::Snapshot;
use crate::remote::framesync::FrameProducer;
use crate::remote::sync::{base_checksum, FrameBody, ServerFrame};
use crate::session::ipc::{self, FrameBuffer, SessionInfo, Tag};
use crate::session::{self, Config};
use crate::util::{self, Error, Result};

const SCROLLBACK: usize = 10_000;

/// A `.castx` recorder writing to a boxed sink (a file, in practice). Built
/// when `$POSH_RECORD_FILE` is set (`posh --record FILE`); tees the session's
/// raw PTY output so `poshterity replay` can reproduce the screen deterministically.
type SessionRecorder = poshterity::castx::Recorder<Box<dyn Write>>;

/// Open the recording named by `$POSH_RECORD_FILE` (if any) and write its
/// header. A failure to open/write only logs and disables recording — it must
/// never stop the session from starting.
fn open_recorder(rows: u16, cols: u16) -> Option<SessionRecorder> {
    let path = std::env::var_os("POSH_RECORD_FILE")?;
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

/// Ensures the session exists, forking off a daemon when needed. Returns
/// true when a new session was created. The daemon is a double-forked
/// grandchild that never returns from this function (it exits the process).
pub fn ensure_session(cfg: &Config, name: &str, command: Option<Vec<String>>) -> Result<bool> {
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
        std::thread::sleep(std::time::Duration::from_millis(10));
        return Ok(true);
    }
    daemon_main(cfg, name, listener, command);
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
    // client advertised frame support AND the frame-emission gate is on (the
    // default; `$POSH_SESSION_FRAMES` not set to an off value). While `Some`, the
    // daemon emits posh-proto `ServerFrame`s (`Tag::Frame`) to this client instead
    // of raw `Tag::Output`; each client diffs against its OWN acked base, so a
    // freshly attached client's first frame is a `Full` while an established one
    // gets a `Diff`. `None` (gate off / non-frame client) ⇒ legacy `Tag::Output`.
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
    // enabled (attach) and again on a resize (§4: a width change reflows, so
    // counting restarts at the new width). `acked_sb_total` is the total the
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
}

impl ClientConn {
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
        let resized = match payload.get(..4).and_then(ipc::decode_resize) {
            Some((r, w)) => {
                self.rows = r;
                self.cols = w;
                true
            }
            None => false,
        };
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

    /// Whether this client advertised the posh-proto frame protocol — i.e. its
    /// `Tag::Init` carried a capability table with `CAP_PROTOCOL_VERSION`. A
    /// baseline (no-table) peer is never frame-capable, so it always receives
    /// raw `Tag::Output`.
    fn is_frame_capable(&self) -> bool {
        caps::find(&self.caps, caps::CAP_PROTOCOL_VERSION).is_some()
    }

    /// Construct this client's `FrameProducer` when the session frame-emission
    /// gate is on AND the client is frame-capable. Idempotent: a bare re-`Init`
    /// (SIGCONT resume) keeps the existing producer (and its acked base) rather
    /// than resetting it. With `gate` off, NEVER constructs a producer, so the
    /// client stays on `Tag::Output` — the Phase 1 safety invariant.
    fn maybe_enable_frames(&mut self, gate: bool) {
        if gate && self.producer.is_none() && self.is_frame_capable() {
            self.producer = Some(FrameProducer::new(self.rows.max(1), self.cols.max(1)));
        }
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
        let use_morph = lossy && caps::find(&self.caps, caps::CAP_MORPH).is_some();
        let stamp_base_sum = lossy && caps::find(&self.caps, caps::CAP_BASE_SUM).is_some();
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
                    flags: 0,
                    caps: caps::own_table(&[]),
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
    fn apply_frame_ack(&mut self, payload: &[u8]) {
        // `Tag::FrameAck` is a not-self-acked verb: a reliable client self-acks in
        // `queue_frame` and never sends it, so ignore it here — that keeps a
        // reliable client's producer state provably untouched by this path. Gated
        // on the ADVERTISED cap (`self.coalesce`, not `coalescing()`): a toggle-OFF
        // ack must still be processed to flip the runtime state back.
        if !self.lossy && !self.coalesce {
            return;
        }
        let Some((acked, flags)) = ipc::decode_frame_ack(payload) else {
            return;
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
            return;
        };
        if let Some(sb_total) = producer.ack(acked) {
            self.acked_sb_total = self.acked_sb_total.max(sb_total);
        }
        if flags & ipc::FRAME_ACK_RESYNC != 0 {
            producer.drop_acked_base();
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
        producer.advance_scrollback(cur_sb_total);
        // The rows that entered scrollback since this client's floor/ack, bounded
        // by what the ring still holds. Work in ring positions (newest-anchored):
        // `grown` rows entered since this frame's coverage and sit at the tail — 0
        // on the reliable socket, where produced == acked — so `end` is the whole
        // ring; `want` (rows since the floor/ack) is capped to what the ring still
        // holds, since evicted older rows are gone by design.
        //
        // mirror of server.rs:761-770 — keep in sync.
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
        // `base` reads the CONFIRMED visible frame (before the self-ack below),
        // exactly as server.rs builds the body.
        let body = FrameBody::Scrollback {
            base: producer.acked_num(),
            rows,
        };
        let bytes = ServerFrame {
            flags: 0,
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

/// Parses the `$POSH_SESSION_FRAMES` daemon frame-emission gate (RFC 0008 §6):
/// an **opt-out**. `0`/`false`/`off`/`no` (case-insensitive, trimmed) turn it
/// OFF; anything else — including unset/empty and any unrecognized value — leaves
/// it ON (the default since the fleet gate-flip). Kept distinct from
/// `$POSH_FRAMESYNC` (the *remote* MorphDelta codec opt-in) so the two are never
/// conflated: this gate decides whether the session daemon emits frames at all,
/// not which codec. `POSH_SESSION_FRAMES=0` restores today's raw-`Tag::Output`
/// path byte-for-byte.
fn parse_frames_gate(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// Whether this daemon emits posh-proto `ServerFrame`s (`Tag::Frame`) to
/// frame-capable clients. DEFAULT ON (opt-out): frames flow to every frame-capable
/// client unless `POSH_SESSION_FRAMES` is explicitly set off (`0`/`false`/`off`/
/// `no`), in which case no producer is ever constructed and every client receives
/// raw `Tag::Output`, byte-for-byte the legacy behavior. The local client has
/// consumed frames since Phase 2 (RFC 0008 / FDR 0011), so on-by-default is safe.
fn session_frames_enabled() -> bool {
    parse_frames_gate(std::env::var("POSH_SESSION_FRAMES").ok().as_deref())
}

/// Broadcasts a PTY-output chunk to every attached client: a posh-proto
/// `ServerFrame` (`Tag::Frame`) for each frame-capable client, the raw `bcast`
/// bytes (`Tag::Output`) for the rest. The dump/snapshot frame inputs are
/// derived once from `term` and cloned per producer — each client diffs against
/// its OWN acked base — and ONLY when at least one client is frame-capable, so a
/// session with none pays exactly today's cost and emits exactly today's
/// `Tag::Output` bytes (the gate-off invariant).
fn broadcast_output(clients: &mut [ClientConn], term: &Terminal, bcast: &[u8]) {
    let frame_inputs = clients.iter().any(|c| c.producer.is_some()).then(|| {
        (
            term.dump_vt(),
            Snapshot::from_term(term),
            term.is_alt_screen(),
            (term.rows(), term.cols()),
        )
    });
    for c in clients.iter_mut() {
        let produced = match &frame_inputs {
            Some((dump, snap, alt, dims)) => c.queue_frame(dump.clone(), snap.clone(), *alt, *dims),
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
    if clients.is_empty() {
        return QueryPolicy::Answer; // model is authoritative with no client
    }
    if clients.iter().any(|c| c.producer.is_none()) {
        return QueryPolicy::Silent; // a legacy client's real terminal answers
    }
    // All frame clients: the kitty reply is spoken only if every real terminal
    // supports it (advertised CAP_KITTY_KEYBOARD). Absence ⇒ suppress kitty.
    let all_kitty = clients
        .iter()
        .all(|c| caps::find(&c.caps, caps::CAP_KITTY_KEYBOARD).is_some());
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

fn daemon_main(
    cfg: &Config,
    name: &str,
    listener: UnixListener,
    command: Option<Vec<String>>,
) -> ! {
    util::redirect_stdio_devnull();
    let _ = util::log_init(&cfg.log_path(name));
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
    let envs = vec![
        ("POSH_SESSION".to_string(), name.to_string()),
        ("POSH_GROUP".to_string(), cfg.group.clone()),
    ];
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
        &format!("daemon started session={name} pid={}", child.pid),
    );

    let _ = listener.set_nonblocking(true);
    let _ = util::set_nonblocking(child.master);

    let mut term = Terminal::with_scrollback(rows, cols, SCROLLBACK);
    let mut clients: Vec<ClientConn> = Vec::new();
    // Join argv with NUL (not spaces) so `posh fork` can recover arguments
    // that contain spaces losslessly. github #18.
    let info_cmd = command.as_ref().map(|c| c.join("\0")).unwrap_or_default();

    // Optional `.castx` recording (posh --record FILE). Best-effort: a failure
    // to open never blocks the session.
    let recorder = open_recorder(rows, cols);

    daemon_loop(
        &listener, &child, &mut term, &mut clients, &info_cmd, &cwd, recorder,
    );

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
    // Tell attached clients the real status before hanging up (their EOF
    // is the detach notice). Best-effort: a stuck client cannot block
    // teardown. github #18.
    for c in clients.iter_mut() {
        ipc::append_frame(&mut c.write_buf, Tag::Exit, &ipc::encode_exit(code));
        let _ = util::write_all_retry(c.stream.as_raw_fd(), &c.write_buf, 100);
    }
    clients.clear();
    let _ = std::fs::remove_file(&socket_path);
    std::process::exit(code);
}

#[allow(clippy::too_many_arguments)]
fn daemon_loop(
    listener: &UnixListener,
    child: &PtyChild,
    term: &mut Terminal,
    clients: &mut Vec<ClientConn>,
    info_cmd: &str,
    cwd: &str,
    mut recorder: Option<SessionRecorder>,
) {
    let listener_fd = listener.as_raw_fd();
    let pty_fd = child.master;
    let mut has_pty_output = false;
    // Frame-emission gate (RFC 0008 §6), read once at startup: when off, no
    // client ever gets a `FrameProducer`, so every client stays on `Tag::Output`
    // (legacy behavior, byte-for-byte). Default ON (opt-out); off only when
    // `POSH_SESSION_FRAMES` is `0`/`false`/`off`/`no`.
    let frames_gate = session_frames_enabled();
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

    'daemon: loop {
        if util::take_flag(&util::SIGTERM_RECEIVED) {
            let signo = util::LAST_SIGNAL.load(std::sync::atomic::Ordering::Acquire);
            util::log_write(
                "info",
                &format!(
                    "{} received, shutting down gracefully",
                    util::signal_name(signo)
                ),
            );
            break;
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
                    &format!(
                        "client backlog high-water fd={} backlog={} drained_total={} last_drain_age_ms={}",
                        c.stream.as_raw_fd(),
                        c.write_buf.len(),
                        c.bytes_drained,
                        now.saturating_sub(c.last_drain_ms),
                    ),
                );
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
                    &format!(
                        "dropping slow client fd={} backlog={} drained_total={} last_drain_age_ms={}",
                        c.stream.as_raw_fd(),
                        c.write_buf.len(),
                        c.bytes_drained,
                        now.saturating_sub(c.last_drain_ms),
                    ),
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

        match util::poll(&mut fds, -1) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                util::log_write("error", &format!("poll failed: {e}"));
                break;
            }
        }

        // New client connections.
        if fds[0].revents & err_events != 0 {
            util::log_write("error", "server socket error");
            break;
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
                    break;
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
                    break;
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
                                }
                                Tag::Init => {
                                    if c.apply_init(&frame.payload) {
                                        resized = true;
                                    }
                                    // Enable per-client frame production for a
                                    // frame-capable client when the gate is on;
                                    // a no-op otherwise (the replay/broadcast
                                    // then stay on Tag::Output). RFC 0008.
                                    let framed_before = c.producer.is_some();
                                    c.maybe_enable_frames(frames_gate);
                                    // Forward-only scrollback (RFC 0002 §3): a
                                    // freshly framed client starts with an empty
                                    // ring, so anchor its floor at the current
                                    // total — only rows appended AFTER attach are
                                    // synced, never pre-attach history.
                                    if !framed_before && c.producer.is_some() {
                                        c.sb_floor = term.primary_scrollback_total();
                                    }
                                    // Replay the current screen so the client
                                    // sees state it missed (including the first
                                    // attach to a detached-created session). The
                                    // dump is queued after the resize below so
                                    // it reflects the new client size. github #16.
                                    needs_replay = has_pty_output;
                                }
                                Tag::Resize => {
                                    if let Some((r, w)) = ipc::decode_resize(&frame.payload) {
                                        c.rows = r;
                                        c.cols = w;
                                        resized = true;
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
                                Tag::Kill => break 'daemon,
                                Tag::Info => {
                                    let info = SessionInfo {
                                        clients: (total_clients - 1) as u64,
                                        pid: child.pid,
                                        cmd: info_cmd.to_string(),
                                        cwd: cwd.to_string(),
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
                                // A lossy relay client (RFC 0008 §3) OR a
                                // coalescing local client (CAP_COALESCE, posh#137)
                                // acking one of its `Tag::Frame`s — the base-advance
                                // a reliable client gets from the immediate self-ack;
                                // also carries the runtime coalescing toggle. Shared
                                // with the tests via `apply_frame_ack` (like
                                // `apply_init`).
                                Tag::FrameAck => c.apply_frame_ack(&frame.payload),
                                // Output, Ack, Exit, and Frame are all
                                // daemon->client only; ignore if received from
                                // a client.
                                Tag::Output | Tag::Ack | Tag::Exit | Tag::Frame => {}
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
            }
            if detach_all {
                util::log_write("info", &format!("detach all clients={}", clients.len()));
                clients.clear();
                break;
            }
            if remove {
                let fd = clients[i].stream.as_raw_fd();
                clients.remove(i);
                util::log_write(
                    "info",
                    &format!("client disconnected fd={fd} remaining={}", clients.len()),
                );
                // The smallest client may have left; grow back (zmx issue #8).
                resized = true;
            }
            if resized {
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
                // Scrollback resize reset (RFC 0002 §4): a width change reflows
                // the terminal, so restart every frame-capable client's
                // appended-row counting at the reflowed total. This is the
                // session-socket stand-in for the UDP client's one-message
                // CAP_SCROLLBACK suppression — socket caps are Init-only, so the
                // restart is handled daemon-side. The matching client drops its
                // ring on the same resize (RFC 0002 §4), so both sides go
                // forward-only from here: no reflowed rows shipped against a stale
                // floor, no mixed-width rows in the ring.
                let sb_total = term.primary_scrollback_total();
                for c in clients.iter_mut() {
                    if c.producer.is_some() {
                        c.sb_floor = sb_total;
                    }
                }
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
                // Replay the ACTIVE broadcast source: while an escape overlay is
                // up it is what every client sees (FDR 0008), so a client
                // attaching / resuming mid-overlay must base on the overlay
                // screen, not the live session underneath (see `active_source`).
                let src = active_source(overlay.as_ref().map(|o| &o.term), term);
                let c = &mut clients[i];
                // Derive the dump/snapshot frame inputs ONLY when a producer
                // exists — exactly the lazy guard `broadcast_output` uses — so a
                // gate-off or non-capable client (the Phase 1 default, hit on
                // every attach) pays only the single `dump_vt_flat` it always did.
                let frame_inputs = c.producer.is_some().then(|| {
                    (
                        src.dump_vt(),
                        Snapshot::from_term(src),
                        src.is_alt_screen(),
                        (src.rows(), src.cols()),
                    )
                });
                let produced = match frame_inputs {
                    Some((dump, snap, alt, dims)) => c.queue_frame(dump, snap, alt, dims),
                    None => false,
                };
                if !produced {
                    c.queue(Tag::Output, &src.dump_vt_flat());
                }
            }
            // Escape-to-shell (FDR 0008): a client asked to open the overlay.
            // Deferred here so the source swap can iterate every client's
            // producer without conflicting with the per-client borrow above.
            // Idempotent via the `overlay.is_none()` guard: a retransmitted
            // request while the overlay is up is a no-op.
            if open_shell && overlay.is_none() {
                let ov_cwd = if term.pwd().is_empty() {
                    cwd.to_string()
                } else {
                    term.pwd().to_string()
                };
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
    }

    // Tear down any escape overlay before the shell/session cleanup (FDR 0008).
    close_overlay(&mut overlay);

    // Flush the recording's held UTF-8 tail + buffered writer on the way out
    // (shell exit / SIGTERM / kill).
    if let Some(mut rec) = recorder {
        let _ = rec.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
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
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        c.apply_init(&init);
        c.maybe_enable_frames(true);
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
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[caps::Cap {
            id: caps::CAP_KITTY_KEYBOARD,
            payload: vec![flags],
        }])));
        c.apply_init(&init);
        c.maybe_enable_frames(true);
        (c, peer)
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
        let legacy = test_client_conn(); // no producer, no caps
        assert_eq!(query_policy(std::slice::from_ref(&legacy)), QueryPolicy::Silent);
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
        let legacy = test_client_conn();
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
        let mut fb = FrameBuffer::new();
        fb.feed(write_buf);
        let mut term = Terminal::with_scrollback(rows, cols, 0);
        term.process(base_dump);
        let mut applier = DumpDiff;
        let mut applied: Vec<u8> = base_dump.to_vec();
        while let Some(frame) = fb.next().unwrap() {
            let body = ServerFrame::decode(&frame.payload).unwrap().body;
            match applier.apply(rows, cols, &applied, &mut term, &body) {
                ApplyOutcome::Advanced { dump } => applied = dump,
                ApplyOutcome::AdvancedNoDump | ApplyOutcome::NoChange => {}
                ApplyOutcome::ReackAndWait => panic!("DumpDiff could not apply a queued body"),
            }
        }
        Snapshot::from_term(&term)
    }

    #[test]
    fn frames_gate_defaults_on_and_parses_falsey() {
        // Default ON (opt-out): unset/empty and any unrecognized value leave it on.
        assert!(parse_frames_gate(None));
        assert!(parse_frames_gate(Some("")));
        assert!(parse_frames_gate(Some("1")));
        assert!(parse_frames_gate(Some("on")));
        assert!(parse_frames_gate(Some("true")));
        // `morph` is the POSH_FRAMESYNC value, NOT this gate — it is not an off
        // spelling, so it leaves the frame gate on (unrecognized ⇒ default).
        assert!(parse_frames_gate(Some("morph")));
        // Falsey spellings (case-insensitive, trimmed) turn it OFF.
        for off in ["0", "false", "off", "no", "  FALSE  ", "Off"] {
            assert!(
                !parse_frames_gate(Some(off)),
                "{off:?} should disable the gate"
            );
        }
    }

    #[test]
    fn producer_constructed_only_when_gated_and_capable() {
        // Gate on + capable => producer.
        let (capable, _p) = frame_capable_conn(24, 80);
        assert!(capable.producer.is_some(), "gate on + cap table => producer");

        // Gate off + capable => none (the Phase 1 safety invariant).
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut gate_off = ClientConn {
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
        };
        let mut init = ipc::encode_resize(24, 80).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        gate_off.apply_init(&init);
        gate_off.maybe_enable_frames(false);
        assert!(gate_off.producer.is_none(), "gate off must not construct a producer");

        // Gate on + NOT capable (bare Init) => none.
        let mut baseline = test_client_conn();
        baseline.apply_init(&ipc::encode_resize(24, 80));
        baseline.maybe_enable_frames(true);
        assert!(baseline.producer.is_none(), "a non-capable client never gets a producer");
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

        c.maybe_enable_frames(true);

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
    fn gate_off_emits_output_for_every_client() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 100);
        term.process(b"content");

        // A cap-advertising client, but the gate is OFF => no producer.
        let (stream, _peer) = UnixStream::pair().unwrap();
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
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
        c.apply_init(&init);
        c.maybe_enable_frames(false);
        assert!(c.producer.is_none());

        let raw = b"raw broadcast bytes";
        broadcast_output(std::slice::from_mut(&mut c), &term, raw);

        let mut fb = FrameBuffer::new();
        fb.feed(&c.write_buf);
        let frame = fb.next().unwrap().expect("one queued record");
        assert_eq!(frame.tag, Tag::Output, "gate off => Tag::Output");
        assert_eq!(frame.payload, raw, "gate off => the raw broadcast bytes, unchanged");
        assert!(fb.next().unwrap().is_none(), "exactly one Output record");
    }

    #[test]
    fn non_capable_client_gets_output_even_with_gate_on() {
        let (rows, cols) = (24u16, 80u16);
        let mut term = Terminal::with_scrollback(rows, cols, 100);
        term.process(b"content");

        // No cap table in the Init => baseline peer; gate ON.
        let mut c = test_client_conn();
        c.apply_init(&ipc::encode_resize(rows, cols));
        c.maybe_enable_frames(true);
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
        baseline.maybe_enable_frames(true);
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

    /// The four-way socket version-skew matrix of RFC 0008 §6: the daemon's
    /// negotiation degrades cleanly across daemon/client versions without a flag
    /// day. "old daemon" is modelled by the `$POSH_SESSION_FRAMES` gate being
    /// OFF (the daemon's newness — gate off ⇒ it never constructs a producer, so
    /// every client gets raw `Tag::Output`); "old client" by a bare 4-byte Init
    /// with no capability table.
    ///
    /// | daemon (gate) | client (Init)        | screen output |
    /// |---------------|----------------------|---------------|
    /// | new (on)      | new (caps)           | `Tag::Frame`  |
    /// | new (on)      | old (bare)           | `Tag::Output` |
    /// | old (off)     | new (caps + Resize)  | `Tag::Output` |
    /// | old (off)     | old (bare)           | `Tag::Output` |
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
            c.maybe_enable_frames(true);
            assert!(c.producer.is_none(), "cell 2: no cap table ⇒ no producer even with gate on");
            broadcast_output(std::slice::from_mut(&mut c), &term, raw);
            assert_single_output(&c.write_buf, raw);
        }

        // Cell 3 (the critical cross-version cell) — old daemon (gate OFF) × new
        // client (cap-extended Init + the Tag::Resize re-assertion) ⇒ Tag::Output,
        // AND the size the new client conveys is recoverable on an old daemon.
        {
            let mut c = test_client_conn();
            let cap_extended_init = {
                let mut init = ipc::encode_resize(rows, cols).to_vec();
                init.extend_from_slice(&caps::encode_table(&caps::own_table(&[])));
                init
            };
            c.apply_init(&cap_extended_init);
            c.maybe_enable_frames(false); // gate OFF ⇒ "old daemon" ⇒ no frames
            assert!(c.producer.is_none(), "cell 3: gate off ⇒ no producer regardless of caps");

            broadcast_output(std::slice::from_mut(&mut c), &term, raw);
            assert_single_output(&c.write_buf, raw);

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

        // Cell 4 — old daemon (gate OFF) × old client (bare Init) ⇒ Tag::Output.
        // The unchanged baseline: neither side negotiates anything new.
        {
            let mut c = test_client_conn();
            c.apply_init(&ipc::encode_resize(rows, cols));
            c.maybe_enable_frames(false);
            assert!(c.producer.is_none(), "cell 4: gate off + no caps ⇒ no producer");
            broadcast_output(std::slice::from_mut(&mut c), &term, raw);
            assert_single_output(&c.write_buf, raw);
        }
    }

    // ---- Task 2.5a: daemon produces scrollback frames (RFC 0002) ----

    /// A frame-capable client that ALSO advertises `CAP_SCROLLBACK` (RFC 0002
    /// §1), so with the gate on it both frames the screen AND wants scrolled-off
    /// rows synced. `gate` off models an "old daemon" (no producer at all).
    fn scrollback_capable_conn(
        rows: u16,
        cols: u16,
        gate: bool,
    ) -> (ClientConn, UnixStream) {
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
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[caps::Cap {
            id: caps::CAP_SCROLLBACK,
            payload: vec![0],
        }])));
        c.apply_init(&init);
        c.maybe_enable_frames(gate);
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

        let (mut c, _peer) = scrollback_capable_conn(rows, cols, true);
        assert!(c.producer.is_some(), "gate on + caps ⇒ producer");
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

    /// Gate OFF ⇒ no producer ⇒ the client stays on `Tag::Output`, so no
    /// scrollback frame is ever emitted even for a scrollback-capable client that
    /// scrolls heavily. The gate-off invariant extends to scrollback unchanged.
    #[test]
    fn gate_off_emits_no_scrollback_frames() {
        let (rows, cols) = (5u16, 24u16);
        let mut term = Terminal::with_scrollback(rows, cols, 1000);

        let (mut c, _peer) = scrollback_capable_conn(rows, cols, false);
        assert!(c.producer.is_none(), "gate off ⇒ no producer regardless of caps");

        scroll_off(&mut term, 12);
        let raw = b"raw broadcast bytes";
        broadcast_output(std::slice::from_mut(&mut c), &term, raw);

        // Every queued record is a raw Tag::Output — never a Tag::Frame.
        let mut fb = FrameBuffer::new();
        fb.feed(&c.write_buf);
        let mut records = 0;
        while let Some(frame) = fb.next().unwrap() {
            assert_eq!(frame.tag, Tag::Output, "gate off must never emit Tag::Frame");
            assert_eq!(frame.payload, raw, "the raw broadcast bytes, unchanged");
            records += 1;
        }
        assert_eq!(records, 1, "exactly one Tag::Output record");
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
        };
        let mut table = vec![caps::Cap {
            id: caps::CAP_LOSSY,
            payload: vec![],
        }];
        table.extend_from_slice(extra);
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&table)));
        c.apply_init(&init);
        c.maybe_enable_frames(true);
        (c, peer)
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
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[caps::Cap {
            id: caps::CAP_COALESCE,
            payload: vec![],
        }])));
        c.apply_init(&init);
        c.maybe_enable_frames(true);
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
        };
        let mut init = ipc::encode_resize(rows, cols).to_vec();
        init.extend_from_slice(&caps::encode_table(&caps::own_table(&[
            caps::Cap { id: caps::CAP_COALESCE, payload: vec![] },
            caps::Cap { id: caps::CAP_SCROLLBACK, payload: vec![0] },
        ])));
        c.apply_init(&init);
        c.maybe_enable_frames(true);
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
}
