//! Thin frame relay (RFC 0008 §3 / FDR 0012 Phase 3): a frame-capable CLIENT of
//! the session daemon's Unix socket that forwards the daemon's `ServerFrame`
//! stream to a remote roaming client over the AEAD-UDP transport, bridging the
//! UDP client's input back to the daemon as `Tag::Input`.
//!
//! # Single-model invariant
//!
//! Unlike the legacy `remote::server` (FDR 0001 Architecture A, which owns a
//! second `posh_term::Terminal` + `FrameProducer` and re-models the inner
//! session), the relay owns NO terminal model. The daemon is the single frame
//! producer; local and remote clients consume the same frames from it. This
//! module's frames path therefore constructs no `posh_term::Terminal`,
//! `FrameProducer`, `Overlay`, `ScreenSwitchFilter`, or wedge-watchdog — that
//! absence IS the single-model assertion. (`posh_term::Terminal` appears only in
//! `#[cfg(test)]`, as the daemon/client MODELS the test drives around the relay.)
//!
//! # Scope — through Task 3.1b (loss / roam / resync recovery)
//!
//! The relay holds a single-frame O(1) retransmit buffer ([`HeldFrame`]): the
//! latest re-wrapped unacked frame, and nothing more. Because the daemon runs
//! this client's `FrameProducer` in lossy, ack-gated mode (`CAP_LOSSY`, Task 3.0),
//! every unacked frame anchors at the client's last *acked* base, so each new
//! frame SUPERSEDES the previous one — retransmitting the newest brings the client
//! fully current. The relay therefore never retains more than ONE frame, even
//! across a roam (the UDP peer silent while the screen keeps changing — each new
//! daemon frame just replaces the held one). It:
//!
//! - retransmits the held frame on the RTO (`conn.rto()`), gated on a reachable
//!   peer, and drops it when the client's cumulative `acked_frame` reaches it;
//! - heartbeats an Empty frame on a static screen (`HEARTBEAT_INTERVAL`) so the
//!   client keeps hearing input acks + peer-liveness when the screen is idle;
//! - forwards a client base-sum divergence (`CLIENT_FLAG_RESYNC`) as a
//!   `Tag::FrameAck` with `FRAME_ACK_RESYNC`, so the daemon drops its base and its
//!   next frame is a recovering `Full`, and discards the diverged held frame.
//!
//! Agent-cap termination is 3.2; the `cmd_server` relay verb + bootstrap is 3.3.
//!
//! Task 3.3 wires `cmd_server`'s `relay` verb as the non-test caller of [`run`];
//! until then the surface here has only the inline-test caller (mirroring
//! `session::ipc::encode_frame_ack`), hence the module-level `dead_code` allow.
#![allow(dead_code)]

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use crate::remote::caps::{self, Cap};
use crate::remote::datagram::Connection;
use crate::remote::server::send_payload;
use crate::remote::sync::{
    self, ClientMessage, FragmentAssembly, Fragmenter, FrameBody, InputInbox, ServerFrame,
    FLAG_SHUTDOWN,
};
use crate::session::ipc::{self, FrameBuffer, Tag};
use crate::session::{self, Config};
use crate::util::{self, Result};

/// The daemon connection, held as a replaceable FIELD rather than a construction
/// invariant (RFC 0008 §3 / FDR 0012 "retarget-readiness"). A retarget — NOT
/// built in Phase 3 — would drop `link`, `connect_or_create` a new daemon socket,
/// re-Init, and bump `frame_offset` so the new daemon's `frame_num = 1` Full is
/// not rejected as stale by the UDP client (`framereplay.rs`:
/// `if frame.frame_num < applied_num`). `frame_offset` is 0 throughout Phase 3.
struct DaemonLink {
    stream: UnixStream,
    read: FrameBuffer,
    write: Vec<u8>,
    frame_offset: u64,
}

/// The client's CONTENT caps to forward into the daemon Init: MORPH / SCROLLBACK
/// / BASE_SUM (RFC 0008 §4). The agent caps (6/7/8) are relay-TERMINATED (Task
/// 3.2) and MUST NOT reach the daemon; CAP_DIAG/CAP_METRICS are answered by the
/// relay from its own transport state — so neither category is forwarded here.
fn content_caps(client_caps: &[Cap]) -> Vec<Cap> {
    [caps::CAP_MORPH, caps::CAP_SCROLLBACK, caps::CAP_BASE_SUM]
        .iter()
        .filter_map(|&id| caps::find(client_caps, id).cloned())
        .collect()
}

/// The daemon `Tag::Init` payload: the 4-byte resize prefix then the RFC 0001
/// table `own_table(content_caps ++ CAP_LOSSY)`. `CAP_LOSSY` opts the daemon's
/// per-client `FrameProducer` into ack-gated, base-anchored mode (no self-ack;
/// RFC 0008 §3), so the base advances only on a forwarded `Tag::FrameAck`.
fn init_payload(rows: u16, cols: u16, content: &[Cap]) -> Vec<u8> {
    let mut table = content.to_vec();
    table.push(Cap {
        id: caps::CAP_LOSSY,
        payload: vec![],
    });
    let mut payload = ipc::encode_resize(rows, cols).to_vec();
    payload.extend_from_slice(&caps::encode_table(&caps::own_table(&table)));
    payload
}

/// Re-wrap a daemon frame's transport header for the UDP client: the daemon's
/// body, flags, and content caps are forwarded verbatim (the daemon is the frame
/// producer), but the frame number carries the retarget `frame_offset` seam and
/// the input/echo acks are the relay's own (the daemon sends 0). RFC 0008 §3.
fn rewrap(daemon: ServerFrame, frame_offset: u64, input_ack: u64, echo_ack: u64) -> ServerFrame {
    ServerFrame {
        flags: daemon.flags,
        caps: daemon.caps,
        frame_num: daemon.frame_num + frame_offset,
        input_ack,
        echo_ack,
        body: daemon.body,
    }
}

