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
//! # Scope — Task 3.1a (happy path only)
//!
//! Every UDP frame is delivered (no loss). The relay re-wraps each daemon frame's
//! transport header and forwards it; it holds NO retransmit buffer. The daemon
//! runs the relay's per-client `FrameProducer` in lossy, ack-gated mode
//! (`CAP_LOSSY`, Task 3.0), so each new frame supersedes the last unacked one and
//! the relay's future retransmit state stays O(1). Loss recovery + the O(1)
//! retransmit buffer + RESYNC-on-divergence are Task 3.1b; agent-cap termination
//! is 3.2; the `cmd_server` relay verb + bootstrap is 3.3.
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
    // the final `FLAG_SHUTDOWN` frame (an Empty body advances no apply state).
    let mut last_frame_num = 0u64;
    let err_events = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;

    loop {
        let mut fds = vec![util::pollfd(conn.raw_fd(), libc::POLLIN)];
        let mut link_events = libc::POLLIN;
        if !link.write.is_empty() {
            link_events |= libc::POLLOUT;
        }
        fds.push(util::pollfd(link.stream.as_raw_fd(), link_events));

        match util::poll(&mut fds, 1000) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }

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
                        // Frame-ack advance -> Tag::FrameAck so the daemon moves
                        // its diff base (no RESYNC flag in 3.1a — that's 3.1b).
                        if msg.acked_frame > acked_forwarded {
                            acked_forwarded = msg.acked_frame;
                            ipc::append_frame(
                                &mut link.write,
                                Tag::FrameAck,
                                &ipc::encode_frame_ack(msg.acked_frame - link.frame_offset, 0),
                            );
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
                                // ECHO_TIMEOUT). The 3.1a happy path has no
                                // optimistic-echo client, so acking received input
                                // as echoed is inert here.
                                let ack = inbox.next_offset();
                                let out = rewrap(daemon_frame, link.frame_offset, ack, ack);
                                last_frame_num = out.frame_num;
                                if conn.has_remote() {
                                    send_payload(&mut conn, &mut fragmenter, &out.encode());
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
    use crate::remote::framesync::{ApplyOutcome, FrameProducer, FrameSync};
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
}