/// The relay's O(1) retransmit buffer (RFC 0008 §3): at most ONE unacked frame,
/// the latest re-wrapped `ServerFrame`. The daemon anchors every unacked frame at
/// the client's last *acked* base (lossy mode, Task 3.0), so a new frame
/// SUPERSEDES the previous one — retransmitting the newest is enough to bring the
/// client fully current. The relay thus never accumulates a diff chain:
/// [`hold`](HeldFrame::hold) replaces, [`drop_if_acked`](HeldFrame::drop_if_acked)
/// releases on the client's cumulative ack, and [`clear`](HeldFrame::clear)
/// discards a diverged frame on RESYNC. This single-frame bound is the whole point
/// of Model 2 — the alternative (relay-owned reliability) must buffer and
/// retransmit the entire unacked chain, which grows unboundedly on roam.
#[derive(Default)]
struct HeldFrame {
    /// The one unacked frame: (re-wrapped `frame_num`, encoded `ServerFrame` bytes
    /// ready to (re)send). `None` when nothing is outstanding.
    frame: Option<(u64, Vec<u8>)>,
}

impl HeldFrame {
    /// Supersede any previously-held frame with the newest one. O(1): this
    /// replaces, it never appends — the supersession invariant makes one enough.
    fn hold(&mut self, frame_num: u64, encoded: Vec<u8>) {
        self.frame = Some((frame_num, encoded));
    }

    /// Release the held frame once the client's cumulative `acked_frame` reaches
    /// it: the client has it, so there is nothing left to retransmit. Idempotent
    /// (a no-op when nothing is held or the ack is still behind).
    fn drop_if_acked(&mut self, acked_frame: u64) {
        if self.frame.as_ref().is_some_and(|(num, _)| acked_frame >= *num) {
            self.frame = None;
        }
    }

    /// Discard the held frame unconditionally: on a base-sum divergence
    /// (`CLIENT_FLAG_RESYNC`) it diffs against a base the client rejected, so the
    /// daemon's forced `Full` — not this frame — re-establishes the client.
    fn clear(&mut self) {
        self.frame = None;
    }

    fn is_held(&self) -> bool {
        self.frame.is_some()
    }

    /// The encoded bytes of the held frame, for a (re)send.
    fn bytes(&self) -> Option<&[u8]> {
        self.frame.as_ref().map(|(_, bytes)| bytes.as_slice())
    }

    /// The held frame's re-wrapped number (test/observability).
    fn frame_num(&self) -> Option<u64> {
        self.frame.as_ref().map(|(num, _)| *num)
    }
}

/// Run the frame relay: handshake with the UDP client, connect to (or create)
/// the session daemon as a lossy frame client, then forward frames until the
/// daemon exits or the client quits. The `Connection` is a bound server socket
/// with no peer yet; the handshake learns it. `command` seeds a freshly created
/// session's shell.
pub(crate) fn run(
    mut conn: Connection,
    cfg: &Config,
    name: &str,
    command: Option<Vec<String>>,
) -> Result<()> {
    // 1. Handshake: learn the UDP client's terminal size + advertised caps from
    //    its first datagram BEFORE connecting to the daemon, so the daemon Init
    //    carries the right size and forwarded content caps (RFC 0008 §3). The
    //    datagram also teaches `conn` its peer address, so later sends land.
    let (rows, cols, client_caps) = wait_for_handshake(&mut conn)?;

    // 2. Connect to / create the session and Init as a LOSSY, frame-capable
    //    client (mirror session/client.rs: cap-extended Init + a Tag::Resize
    //    re-assert, since a strict daemon drops a non-4-byte Init's size).
    let stream = session::connect_or_create(cfg, name, command)?;
    stream.set_nonblocking(true)?;
    let mut link = DaemonLink {
        stream,
        read: FrameBuffer::new(),
        write: Vec::new(),
        frame_offset: 0,
    };
    let content = content_caps(&client_caps);
    ipc::append_frame(
        &mut link.write,
        Tag::Init,
        &init_payload(rows, cols, &content),
    );
    ipc::append_frame(
        &mut link.write,
        Tag::Resize,
        &ipc::encode_resize(rows, cols),
    );

    relay_loop(conn, link, (rows, cols))
}

/// Block (polling) on the UDP socket until the first authentic, sized
/// `ClientMessage` arrives; return its size + advertised caps. The datagram also
/// pins the server `Connection`'s peer address as a side effect of `recv`.
fn wait_for_handshake(conn: &mut Connection) -> Result<(u16, u16, Vec<Cap>)> {
    let mut assembly = FragmentAssembly::new();
    loop {
        let mut fds = [util::pollfd(conn.raw_fd(), libc::POLLIN)];
        match util::poll(&mut fds, 1000) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        loop {
            match conn.recv() {
                Ok(Some(payload)) => {
                    let Ok(frag) = sync::Fragment::from_bytes(&payload) else {
                        continue;
                    };
                    let Some(assembled) = assembly.add(frag) else {
                        continue;
                    };
                    let Ok(msg) = ClientMessage::decode(&assembled) else {
                        continue;
                    };
                    if msg.rows > 0 && msg.cols > 0 {
                        return Ok((msg.rows, msg.cols, msg.caps));
                    }
                }
                Ok(None) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// The relay poll loop (a substituted `server_loop`: the daemon socket fd
/// replaces the PTY fd, and there is no `Terminal`/`FrameProducer`/`Overlay`).
/// Forwards the daemon `ServerFrame` stream out to the UDP client and the client's
/// input/resize/frame-acks back to the daemon.
fn relay_loop(mut conn: Connection, mut link: DaemonLink, mut client_size: (u16, u16)) -> Result<()> {
    let mut fragmenter = Fragmenter::new();
    let mut assembly = FragmentAssembly::new();
    let mut inbox = InputInbox::new();
    // Highest UDP-client `acked_frame` already forwarded to the daemon as a
    // `Tag::FrameAck`, so the daemon advances its diff base (RFC 0008 §3).
    let mut acked_forwarded = 0u64;
    // The last frame number forwarded to the UDP client, reused as the number of
    // the final `FLAG_SHUTDOWN` frame and the heartbeat Empty frame (an Empty body
    // advances no apply state).
    let mut last_frame_num = 0u64;
    // The single held unacked frame (O(1) retransmit buffer, RFC 0008 §3) and the
    // ms clock of its last (re)send — also the heartbeat's last-send clock,
    // exactly as `server.rs` shares one `last_send` for retransmit + heartbeat.
    let mut held = HeldFrame::default();
    let mut last_send = 0u64;
    let err_events = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;

    loop {
        // Wake in time to retransmit the held frame (on the RTO) or heartbeat (on
        // HEARTBEAT_INTERVAL) even with no fd activity — mirrors `server.rs`'s
        // select deadline. This `now` only sizes the timeout; the send decisions
        // below re-read the clock post-poll.
        let now = util::now_ms();
        let timeout = if conn.has_remote() {
            let mut deadline = last_send + sync::HEARTBEAT_INTERVAL;
            if held.is_held() {
                deadline = deadline.min(last_send + conn.rto());
            }
            deadline.saturating_sub(now).min(1000) as i32
        } else {
            // Defensive only: `has_remote()` is pinned at first contact (during
            // the handshake `recv`) and never cleared — mosh-style, the peer's
            // address is remembered across silence, so a roamed peer still reads
            // as reachable and is recovered by resending to that last-known
            // address on the RTO (the `roam` test exercises exactly this via
            // dropped datagrams, not a false `has_remote()`). This branch is thus
            // effectively unreachable after the handshake; the 1000ms cap just
            // keeps the loop cycling if it ever is.
            1000
        };

        let mut fds = vec![util::pollfd(conn.raw_fd(), libc::POLLIN)];
        let mut link_events = libc::POLLIN;
        if !link.write.is_empty() {
            link_events |= libc::POLLOUT;
        }
        fds.push(util::pollfd(link.stream.as_raw_fd(), link_events));

        match util::poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        let now = util::now_ms();

        // --- UDP client -> daemon ---
        let mut winding_down = false;
        if fds[0].revents & (libc::POLLIN | err_events) != 0 {
            loop {
                match conn.recv() {
                    Ok(Some(payload)) => {
                        let Ok(frag) = sync::Fragment::from_bytes(&payload) else {
                            continue;
                        };
                        let Some(assembled) = assembly.add(frag) else {
                            continue;
                        };
                        let Ok(msg) = ClientMessage::decode(&assembled) else {
                            continue;
                        };
                        // Resize -> Tag::Resize (mirror handle_client_message).
                        if msg.rows > 0 && msg.cols > 0 && (msg.rows, msg.cols) != client_size {
                            client_size = (msg.rows, msg.cols);
                            ipc::append_frame(
                                &mut link.write,
                                Tag::Resize,
                                &ipc::encode_resize(msg.rows, msg.cols),
                            );
                        }
                        // New input bytes -> Tag::Input. Idempotent under the
                        // cumulative retransmit stream via InputInbox.
                        if let Some(new_input) = inbox.accept(msg.input_base, &msg.input) {
                            ipc::append_frame(&mut link.write, Tag::Input, new_input);
                        }
                        // Drop the held frame once the client confirms it via the
                        // cumulative ack: it has the frame, nothing to retransmit.
                        held.drop_if_acked(msg.acked_frame);
                        // Frame-ack / resync -> Tag::FrameAck so the daemon moves
                        // its diff base. CLIENT_FLAG_RESYNC (a client base-sum
                        // divergence) additionally sets FRAME_ACK_RESYNC so the
                        // daemon drops its base and its next frame is a recovering
                        // Full; the held frame diverged, so discard it too.
                        let resync = msg.flags & sync::CLIENT_FLAG_RESYNC != 0;
                        if msg.acked_frame > acked_forwarded || resync {
                            acked_forwarded = acked_forwarded.max(msg.acked_frame);
                            let ack_flags = if resync { ipc::FRAME_ACK_RESYNC } else { 0 };
                            ipc::append_frame(
                                &mut link.write,
                                Tag::FrameAck,
                                &ipc::encode_frame_ack(
                                    acked_forwarded - link.frame_offset,
                                    ack_flags,
                                ),
                            );
                            if resync {
                                held.clear();
                            }
                        }
                        // Client quit (mosh Ctrl-^ .): FDR 0011 durable sessions
                        // -> Tag::Detach (leave the session running); explicit
                        // kill stays a palette-only action. Then wind the relay
                        // down.
                        if msg.flags & sync::CLIENT_FLAG_SHUTDOWN != 0 {
                            ipc::append_frame(&mut link.write, Tag::Detach, b"");
                            winding_down = true;
                        }
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }

        // --- daemon -> UDP client ---
        if fds[1].revents & libc::POLLIN != 0 {
            match link.read.read_from(link.stream.as_raw_fd()) {
                Ok(0) => return Ok(()), // daemon closed the socket
                Ok(_) => loop {
                    match link.read.next() {
                        Ok(Some(frame)) => match frame.tag {
                            Tag::Frame => {
                                let daemon_frame = ServerFrame::decode(&frame.payload)?;
                                // input_ack is the relay's own received-input
                                // offset (the daemon sends 0). echo_ack mirrors it:
                                // TODO(3.1b): proper EchoAck maturity (mosh
                                // ECHO_TIMEOUT). The happy path has no
                                // optimistic-echo client, so acking received input
                                // as echoed is inert here.
                                let ack = inbox.next_offset();
                                let out = rewrap(daemon_frame, link.frame_offset, ack, ack);
                                last_frame_num = out.frame_num;
                                // Supersede the held frame (O(1)): the daemon
                                // anchored this frame at the client's acked base,
                                // so it fully replaces any prior unacked one.
                                held.hold(out.frame_num, out.encode());
                                // Send now when the peer is reachable; while it is
                                // roamed away we keep holding (each new frame just
                                // replaces) and the retransmit tick delivers the
                                // latest once the peer returns.
                                if conn.has_remote() {
                                    if let Some(bytes) = held.bytes() {
                                        send_payload(&mut conn, &mut fragmenter, bytes);
                                    }
                                    last_send = now;
                                }
                            }
                            Tag::Exit => {
                                // Session over: tell the UDP client and wind down.
                                let code = ipc::decode_exit(&frame.payload);
                                send_shutdown(
                                    &mut conn,
                                    &mut fragmenter,
                                    &inbox,
                                    last_frame_num,
                                    code,
                                );
                                return Ok(());
                            }
                            // A frames-OFF daemon serves raw Tag::Output; the
                            // runtime fallback to the legacy server is Task 3.3.
                            // The 3.1a test always runs a frames-on daemon.
                            Tag::Output => util::log_write(
                                "warn",
                                "relay received Tag::Output from a frames-off daemon \
                                 (3.3 legacy fallback not wired)",
                            ),
                            _ => {}
                        },
                        Ok(None) => break,
                        Err(e) => return Err(e),
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionReset
                        || e.kind() == std::io::ErrorKind::BrokenPipe =>
                {
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Flush queued writes toward the daemon.
        if fds[1].revents & libc::POLLOUT != 0 && !link.write.is_empty() {
            match (&link.stream).write(&link.write) {
                Ok(n) => {
                    link.write.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionReset
                        || e.kind() == std::io::ErrorKind::BrokenPipe =>
                {
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Retransmit the one held (unacked) frame on the RTO, or — when nothing is
        // held — heartbeat an Empty frame so a static screen still advances the
        // client's input ack and signals peer-liveness. Both gate on a reachable
        // peer and restamp `last_send`; a fresh daemon frame this iteration already
        // stamped `last_send = now`, so neither re-fires. Mirrors `server.rs`'s
        // retransmit / send_empty gates (one shared `last_send`).
        if conn.has_remote() {
            if held.is_held() {
                if now.saturating_sub(last_send) >= conn.rto() {
                    if let Some(bytes) = held.bytes() {
                        send_payload(&mut conn, &mut fragmenter, bytes);
                    }
                    last_send = now;
                }
            } else if now.saturating_sub(last_send) >= sync::HEARTBEAT_INTERVAL {
                send_empty(&mut conn, &mut fragmenter, &inbox, last_frame_num);
                last_send = now;
            }
        }

        if winding_down {
            // Push the queued Tag::Detach to the daemon (best-effort), then tell
            // the UDP client the transport is over.
            let _ = util::write_all_retry(link.stream.as_raw_fd(), &link.write, 100);
            send_shutdown(&mut conn, &mut fragmenter, &inbox, last_frame_num, None);
            return Ok(());
        }

        if fds[1].revents & err_events != 0 {
            return Ok(());
        }
    }
}

/// Send the UDP client an Empty heartbeat frame carrying the current input acks
/// (RFC 0008 §3): on a static screen it advances the client's `input_ack` and
/// signals peer-liveness when no visible frame is flowing. Reuses `frame_num`
/// (the last forwarded number) — an Empty body advances no apply state, so the
/// client reads the fresher acks without disturbing its screen. Mirrors
/// `server.rs`'s heartbeat empty-frame path. The caller has already confirmed a
/// reachable peer.
fn send_empty(
    conn: &mut Connection,
    fragmenter: &mut Fragmenter,
    inbox: &InputInbox,
    frame_num: u64,
) {
    let ack = inbox.next_offset();
    let frame = ServerFrame {
        flags: 0,
        caps: caps::own_table(&[]),
        frame_num,
        input_ack: ack,
        echo_ack: ack,
        body: FrameBody::Empty,
    };
    send_payload(conn, fragmenter, &frame.encode());
}

/// Send the UDP client a final `FLAG_SHUTDOWN` frame (Empty body), carrying the
/// exit-status cap when the daemon reported one. Reuses `frame_num` (the last
/// forwarded number) — an Empty body advances no apply state, so the client
/// accepts it as the quit signal.
fn send_shutdown(
    conn: &mut Connection,
    fragmenter: &mut Fragmenter,
    inbox: &InputInbox,
    frame_num: u64,
    exit_code: Option<i32>,
) {
    if !conn.has_remote() {
        return;
    }
    let extras: Vec<Cap> = match exit_code {
        Some(code) => vec![Cap {
            id: caps::CAP_EXIT_STATUS,
            payload: vec![code.clamp(0, 255) as u8],
        }],
        None => Vec::new(),
    };
    let ack = inbox.next_offset();
    let frame = ServerFrame {
        flags: FLAG_SHUTDOWN,
        caps: caps::own_table(&extras),
        frame_num,
        input_ack: ack,
        echo_ack: ack,
        body: FrameBody::Empty,
    };
    send_payload(conn, fragmenter, &frame.encode());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use posh_term::Terminal;

    use crate::remote::caps::{
        CAP_AGENT_FORWARD, CAP_LOSSY, CAP_MORPH, CAP_PROTOCOL_VERSION, CAP_SCROLLBACK,
    };
    use crate::remote::crypto::Key;
    use crate::remote::datagram::Family;
    use crate::remote::display::Snapshot;
    use crate::remote::framesync::{ApplyOutcome, FrameApplier, FrameProducer, FrameSync};
    use crate::remote::sync::InputOutbox;
    use crate::util::now_ms;

    // ---- pure re-wrap / cap-handshake helpers ------------------------------

    #[test]
    fn content_caps_forwards_content_and_drops_agent_caps() {
        let advertised = vec![
            Cap {
                id: CAP_SCROLLBACK,
                payload: vec![0],
            },
            Cap {
                id: CAP_AGENT_FORWARD, // relay-terminated (3.2): must NOT forward
                payload: vec![],
            },
            Cap {
                id: CAP_MORPH,
                payload: vec![],
            },
        ];
        let content = content_caps(&advertised);
        assert!(caps::find(&content, CAP_SCROLLBACK).is_some());
        assert!(caps::find(&content, CAP_MORPH).is_some());
        assert!(
            caps::find(&content, CAP_AGENT_FORWARD).is_none(),
            "agent caps are relay-terminated, never forwarded to the daemon"
        );
    }

    #[test]
    fn init_payload_advertises_lossy_size_and_content() {
        let content = content_caps(&[Cap {
            id: CAP_SCROLLBACK,
            payload: vec![0],
        }]);
        let payload = init_payload(30, 100, &content);
        assert_eq!(ipc::decode_resize(&payload[..4]).unwrap(), (30, 100));
        let (table, _) = caps::decode_table(&payload[4..]).unwrap();
        assert!(
            caps::find(&table, CAP_LOSSY).is_some(),
            "the relay must opt the daemon client into lossy mode"
        );
        assert!(caps::find(&table, CAP_PROTOCOL_VERSION).is_some());
        assert!(caps::find(&table, CAP_SCROLLBACK).is_some());
    }

    #[test]
    fn rewrap_forwards_body_and_rewrites_header() {
        let daemon = ServerFrame {
            flags: FLAG_SHUTDOWN,
            caps: caps::own_table(&[]),
            frame_num: 5,
            input_ack: 0,
            echo_ack: 0,
            body: FrameBody::Full(b"dump".to_vec()),
        };
        let out = rewrap(daemon.clone(), 0, 7, 7);
        assert_eq!(out.frame_num, 5, "phase-3 offset is 0");
        assert_eq!(out.input_ack, 7, "the relay stamps its own input ack");
        assert_eq!(out.echo_ack, 7);
        assert_eq!(out.flags, daemon.flags, "daemon flags forwarded verbatim");
        assert_eq!(out.body, daemon.body, "the daemon body is forwarded as-is");
        // The retarget seam: a swapped daemon's frame_num is offset so the
        // client does not reject a restarted frame_num=1 as stale (FDR 0012).
        assert_eq!(rewrap(daemon, 100, 0, 0).frame_num, 105);
    }

    /// The O(1) supersession invariant (RFC 0008 §3), proven deterministically on
    /// the real buffer the loop uses: `hold` REPLACES (never accumulates), an ack
    /// below the held number keeps it, an ack at/above releases it, and `clear`
    /// (RESYNC) discards unconditionally. This is the authoritative "at most one
    /// held frame" assertion; the roam integration test below shows the same bound
    /// end-to-end (a silent peer + a changing screen never grows the buffer).
    #[test]
    fn held_frame_is_o1_supersede_drop_clear() {
        let mut held = HeldFrame::default();
        assert!(!held.is_held());

        held.hold(1, vec![0xaa]);
        assert_eq!(held.frame_num(), Some(1));

        // A newer daemon frame SUPERSEDES the older one — still exactly one held.
        held.hold(2, vec![0xbb]);
        assert_eq!(held.frame_num(), Some(2));
        assert_eq!(held.bytes(), Some(&b"\xbb"[..]));

        // A cumulative ack BELOW the held number keeps it (not yet confirmed).
        held.drop_if_acked(1);
        assert_eq!(held.frame_num(), Some(2), "unconfirmed frame stays held");

        // An ack AT or ABOVE the held number releases it (client has it).
        held.drop_if_acked(5);
        assert!(!held.is_held(), "confirmed frame is dropped");

        // clear() (the RESYNC path) discards the diverged frame unconditionally.
        held.hold(9, vec![0xcc]);
        held.clear();
        assert!(!held.is_held(), "RESYNC clears the held frame");
    }

    // ---- integration: real relay_loop + synthetic daemon + synthetic client -

    /// Fill a screen so a later small append diffs as a clear win (a `Diff`, not
    /// a `Full`) — the same fixture the daemon/client frame tests use.
    fn fill_screen(term: &mut Terminal) {
        term.process(b"\x1b[2J\x1b[H");
        for i in 0..18u8 {
            term.process(format!("relay line {i:02} content\r\n").as_bytes());
        }
    }

    /// Encode one lossy-daemon visible frame (NOT self-acked) and write it as a
    /// `Tag::Frame` record onto the daemon end of the socket. Mirrors the real
    /// daemon's `queue_frame` lossy branch (`session::daemon`).
    fn write_daemon_frame(daemon_end: &UnixStream, prod: &mut FrameProducer, term: &Terminal) {
        prod.advance_visible(
            term.dump_vt(),
            Snapshot::from_term(term),
            term.is_alt_screen(),
            (term.rows(), term.cols()),
            0,
        );
        let body = prod.encode_visible(false);
        let frame_num = prod.current_num();
        let bytes = ServerFrame {
            flags: 0,
            caps: caps::own_table(&[]),
            frame_num,
            input_ack: 0,
            echo_ack: 0,
            body,
        }
        .encode();
        let mut rec = Vec::new();
        ipc::append_frame(&mut rec, Tag::Frame, &bytes);
        util::write_all_retry(daemon_end.as_raw_fd(), &rec, 1000).unwrap();
        // Lossy: NO self-ack. The base advances only on the relay's FrameAck.
    }

    /// The Task 3.1a acceptance property. Drives the real `relay_loop` in a
    /// thread between a synthetic DAEMON (a real lossy `FrameProducer` over a
    /// `UnixStream` pair — no self-ack; base advances only on the relay's
    /// forwarded `Tag::FrameAck`) and a synthetic UDP CLIENT (a `Terminal` + a
    /// `FrameSync::DumpDiff` applier + an `InputOutbox` over a `Connection`
    /// loopback pair). Delivers every frame and asserts: the client converges on
    /// the daemon screen across a `Full` then a `Diff`; the client's forwarded
    /// acks advance the daemon base (so frame 2 is a `Diff`, not a repeated
    /// `Full`); typed input reaches the daemon; and the relay Init'd the daemon
    /// as a lossy frame client.
    #[test]
    fn relay_bridges_frames_acks_and_input_over_udp() {
        let (rows, cols) = (24u16, 80u16);

        // UDP loopback transport.
        let key = Key::random();
        let (relay_conn, port) = Connection::server((62700, 62799), &key, Family::Inet).unwrap();
        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut client_conn = Connection::client(addr, &key).unwrap();

        // Daemon socket: the relay owns one end, the test plays the daemon on the
        // other. Both nonblocking so neither side of the test blocks.
        let (relay_end, daemon_end) = UnixStream::pair().unwrap();
        relay_end.set_nonblocking(true).unwrap();
        daemon_end.set_nonblocking(true).unwrap();

        let mut link = DaemonLink {
            stream: relay_end,
            read: FrameBuffer::new(),
            write: Vec::new(),
            frame_offset: 0,
        };
        // The relay would send Init+Resize after connect_or_create; queue them so
        // the daemon side can confirm it was Init'd as a lossy frame client.
        ipc::append_frame(
            &mut link.write,
            Tag::Init,
            &init_payload(rows, cols, &content_caps(&[])),
        );
        ipc::append_frame(&mut link.write, Tag::Resize, &ipc::encode_resize(rows, cols));

        let relay = std::thread::spawn(move || relay_loop(relay_conn, link, (rows, cols)));

        // --- synthetic UDP client state ---
        let mut client_term = Terminal::with_scrollback(rows, cols, 0);
        let mut applier = FrameSync::DumpDiff.applier();
        let mut applied_data: Vec<u8> = Vec::new();
        let mut applied_num = 0u64;
        let mut acked_frame = 0u64;
        let mut input_acked = 0u64;
        let mut outbox = InputOutbox::new();
        outbox.push(b"hi\n");
        let mut saw_full = false;
        let mut saw_diff = false;

        // --- synthetic daemon state ---
        let mut dterm = Terminal::with_scrollback(rows, cols, 1000);
        fill_screen(&mut dterm);
        let mut dprod = FrameProducer::new(rows, cols);
        let mut dread = FrameBuffer::new();
        let mut daemon_input: Vec<u8> = Vec::new();
        let mut saw_lossy_init = false;
        let mut sent_full = false;
        let mut sent_diff = false;

        let mut fragmenter = Fragmenter::new();
        let mut assembly = FragmentAssembly::new();

        let mut shutting = false;
        let mut done = false;
        let deadline = now_ms() + 15_000;
        while now_ms() < deadline {
            // client -> relay: the mosh cumulative-retransmit message.
            let flags = if shutting { sync::CLIENT_FLAG_SHUTDOWN } else { 0 };
            let msg = ClientMessage {
                flags,
                caps: caps::own_table(&[]),
                acked_frame,
                rows,
                cols,
                input_base: outbox.base(),
                input: outbox.pending().to_vec(),
            };
            for frag in fragmenter.make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX) {
                client_conn.send(&frag.to_bytes()).unwrap();
            }

            std::thread::sleep(Duration::from_millis(20));

            // relay -> client: drain frames and apply them.
            loop {
                match client_conn.recv() {
                    Ok(Some(payload)) => {
                        let Ok(frag) = sync::Fragment::from_bytes(&payload) else {
                            continue;
                        };
                        let Some(assembled) = assembly.add(frag) else {
                            continue;
                        };
                        let Ok(frame) = ServerFrame::decode(&assembled) else {
                            continue;
                        };
                        input_acked = input_acked.max(frame.input_ack);
                        outbox.ack(input_acked);
                        match &frame.body {
                            FrameBody::Full(_) => saw_full = true,
                            FrameBody::Diff { .. } => saw_diff = true,
                            _ => {}
                        }
                        match applier.apply(rows, cols, &applied_data, &mut client_term, &frame.body)
                        {
                            ApplyOutcome::Advanced { dump } => {
                                applied_data = dump;
                                applied_num = frame.frame_num;
                            }
                            ApplyOutcome::AdvancedNoDump => applied_num = frame.frame_num,
                            ApplyOutcome::NoChange | ApplyOutcome::ReackAndWait => {}
                        }
                        acked_frame = acked_frame.max(applied_num);
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }

            // daemon side: read what the relay forwarded to us.
            let _ = dread.read_from(daemon_end.as_raw_fd());
            while let Ok(Some(frame)) = dread.next() {
                match frame.tag {
                    Tag::Init => {
                        // Confirm the relay Init'd us as a LOSSY frame client.
                        saw_lossy_init |= frame
                            .payload
                            .get(4..)
                            .and_then(|b| caps::decode_table(b).ok())
                            .is_some_and(|(table, _)| caps::find(&table, CAP_LOSSY).is_some());
                    }
                    Tag::Input => daemon_input.extend_from_slice(&frame.payload),
                    Tag::FrameAck => {
                        if let Some((acked, _flags)) = ipc::decode_frame_ack(&frame.payload) {
                            dprod.ack(acked);
                        }
                    }
                    _ => {}
                }
            }

            // daemon: produce the Full only after the relay has forwarded the
            // client's input — proving the UDP remote is pinned, so the relay
            // (no retransmit buffer in 3.1a) will not drop the frame.
            if !sent_full && !daemon_input.is_empty() {
                write_daemon_frame(&daemon_end, &mut dprod, &dterm);
                sent_full = true;
            }
            // After the client acks frame 1 (relay -> Tag::FrameAck -> base 1),
            // a small append now encodes as a Diff anchored at that base.
            if sent_full && !sent_diff && dprod.acked_num() >= 1 {
                dterm.process(b"edit ");
                write_daemon_frame(&daemon_end, &mut dprod, &dterm);
                sent_diff = true;
            }

            let converged =
                Snapshot::from_term(&client_term) == Snapshot::from_term(&dterm);
            if sent_diff
                && applied_num >= 2
                && saw_full
                && saw_diff
                && converged
                && daemon_input == b"hi\n"
            {
                if shutting {
                    done = true;
                    break;
                }
                // First fully-converged pass: request shutdown, then loop once
                // more so the relay sees the flag and winds down.
                shutting = true;
            }
        }

        assert!(
            done,
            "relay did not converge: saw_full={saw_full} saw_diff={saw_diff} \
             applied_num={applied_num} daemon_input={daemon_input:?} \
             converged={}",
            Snapshot::from_term(&client_term) == Snapshot::from_term(&dterm)
        );
        assert!(
            saw_lossy_init,
            "the relay must Init the daemon as a lossy frame client (CAP_LOSSY)"
        );
        assert_eq!(daemon_input, b"hi\n", "typed input must reach the daemon");

        // The CLIENT_FLAG_SHUTDOWN we sent makes the relay forward Tag::Detach and
        // wind down, so the loop returns Ok.
        relay.join().unwrap().unwrap();
    }

    // ---- 3.1b reliability: loss / roam / resync / heartbeat ----------------

    /// A loopback rig for the reliability tests: a real `relay_loop` thread
    /// between a synthetic lossy DAEMON (a real `FrameProducer` over a
    /// `UnixStream` pair — no self-ack) and a synthetic UDP CLIENT (a `Terminal` +
    /// `FrameApplier` + `InputOutbox` over a `Connection` loopback). Loss is
    /// injected on the CLIENT's receive side (`drop_incoming`): indistinguishable
    /// from wire loss to the relay, since the client never acks the dropped frame,
    /// so the relay's held frame survives and its RTO retransmit redelivers it.
    /// One-shot `pending_flags` drive a roam (silence) or a RESYNC.
    struct Harness {
        client_conn: Connection,
        fragmenter: Fragmenter,
        assembly: FragmentAssembly,
        rows: u16,
        cols: u16,
        // client model
        client_term: Terminal,
        applier: Box<dyn FrameApplier>,
        applied_data: Vec<u8>,
        applied_num: u64,
        acked_frame: u64,
        input_acked: u64,
        outbox: InputOutbox,
        // client knobs
        drop_incoming: usize,
        pending_flags: u8,
        saw_full: bool,
        saw_diff: bool,
        // daemon model
        daemon_end: UnixStream,
        dterm: Terminal,
        dprod: FrameProducer,
        dread: FrameBuffer,
        daemon_input: Vec<u8>,
        saw_lossy_init: bool,
        saw_resync_ack: bool,
        // the relay thread
        relay: Option<std::thread::JoinHandle<Result<()>>>,
    }

    impl Harness {
        fn new() -> Harness {
            let (rows, cols) = (24u16, 80u16);
            let key = Key::random();
            let (relay_conn, port) =
                Connection::server((62700, 62799), &key, Family::Inet).unwrap();
            let addr = format!("127.0.0.1:{port}").parse().unwrap();
            let client_conn = Connection::client(addr, &key).unwrap();

            let (relay_end, daemon_end) = UnixStream::pair().unwrap();
            relay_end.set_nonblocking(true).unwrap();
            daemon_end.set_nonblocking(true).unwrap();

            let mut link = DaemonLink {
                stream: relay_end,
                read: FrameBuffer::new(),
                write: Vec::new(),
                frame_offset: 0,
            };
            // The relay would send Init+Resize after connect_or_create; queue them
            // so the daemon side can confirm the lossy Init.
            ipc::append_frame(
                &mut link.write,
                Tag::Init,
                &init_payload(rows, cols, &content_caps(&[])),
            );
            ipc::append_frame(&mut link.write, Tag::Resize, &ipc::encode_resize(rows, cols));

            let relay = std::thread::spawn(move || relay_loop(relay_conn, link, (rows, cols)));

            let mut dterm = Terminal::with_scrollback(rows, cols, 1000);
            fill_screen(&mut dterm);

            Harness {
                client_conn,
                fragmenter: Fragmenter::new(),
                assembly: FragmentAssembly::new(),
                rows,
                cols,
                client_term: Terminal::with_scrollback(rows, cols, 0),
                applier: FrameSync::DumpDiff.applier(),
                applied_data: Vec::new(),
                applied_num: 0,
                acked_frame: 0,
                input_acked: 0,
                outbox: InputOutbox::new(),
                drop_incoming: 0,
                pending_flags: 0,
                saw_full: false,
                saw_diff: false,
                daemon_end,
                dterm,
                dprod: FrameProducer::new(rows, cols),
                dread: FrameBuffer::new(),
                daemon_input: Vec::new(),
                saw_lossy_init: false,
                saw_resync_ack: false,
                relay: Some(relay),
            }
        }

        /// Build and send one cumulative-retransmit `ClientMessage` from the
        /// current client state, folding in and clearing any one-shot flags (as
        /// the real client clears RESYNC/SHUTDOWN after one send).
        fn client_tick(&mut self) {
            let msg = ClientMessage {
                flags: self.pending_flags,
                caps: caps::own_table(&[]),
                acked_frame: self.acked_frame,
                rows: self.rows,
                cols: self.cols,
                input_base: self.outbox.base(),
                input: self.outbox.pending().to_vec(),
            };
            self.pending_flags = 0;
            for frag in self
                .fragmenter
                .make_fragments(&msg.encode(), sync::FRAGMENT_CONTENTS_MAX)
            {
                self.client_conn.send(&frag.to_bytes()).unwrap();
            }
        }

        /// Drain frames the relay sent and apply them — unless dropped (loss
        /// injection on the relay->client path). Updates apply/ack bookkeeping and
        /// the Full/Diff-seen flags. The `input_ack` is read from EVERY frame
        /// (even an Empty heartbeat) before the body is applied.
        fn client_recv(&mut self) {
            loop {
                match self.client_conn.recv() {
                    Ok(Some(payload)) => {
                        let Ok(frag) = sync::Fragment::from_bytes(&payload) else {
                            continue;
                        };
                        let Some(assembled) = self.assembly.add(frag) else {
                            continue;
                        };
                        let Ok(frame) = ServerFrame::decode(&assembled) else {
                            continue;
                        };
                        if self.drop_incoming > 0 {
                            self.drop_incoming -= 1;
                            continue; // simulate wire loss on the relay->client path
                        }
                        self.input_acked = self.input_acked.max(frame.input_ack);
                        self.outbox.ack(self.input_acked);
                        match &frame.body {
                            FrameBody::Full(_) => self.saw_full = true,
                            FrameBody::Diff { .. } => self.saw_diff = true,
                            _ => {}
                        }
                        match self.applier.apply(
                            self.rows,
                            self.cols,
                            &self.applied_data,
                            &mut self.client_term,
                            &frame.body,
                        ) {
                            ApplyOutcome::Advanced { dump } => {
                                self.applied_data = dump;
                                self.applied_num = frame.frame_num;
                            }
                            ApplyOutcome::AdvancedNoDump => self.applied_num = frame.frame_num,
                            ApplyOutcome::NoChange | ApplyOutcome::ReackAndWait => {}
                        }
                        self.acked_frame = self.acked_frame.max(self.applied_num);
                    }
                    Ok(None) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }

        /// Read whatever the relay forwarded to the daemon: confirm the lossy
        /// Init, collect input, and apply frame-acks exactly as the real daemon's
        /// `apply_frame_ack` does (advance the base; RESYNC drops it).
        fn daemon_pump(&mut self) {
            let _ = self.dread.read_from(self.daemon_end.as_raw_fd());
            while let Ok(Some(rec)) = self.dread.next() {
                match rec.tag {
                    Tag::Init => {
                        self.saw_lossy_init |= rec
                            .payload
                            .get(4..)
                            .and_then(|b| caps::decode_table(b).ok())
                            .is_some_and(|(t, _)| caps::find(&t, CAP_LOSSY).is_some());
                    }
                    Tag::Input => self.daemon_input.extend_from_slice(&rec.payload),
                    Tag::FrameAck => {
                        if let Some((acked, flags)) = ipc::decode_frame_ack(&rec.payload) {
                            self.dprod.ack(acked);
                            if flags & ipc::FRAME_ACK_RESYNC != 0 {
                                self.dprod.drop_acked_base();
                                self.saw_resync_ack = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        /// Produce one lossy visible frame from the current daemon screen (no
        /// self-ack — the base advances only on a forwarded FrameAck).
        fn daemon_produce(&mut self) {
            write_daemon_frame(&self.daemon_end, &mut self.dprod, &self.dterm);
        }

        fn converged(&self) -> bool {
            Snapshot::from_term(&self.client_term) == Snapshot::from_term(&self.dterm)
        }

        /// One driver step: client sends, brief settle, then both directions pump.
        fn step(&mut self) {
            self.client_tick();
            std::thread::sleep(Duration::from_millis(20));
            self.client_recv();
            self.daemon_pump();
        }

        /// Wind the relay down (CLIENT_FLAG_SHUTDOWN -> Tag::Detach) and join,
        /// pumping the daemon side so it drains and the socket never wedges.
        fn join(mut self) {
            self.pending_flags |= sync::CLIENT_FLAG_SHUTDOWN;
            self.client_tick();
            let Some(handle) = self.relay.take() else {
                return;
            };
            let deadline = now_ms() + 3000;
            while now_ms() < deadline && !handle.is_finished() {
                self.daemon_pump();
                std::thread::sleep(Duration::from_millis(10));
            }
            handle.join().unwrap().unwrap();
        }
    }

    /// Drop the daemon's Full off the wire; the relay's held frame is never acked,
    /// so its RTO retransmit redelivers it and the client converges. The single
    /// exercise of the O(1) buffer's recovery path end-to-end.
    #[test]
    fn retransmit_recovers_dropped_frames() {
        let mut h = Harness::new();
        h.outbox.push(b"hi\n");
        // Drop the first two deliveries (the initial send + one retransmit); the
        // second retransmit must get through.
        h.drop_incoming = 2;
        let mut produced = false;

        let deadline = now_ms() + 15_000;
        while now_ms() < deadline {
            h.step();
            // Produce the daemon Full once, after the relay forwarded our input
            // (peer pinned). It will be dropped twice, then retransmitted.
            if !produced && !h.daemon_input.is_empty() {
                h.daemon_produce();
                produced = true;
            }
            if produced && h.saw_full && h.converged() && h.daemon_input == b"hi\n" {
                break;
            }
        }
        assert!(produced, "daemon never saw the forwarded input");
        assert!(h.saw_full, "client never received the Full (retransmit failed)");
        assert!(h.converged(), "client did not converge after drop + retransmit");
        h.join();
    }

    /// Roam: the client goes silent (drops every frame) while the daemon changes
    /// the screen several times, then returns. Each new daemon frame supersedes
    /// the held one — the relay holds only the latest — so on return the client
    /// jumps straight from the base (frame 1) to the final screen (frame 4) via a
    /// single retransmitted, base-anchored Diff. Convergence + that A->final jump
    /// is the end-to-end O(1) proof (the deterministic bound is
    /// `held_frame_is_o1_supersede_drop_clear`).
    #[test]
    fn roam_holds_only_latest_and_reconverges() {
        let mut h = Harness::new();
        h.outbox.push(b"go\n");
        let mut base_sent = false;
        let mut roamed = false;
        let mut extra = 0u32;
        let mut resumed = false;

        let deadline = now_ms() + 20_000;
        while now_ms() < deadline {
            h.step();

            // 1. Establish a base A (frame 1) the client acks.
            if !base_sent && !h.daemon_input.is_empty() {
                h.daemon_produce();
                base_sent = true;
            }
            // 2. Once the client holds A, roam: silence the client (drop all) and
            //    change the daemon screen 3 times. Each frame anchors at the acked
            //    base A (no acks flow) and supersedes the held one.
            if base_sent && !roamed && h.applied_num >= 1 {
                h.drop_incoming = usize::MAX;
                roamed = true;
            }
            if roamed && !resumed && extra < 3 {
                h.dterm.process(format!("roam edit {extra} ").as_bytes());
                h.daemon_produce();
                extra += 1;
            }
            // 3. The client returns: stop dropping. The relay retransmits the one
            //    held (latest, base-A anchored) frame and the client converges.
            if roamed && !resumed && extra == 3 {
                h.drop_incoming = 0;
                resumed = true;
            }
            if resumed && h.converged() && h.applied_num >= 4 {
                break;
            }
        }
        assert!(resumed, "test never reached the roam-return phase");
        assert!(h.converged(), "client did not reconverge after roam");
        assert_eq!(
            h.applied_num, 4,
            "client jumped base(1) -> final(4): the relay held only the latest frame (O(1))"
        );
        h.join();
    }

    /// A client base-sum divergence (`CLIENT_FLAG_RESYNC`) is forwarded as a
    /// `Tag::FrameAck{RESYNC}`; the daemon drops its base so its next frame is a
    /// `Full` that recovers the diverged client.
    #[test]
    fn resync_forces_a_recovering_full() {
        let mut h = Harness::new();
        h.outbox.push(b"x\n");
        let mut base_sent = false;
        let mut diverged = false;
        let mut recovery_sent = false;

        let deadline = now_ms() + 15_000;
        while now_ms() < deadline {
            h.step();

            // 1. Sync the client to a base screen A.
            if !base_sent && !h.daemon_input.is_empty() {
                h.daemon_produce();
                base_sent = true;
            }
            // 2. Diverge: change the daemon screen (A is now stale) and have the
            //    client request a resync. Watch for the RECOVERY Full specifically.
            if base_sent && !diverged && h.applied_num >= 1 {
                h.dterm.process(b"\x1b[2J\x1b[Hdiverged and resynced\r\n");
                h.saw_full = false;
                h.pending_flags |= sync::CLIENT_FLAG_RESYNC;
                diverged = true;
            }
            // 3. After the relay forwards the RESYNC ack (daemon drops its base),
            //    the daemon's next frame is a Full — send it once.
            if h.saw_resync_ack && !recovery_sent {
                h.daemon_produce();
                recovery_sent = true;
            }
            if diverged && h.saw_resync_ack && h.saw_full && h.converged() {
                break;
            }
        }
        assert!(
            h.saw_resync_ack,
            "relay never forwarded a FRAME_ACK_RESYNC to the daemon"
        );
        assert!(h.saw_full, "daemon's post-resync frame was not a recovering Full");
        assert!(h.converged(), "client did not recover after RESYNC");
        h.join();
    }

    /// A static screen still advances the client's `input_ack`: input typed after
    /// the last visible frame is carried forward only by the periodic Empty
    /// heartbeat (no new frame, no force-ack in scope).
    #[test]
    fn heartbeat_advances_input_ack_on_static_screen() {
        let mut h = Harness::new();
        h.outbox.push(b"a\n");
        let first_len = 2u64; // "a\n"
        let total_len = first_len + 5; // + "more\n"
        let mut base_sent = false;
        let mut typed_more = false;

        let deadline = now_ms() + 20_000;
        while now_ms() < deadline {
            h.step();

            // 1. One visible frame carries the first input's ack.
            if !base_sent && !h.daemon_input.is_empty() {
                h.daemon_produce();
                base_sent = true;
            }
            // 2. After the client synced + heard that ack, type MORE input but
            //    keep the screen static (no new daemon frame). Only a heartbeat
            //    Empty frame can now carry the elevated input_ack forward.
            if base_sent && !typed_more && h.applied_num >= 1 && h.input_acked >= first_len {
                h.outbox.push(b"more\n");
                typed_more = true;
            }
            if typed_more && h.input_acked >= total_len && h.outbox.pending().is_empty() {
                break;
            }
        }
        assert!(typed_more, "never reached the static-input phase");
        assert_eq!(
            h.input_acked, total_len,
            "the heartbeat carried the full input ack (\"a\\nmore\\n\")"
        );
        assert!(
            h.outbox.pending().is_empty(),
            "client outbox cleared by the heartbeat ack"
        );
        h.join();
    }
}
