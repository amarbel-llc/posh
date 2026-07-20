//! Deterministic server-frame harness (github #75).
//!
//! posh's remote server-frame tests spawn a real `/bin/sh` under a PTY and run
//! the live `server_loop` over loopback UDP — timing-dependent and flaky. This
//! harness drives the *same* frame codecs (`posh_proto::framesync`) with no
//! socket, no PTY, and no clock: a [`ServerSide`] feeds bytes through a server
//! `Terminal`, encodes a [`ServerFrame`], and hands it to a [`FrameChannel`]; a
//! [`ClientSide`] applies delivered frames into its own mirror terminal and
//! acks. The [`TestChannel`] in between exposes the determinism knobs that
//! replace wall-clock: `deliver` one frame, `drop_next` (loss), and `ack`
//! (let the server learn the client's progress, or withhold it to model
//! ack-lag).
//!
//! [`FrameHarness`] ties the three together. Convergence — the client's mirror
//! reproducing the server's screen byte-for-byte ([`FrameHarness::converged`])
//! — is the invariant remote tests assert, and the dual of the background-bleed
//! divergence tracked in posh#100.
//!
//! Faithful simplifications vs the production `server_loop`: the server retains
//! every produced frame's [`Baseline`] (so it can always anchor an incremental
//! body at whatever frame the client last acked, instead of dropping baselines
//! under loss), input/echo acks and capability negotiation are omitted, and the
//! client acks its true `applied_num` on every frame (advanced or re-acked).
//! Scrollback bodies are not yet produced here.

use std::collections::VecDeque;

use posh_proto::channel::{ClientAck, FrameChannel};
use posh_proto::display::Snapshot;
use posh_proto::frame::{FrameBody, ServerFrame};
use posh_proto::framesync::{
    ApplyOutcome, Baseline, CurrentFrame, DumpDiff, FrameApplier, FrameEncoder, FrameSync,
    MorphDelta,
};
use posh_term::Terminal;

fn encoder_for(sync: FrameSync) -> Box<dyn FrameEncoder> {
    match sync {
        FrameSync::Morph => Box::new(MorphDelta::default()),
        FrameSync::DumpDiff => Box::new(DumpDiff),
    }
}

/// The server end: the authoritative `Terminal` advanced by fed bytes.
///
/// Deliberately holds NO per-client encoder state. Production gives every
/// attached client its own `FrameProducer`, because each diffs against its OWN
/// acked base (`daemon.rs`'s per-`ClientConn` producer); that state lives on
/// [`ClientLane`] here for the same reason.
pub struct ServerSide {
    term: Terminal,
}

impl ServerSide {
    fn new(rows: u16, cols: u16) -> ServerSide {
        ServerSide {
            term: Terminal::with_scrollback(rows, cols, 0),
        }
    }

    fn baseline_now(&self, num: u64) -> Baseline {
        Baseline {
            num,
            dump: self.term.dump_vt(),
            snapshot: Snapshot::from_term(&self.term),
            alt_screen: self.term.is_alt_screen(),
            rows: self.term.rows(),
            cols: self.term.cols(),
        }
    }

    /// Encode one frame of the server's CURRENT state for `lane`, anchored at
    /// that lane's own acked baseline, and enqueue it on the lane's queue.
    fn encode_for(&self, lane: &mut ClientLane) {
        let num = lane.next_num;
        lane.next_num += 1;
        let dump = self.term.dump_vt();
        let snapshot = Snapshot::from_term(&self.term);
        let cur = CurrentFrame {
            dump: &dump,
            snapshot: &snapshot,
            alt_screen: self.term.is_alt_screen(),
            rows: self.term.rows(),
            cols: self.term.cols(),
        };
        let body = lane.enc.encode(lane.acked.as_ref(), &cur);
        lane.produced.push_back(self.baseline_now(num));
        lane.send_frame(ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: num,
            input_ack: 0,
            echo_ack: 0,
            body,
        });
    }
}

/// The client end: a mirror `Terminal` the delivered frames are applied into,
/// driven by the same [`FrameApplier`] codec production uses.
pub struct ClientSide {
    term: Terminal,
    applier: Box<dyn FrameApplier>,
    applied_num: u64,
    applied_data: Vec<u8>,
    rows: u16,
    cols: u16,
}

impl ClientSide {
    /// A mirror at `rows` x `cols` — which need NOT match the server's. In
    /// production they routinely don't: the daemon sizes the pty to the
    /// SMALLEST attached client (tmux `window-size smallest`), so every larger
    /// client permanently renders a grid smaller than its own terminal.
    pub fn new(rows: u16, cols: u16, sync: FrameSync) -> ClientSide {
        ClientSide {
            term: Terminal::with_scrollback(rows, cols, 0),
            applier: sync.applier(),
            applied_num: 0,
            applied_data: Vec::new(),
            rows,
            cols,
        }
    }

    /// Apply one delivered frame (mirroring `client.rs::apply_frame`, minus the
    /// base-checksum and forensics paths), returning the ack the server will
    /// see: always the client's true `applied_num`, whether it advanced or
    /// re-acked an unappliable body.
    fn apply(&mut self, frame: &ServerFrame) -> ClientAck {
        // A stale retransmission of an already-applied frame: re-ack, no change.
        if frame.frame_num < self.applied_num {
            return ClientAck { acked_frame: self.applied_num };
        }
        match &frame.body {
            FrameBody::Empty => {}
            FrameBody::Full(_) => {
                let outcome =
                    self.applier
                        .apply(self.rows, self.cols, &self.applied_data, &mut self.term, &frame.body);
                self.absorb(outcome, frame.frame_num);
            }
            // Incremental bodies apply only at their anchored base; a base
            // mismatch (the client moved past it, or never reached it) re-acks
            // and waits for the server to anchor at the client's real state.
            FrameBody::Diff { base, .. } | FrameBody::Morph { base, .. } => {
                if *base == self.applied_num && frame.frame_num > self.applied_num {
                    let outcome = self.applier.apply(
                        self.rows,
                        self.cols,
                        &self.applied_data,
                        &mut self.term,
                        &frame.body,
                    );
                    self.absorb(outcome, frame.frame_num);
                }
            }
            // Scrollback production isn't modelled here yet (github #75 follow-up).
            // v2 bodies (RFC 0009) carry no visible state and never touch
            // applied_num by design, so ignoring them here is exact.
            FrameBody::Scrollback { .. } | FrameBody::Scrollback2 { .. } => {}
        }
        ClientAck { acked_frame: self.applied_num }
    }

    fn absorb(&mut self, outcome: ApplyOutcome, num: u64) {
        match outcome {
            ApplyOutcome::Advanced { dump } => {
                self.applied_num = num;
                self.applied_data = dump;
            }
            ApplyOutcome::AdvancedNoDump => self.applied_num = num,
            ApplyOutcome::ReackAndWait | ApplyOutcome::NoChange => {}
        }
    }
}

/// Identifies one attached client on a [`FrameHarness`].
///
/// The index is private: ids come from [`FrameHarness::add_client`] or
/// [`ClientId::PRIMARY`], so one cannot be fabricated (or carried over from a
/// different harness) and silently index the wrong lane.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClientId(usize);

impl ClientId {
    /// The client every harness starts with, at the server's own size.
    pub const PRIMARY: ClientId = ClientId(0);
}

/// One client and the in-memory network to it: its own frame queue, its own ack
/// queue, and — mirroring production — its own encoder and acked baseline, so
/// frames for this client are diffed against what THIS client last acked.
///
/// Independent queues are what make loss expressible per client: a frame can be
/// dropped toward one client while another receives it, leaving the two at
/// different bases. A single shared queue would move every client in lockstep.
pub struct ClientLane {
    client: ClientSide,
    enc: Box<dyn FrameEncoder>,
    next_num: u64,
    /// The baseline THIS client acked (`None` until its first ack → `Full`).
    acked: Option<Baseline>,
    /// Produced-but-not-yet-superseded baselines for this client.
    produced: VecDeque<Baseline>,
    to_client: VecDeque<ServerFrame>,
    to_server: VecDeque<ClientAck>,
}

impl FrameChannel for ClientLane {
    fn send_frame(&mut self, frame: ServerFrame) {
        self.to_client.push_back(frame);
    }

    fn recv_ack(&mut self) -> Option<ClientAck> {
        self.to_server.pop_front()
    }
}

impl ClientLane {
    fn new(rows: u16, cols: u16, sync: FrameSync) -> ClientLane {
        ClientLane {
            client: ClientSide::new(rows, cols, sync),
            enc: encoder_for(sync),
            next_num: 1,
            acked: None,
            produced: VecDeque::new(),
            to_client: VecDeque::new(),
            to_server: VecDeque::new(),
        }
    }

    /// Deliver this lane's next pending frame; the client applies it and queues
    /// its ack. False when nothing is pending.
    fn deliver(&mut self) -> bool {
        match self.to_client.pop_front() {
            Some(frame) => {
                let ack = self.client.apply(&frame);
                self.to_server.push_back(ack);
                true
            }
            None => false,
        }
    }

    /// Drop this lane's next pending frame (UDP loss). False when none pending.
    fn drop_next(&mut self) -> bool {
        self.to_client.pop_front().is_some()
    }

    /// Advance this lane's acked baseline to the highest frame the client has
    /// acknowledged and the lane still retains.
    fn drain_acks(&mut self) {
        while let Some(ack) = self.recv_ack() {
            if let Some(base) = self.produced.iter().find(|b| b.num == ack.acked_frame) {
                self.acked = Some(base.clone());
            }
            while self.produced.front().is_some_and(|b| b.num < ack.acked_frame) {
                self.produced.pop_front();
            }
        }
    }
}

/// A terminal's rows as text with leading and trailing BLANK rows removed —
/// the anchor-agnostic view of its content.
///
/// Anchor-agnostic on purpose: `dump_vt` legitimately places a smaller session
/// at either end of a larger client. A session with no scrollback homes and
/// draws downward (content at the top, blanks below); a scrolled session
/// replays as a continuous newline flow that lands at the target's BOTTOM
/// (blanks above). Both are correct, so the invariant is that the content is
/// present and in order — not which row number it starts on. Interior blank
/// rows are preserved, so a hole punched in the middle still fails.
fn content_rows(term: &Terminal) -> Vec<String> {
    let rows: Vec<String> = (0..term.rows())
        .map(|r| {
            term.screen()
                .row(r)
                .map(|row| row.text(false).trim_end().to_string())
                .unwrap_or_default()
        })
        .collect();
    match (
        rows.iter().position(|r| !r.is_empty()),
        rows.iter().rposition(|r| !r.is_empty()),
    ) {
        (Some(start), Some(end)) => rows[start..=end].to_vec(),
        _ => Vec::new(),
    }
}

/// The text of the row the cursor is on, trailing blanks trimmed.
fn cursor_row_text(term: &Terminal) -> String {
    let c = term.cursor();
    term.screen()
        .row(c.row)
        .map(|r| r.text(false).trim_end().to_string())
        .unwrap_or_default()
}

/// Drives a deterministic server↔client frame round-trip over [`TestChannel`].
///
/// ```ignore
/// let mut h = FrameHarness::new(24, 80, FrameSync::Morph);
/// h.feed(b"\x1b[41mred bar\x1b[0m\r\n"); // server encodes a frame
/// h.deliver();                            // client applies it
/// h.ack();                                // server learns the ack
/// assert!(h.converged());                 // mirror == server, no bleed
/// ```
pub struct FrameHarness {
    server: ServerSide,
    lanes: Vec<ClientLane>,
    sync: FrameSync,
}

impl FrameHarness {
    /// A harness with one client at the server's own size — the same-geometry
    /// case. Use [`FrameHarness::add_client`] for a differently-sized one.
    pub fn new(rows: u16, cols: u16, sync: FrameSync) -> FrameHarness {
        FrameHarness {
            server: ServerSide::new(rows, cols),
            lanes: vec![ClientLane::new(rows, cols, sync)],
            sync,
        }
    }

    /// Attach another client at ITS OWN geometry, which need not match the
    /// server's. It starts with no acked base, so its first frame is a `Full`.
    pub fn add_client(&mut self, rows: u16, cols: u16) -> ClientId {
        self.lanes.push(ClientLane::new(rows, cols, self.sync));
        ClientId(self.lanes.len() - 1)
    }

    fn lane(&self, id: ClientId) -> &ClientLane {
        &self.lanes[id.0]
    }

    fn lane_mut(&mut self, id: ClientId) -> &mut ClientLane {
        &mut self.lanes[id.0]
    }

    /// Advance the server by `bytes` and enqueue one frame toward EVERY client,
    /// each encoded against that client's own acked base.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.server.term.process(bytes);
        let _ = self.server.term.take_responses();
        for lane in &mut self.lanes {
            self.server.encode_for(lane);
        }
    }

    /// Deliver the next pending frame to client 0 (it applies + acks).
    pub fn deliver(&mut self) -> bool {
        self.deliver_for(ClientId::PRIMARY)
    }

    /// Deliver `id`'s next pending frame, leaving other clients untouched.
    pub fn deliver_for(&mut self, id: ClientId) -> bool {
        self.lane_mut(id).deliver()
    }

    /// Deliver every pending frame to every client, then let the server learn
    /// all their acks — the "no loss, no lag" shortcut.
    pub fn deliver_all(&mut self) {
        for lane in &mut self.lanes {
            while lane.deliver() {}
            lane.drain_acks();
        }
    }

    /// Drop the next pending frame to client 0, modelling UDP loss.
    pub fn drop_next(&mut self) -> bool {
        self.drop_next_for(ClientId::PRIMARY)
    }

    /// Drop `id`'s next pending frame only — other clients still receive theirs.
    pub fn drop_next_for(&mut self, id: ClientId) -> bool {
        self.lane_mut(id).drop_next()
    }

    /// Let the server learn every ack client 0 has made available. Withhold
    /// this between feeds to model ack-lag.
    pub fn ack(&mut self) {
        self.ack_for(ClientId::PRIMARY);
    }

    /// Let the server learn `id`'s acks only.
    pub fn ack_for(&mut self, id: ClientId) {
        self.lane_mut(id).drain_acks();
    }

    /// Number of frames sent to client 0 but not yet delivered or dropped.
    pub fn pending_frames(&self) -> usize {
        self.pending_frames_for(ClientId::PRIMARY)
    }

    /// Frames pending toward `id`.
    pub fn pending_frames_for(&self, id: ClientId) -> usize {
        self.lane(id).to_client.len()
    }

    /// The server's authoritative visible screen state (cells, cursor, modes).
    pub fn server_snapshot(&self) -> Snapshot {
        Snapshot::from_term(&self.server.term)
    }

    /// Client 0's mirror's visible screen state.
    pub fn client_snapshot(&self) -> Snapshot {
        self.client_snapshot_for(ClientId::PRIMARY)
    }

    /// `id`'s mirror's visible screen state.
    pub fn client_snapshot_for(&self, id: ClientId) -> Snapshot {
        Snapshot::from_term(&self.lane(id).client.term)
    }

    /// The content invariant for a client whose geometry may DIFFER from the
    /// server's: the server's rows appear on the client, in order, and the
    /// client's cursor sits on the same row CONTENT as the server's.
    ///
    /// Deliberately not "compare the client against a fresh terminal fed the
    /// server's `dump_vt`". That reference would run through the same
    /// serializer under test, so a height-dependence bug in `dump_vt` yields
    /// the same wrong answer on both sides and the check passes — it would have
    /// caught neither of the mismatched-size cursor bugs this harness exists to
    /// prevent. Anchoring on content instead keeps the oracle independent.
    pub fn mirrors_content(&self, id: ClientId) -> bool {
        let client = &self.lane(id).client.term;
        content_rows(client) == content_rows(&self.server.term)
            && cursor_row_text(client) == cursor_row_text(&self.server.term)
    }

    /// [`FrameHarness::mirrors_content`] with a readable diff on failure.
    pub fn assert_mirrors_content(&self, id: ClientId) {
        let client = &self.lane(id).client.term;
        assert!(
            self.mirrors_content(id),
            "client {id:?} ({}x{}) diverged from server ({}x{}):\n  \
             server rows: {:?}\n  client rows: {:?}\n  \
             server cursor row: {:?}\n  client cursor row: {:?}",
            client.rows(),
            client.cols(),
            self.server.term.rows(),
            self.server.term.cols(),
            content_rows(&self.server.term),
            content_rows(client),
            cursor_row_text(&self.server.term),
            cursor_row_text(client),
        );
    }

    /// The client mirror reproduces the server's *visible* screen exactly. A
    /// background-color bleed / over-paint (posh#100) is precisely a violation
    /// of this on the client side: cells carrying a background the source cell
    /// did not have.
    ///
    /// Compared at `Snapshot` granularity rather than `dump_vt` bytes on
    /// purpose: the `MorphDelta` escape stream deliberately normalizes the
    /// terminal's trailing SGR pen to default at frame end, so the live pen (in
    /// `dump_vt`) can differ from the server's even when every visible cell —
    /// everything the user sees — is identical. `Snapshot` captures the rendered
    /// grid, not the residual pen.
    pub fn converged(&self) -> bool {
        self.client_snapshot() == self.server_snapshot()
    }

    /// Convergence with a readable diff on failure.
    pub fn assert_converged(&self) {
        assert!(
            self.converged(),
            "client mirror diverged from server (visible screen):\n  server: {:?}\n  client: {:?}",
            String::from_utf8_lossy(&self.server.term.dump_vt()),
            String::from_utf8_lossy(&self.lane(ClientId::PRIMARY).client.term.dump_vt()),
        );
    }
}

#[cfg(test)]
mod mismatched_size_tests {
    use super::*;

    /// The production shape (posh#139): the daemon sizes the pty to the
    /// SMALLEST attached client, so a larger client permanently renders a grid
    /// smaller than its own terminal. Both clients consume the SAME frame
    /// stream and both must end up showing the session's content with their
    /// cursor on the right line — the invariant two separate `dump_vt` bugs
    /// violated (the scrollback path's absolute CUP, and the alt path's
    /// unhomed grid), neither of which this harness could previously express.
    #[test]
    fn a_taller_client_mirrors_the_session_content() {
        for sync in [FrameSync::DumpDiff, FrameSync::Morph] {
            let mut h = FrameHarness::new(24, 80, sync); // client 0 == server
            let tall = h.add_client(50, 80); // client 1 is taller

            h.feed(b"line A\r\nline B\r\nprompt$ ");
            h.deliver_all();

            h.assert_mirrors_content(ClientId::PRIMARY);
            h.assert_mirrors_content(tall);
            assert!(
                h.converged(),
                "the same-size client must still converge exactly ({sync:?})"
            );
        }
    }

    /// Scrolled far enough to fill scrollback, which routes `dump_vt` down its
    /// bottom-landing flow instead of the homed draw — the case where the
    /// content sits at the taller client's BOTTOM rather than its top. The
    /// content assertion is anchor-agnostic precisely so both are accepted.
    #[test]
    fn a_taller_client_mirrors_a_scrolled_session() {
        let mut h = FrameHarness::new(24, 80, FrameSync::DumpDiff);
        let tall = h.add_client(50, 80);

        for i in 0..40u16 {
            h.feed(format!("line {i:02}\r\n").as_bytes());
        }
        h.feed(b"prompt$ ");
        h.deliver_all();

        h.assert_mirrors_content(ClientId::PRIMARY);
        h.assert_mirrors_content(tall);
        h.assert_converged(); // the same-size client still matches exactly
    }

    /// A full-screen app on the alt screen, mirrored onto a taller client. This
    /// is the exact shape of the alt-path cursor bug: the grid was drawn from
    /// wherever the preceding flow parked the cursor, so on a taller target the
    /// cursor landed above its content.
    #[test]
    fn a_taller_client_mirrors_an_alt_screen_session() {
        let mut h = FrameHarness::new(24, 80, FrameSync::DumpDiff);
        let tall = h.add_client(50, 80);

        for i in 0..40u16 {
            h.feed(format!("line {i:02}\r\n").as_bytes());
        }
        h.feed(b"\x1b[?1049h"); // enter the alt screen
        h.feed(b"\x1b[5;1HALTMARK");
        h.deliver_all();

        h.assert_mirrors_content(ClientId::PRIMARY);
        h.assert_mirrors_content(tall);
        h.assert_converged(); // the same-size client still matches exactly
    }

    /// Independent queues: a frame lost toward ONE client must not disturb the
    /// other, and the straggler must recover on the next frame — encoded
    /// against ITS OWN stale base, not the other client's.
    #[test]
    fn loss_toward_one_client_leaves_the_other_untouched() {
        let mut h = FrameHarness::new(24, 80, FrameSync::DumpDiff);
        let tall = h.add_client(50, 80);

        h.feed(b"first\r\n");
        assert!(h.drop_next_for(ClientId::PRIMARY), "client 0 loses frame 1");
        assert!(h.deliver_for(tall), "the taller client receives it");
        h.ack_for(tall);
        h.assert_mirrors_content(tall);

        // The straggler catches up on the next frame without the other
        // client's progress being rolled back.
        h.feed(b"second\r\n");
        h.deliver_all();
        h.assert_mirrors_content(ClientId::PRIMARY);
        h.assert_mirrors_content(tall);
        h.assert_converged(); // the straggler recovered exactly, not approximately
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A table of transitions every round-trip must reproduce, including
    // background-SGR content (the posh#100 bleed class): a colored bar, a
    // full-width background fill, an erase-to-EOL under a non-default pen, and
    // a region that scrolls within the visible grid.
    const STEPS: &[&[u8]] = &[
        b"\x1b[2J\x1b[Hplain prompt $ ",
        b"echo hello\r\n",
        b"\x1b[41m red background bar across cells \x1b[0m\r\n",
        b"\x1b[44;37mblue fill\x1b[K\r\n", // erase-to-EOL with a non-default pen
        b"\x1b[1;33mbold yellow\x1b[0m then default\r\n",
        b"line\r\nline\r\nline\r\nline\r\nline\r\n",
    ];

    fn run_immediate(sync: FrameSync) {
        let mut h = FrameHarness::new(24, 80, sync);
        for step in STEPS {
            h.feed(step);
            assert!(h.deliver(), "a frame should be pending to deliver");
            h.ack();
            h.assert_converged();
        }
    }

    #[test]
    fn morph_round_trip_converges_with_background_sgr() {
        run_immediate(FrameSync::Morph);
    }

    #[test]
    fn dumpdiff_round_trip_converges_with_background_sgr() {
        run_immediate(FrameSync::DumpDiff);
    }

    fn run_with_loss(sync: FrameSync) {
        // Incremental bodies anchor at the acked base, so a dropped intermediate
        // frame is recovered by the next one (anchored at the same base) — no
        // background bleed survives the loss.
        let mut h = FrameHarness::new(24, 80, sync);
        h.feed(b"\x1b[2J\x1b[Hbase\r\n");
        assert!(h.deliver()); // Full keyframe
        h.ack();
        h.assert_converged();

        h.feed(b"\x1b[42mgreen row one\x1b[0m\r\n"); // frame 2, anchored at 1
        assert!(h.drop_next(), "frame 2 dropped in flight");
        h.feed(b"\x1b[45mmagenta row two\x1b[0m\r\n"); // frame 3, still anchored at 1
        assert!(h.deliver());
        h.ack();
        h.assert_converged();
    }

    #[test]
    fn morph_recovers_from_a_dropped_frame() {
        run_with_loss(FrameSync::Morph);
    }

    #[test]
    fn dumpdiff_recovers_from_a_dropped_frame() {
        run_with_loss(FrameSync::DumpDiff);
    }

    #[test]
    fn ack_lag_then_catch_up_converges() {
        // Deliver frames while withholding acks: the server keeps anchoring at
        // the stale base, the client (already past it) re-acks without applying,
        // and once the withheld acks reach the server it anchors at the client's
        // real state and the next frame converges. No divergence, no bleed.
        let mut h = FrameHarness::new(24, 80, FrameSync::Morph);
        h.feed(b"\x1b[2J\x1b[Hstart\r\n");
        assert!(h.deliver());
        h.ack();

        // Two more frames delivered, acks withheld.
        h.feed(b"\x1b[46mcyan\x1b[0m one\r\n");
        assert!(h.deliver());
        h.feed(b"\x1b[43myellow\x1b[0m two\r\n");
        assert!(h.deliver());

        // The server still thinks the client is back at frame 1; let the acks
        // through and feed once more so it re-anchors and converges.
        h.ack();
        h.feed(b"settled\r\n");
        assert!(h.deliver());
        h.ack();
        h.assert_converged();
    }

    // ---- RFC 0008 §2: the reliable transport as the degenerate datagram ----

    use posh_proto::framesync::FrameProducer;

    /// A Diff-friendly multi-step script: one substantial initial paint (cursor
    /// parked mid-screen, NO scroll), then a sequence of small edits that
    /// overwrite a single FIXED lower row via absolute cursor positioning. The
    /// stable 16-line top region is a long shared prefix every later frame
    /// diffs against, so each edit is a clear prefix/suffix-diff win (`make_diff`
    /// is prefix/suffix-based) — a `Diff` under DumpDiff, a `Morph` under
    /// MorphDelta — never a forced `Full`. Successive writes near the cursor are
    /// exactly what makes the diffs expressible.
    fn degenerate_script() -> Vec<Vec<u8>> {
        let mut first = b"\x1b[2J\x1b[H".to_vec();
        for i in 0..16u8 {
            first.extend_from_slice(
                format!("line {i:02} of representative session content\r\n").as_bytes(),
            );
        }
        // Each edit homes to row 18 (1-indexed) and rewrites it under a distinct
        // pen, erasing to end-of-line so no stale tail survives — including the
        // background-SGR content (red bar, blue erase-to-EOL) that the posh#100
        // bleed class lives in.
        let edits: &[&[u8]] = &[
            b"\x1b[18;1Hecho hello\x1b[K",
            b"\x1b[18;1H\x1b[41m red status bar across the row \x1b[0m\x1b[K",
            b"\x1b[18;1H\x1b[44;37mblue fill then erase to eol\x1b[K",
            b"\x1b[18;1H\x1b[1;33mbold yellow\x1b[0m back to default\x1b[K",
            b"\x1b[18;1Hfinal line of the degenerate script\x1b[K",
        ];
        let mut steps = vec![first];
        steps.extend(edits.iter().map(|e| e.to_vec()));
        steps
    }

    fn body_kind(b: &FrameBody) -> &'static str {
        match b {
            FrameBody::Full(_) => "Full",
            FrameBody::Diff { .. } => "Diff",
            FrameBody::Morph { .. } => "Morph",
            FrameBody::Scrollback { .. } => "Scrollback",
            FrameBody::Scrollback2 { .. } => "Scrollback2",
            FrameBody::Empty => "Empty",
        }
    }

    /// Drive the shared [`FrameProducer`] — the very state machine the session
    /// daemon (RFC 0008) and the roaming server drive — over a lossless,
    /// immediate-ack channel: feed each step into a server `Terminal`, produce +
    /// encode one frame, apply it into a mirror through the REAL client-side
    /// applier, then ack at once so the base for the next frame is always the
    /// frame just sent. Asserts the mirror converges on the server screen after
    /// every frame and that no body is ever inapplicable — both impossible to
    /// violate over a reliable transport. Returns each step's produced body in
    /// order, so the caller can pin the body-kind sequence the `FrameHarness`
    /// does not expose.
    fn drive_producer_immediate(sync: FrameSync, steps: &[Vec<u8>]) -> Vec<FrameBody> {
        let (rows, cols) = (24u16, 80u16);
        let use_morph = matches!(sync, FrameSync::Morph);
        let mut server = Terminal::with_scrollback(rows, cols, 0);
        let mut producer = FrameProducer::new(rows, cols);
        let mut client = Terminal::with_scrollback(rows, cols, 0);
        let mut applier = sync.applier();
        let mut applied: Vec<u8> = Vec::new();
        let mut bodies = Vec::new();
        for step in steps {
            server.process(step);
            let _ = server.take_responses();
            producer.advance_visible(
                server.dump_vt(),
                Snapshot::from_term(&server),
                server.is_alt_screen(),
                (server.rows(), server.cols()),
                0,
            );
            let body = producer.encode_visible(use_morph);
            let num = producer.current_num();
            match applier.apply(rows, cols, &applied, &mut client, &body) {
                ApplyOutcome::Advanced { dump } => applied = dump,
                ApplyOutcome::AdvancedNoDump | ApplyOutcome::NoChange => {}
                ApplyOutcome::ReackAndWait => {
                    panic!("a lossless transport must never force a re-ack: the base never diverges")
                }
            }
            // Immediate ack: the reliable, ordered socket delivers every frame, so
            // the producer learns the new base at once and never retransmits.
            producer.ack(num);
            assert_eq!(
                Snapshot::from_term(&client),
                Snapshot::from_term(&server),
                "client mirror diverged from server at frame {num} ({})",
                body_kind(&body),
            );
            bodies.push(body);
        }
        bodies
    }

    /// RFC 0008 §2 — the reliable Unix socket is the *degenerate* case of the
    /// lossy datagram protocol: over an immediate-ack channel the producer's
    /// loss machinery is inert. The acked base is never lost, so after the
    /// initial keyframe the codec ships only incremental bodies, and a consumer
    /// reconstructs the source screen identically at every step.
    ///
    /// Two complementary halves:
    ///   * the [`FrameHarness`] (the #75 deterministic harness the plan names)
    ///     proves the client mirror converges on the server screen after EVERY
    ///     delivered-and-acked step;
    ///   * a [`FrameProducer`] proves the *body sequence* the harness cannot
    ///     expose — a single keyframe, then only `Diff`/`Morph` — i.e. the base
    ///     is never re-keyframed mid-stream the way a lost base would force.
    fn reliable_is_degenerate(sync: FrameSync) {
        let steps = degenerate_script();

        // (a) Convergence at every step over the lossless, immediate-ack harness.
        let mut h = FrameHarness::new(24, 80, sync);
        for step in &steps {
            h.feed(step);
            assert!(h.deliver(), "a frame must be pending to deliver");
            h.ack();
            h.assert_converged();
        }

        // (b) The same script through the shared FrameProducer, acking each frame
        // at once: collect the body kinds (and re-verify convergence through the
        // real client-side applier inside the driver).
        let bodies = drive_producer_immediate(sync, &steps);
        assert_eq!(bodies.len(), steps.len(), "exactly one body per step");
        let kinds: Vec<&str> = bodies.iter().map(body_kind).collect();

        // The degenerate invariant common to both codecs: no body after the first
        // is a keyframe. A lossless channel never loses the base, so the producer
        // never falls back to a forced `Full` (or `Empty`) mid-stream.
        assert!(
            bodies[1..]
                .iter()
                .all(|b| matches!(b, FrameBody::Diff { .. } | FrameBody::Morph { .. })),
            "every body after the first must be incremental over a lossless channel: {kinds:?}"
        );

        match sync {
            FrameSync::DumpDiff => {
                // Against the empty frame-0 base a DumpDiff is never a net win, so
                // the first frame is the one and only `Full`; every later edit is a
                // `Diff` against the held base. Exactly the "Full once, then Diff"
                // shape RFC 0008 §2 pins.
                assert!(
                    matches!(bodies[0], FrameBody::Full(_)),
                    "DumpDiff: the first frame is a Full keyframe, got {}",
                    kinds[0]
                );
                assert_eq!(
                    bodies.iter().filter(|b| matches!(b, FrameBody::Full(_))).count(),
                    1,
                    "DumpDiff: exactly one Full over a lossless channel: {kinds:?}"
                );
                assert!(
                    bodies[1..].iter().all(|b| matches!(b, FrameBody::Diff { .. })),
                    "DumpDiff: every body after the keyframe is a Diff: {kinds:?}"
                );
            }
            FrameSync::Morph => {
                // The producer starts from a blank frame-0 morph base, and every
                // step here is morph-expressible (no alt-screen toggle, no resize),
                // so the base is always held and EVERY body is a `Morph` — zero
                // forced `Full`s, an even stronger statement of the degenerate
                // thesis than "one keyframe then incremental".
                assert!(
                    bodies.iter().all(|b| matches!(b, FrameBody::Morph { .. })),
                    "Morph: every body is a Morph, zero forced Fulls: {kinds:?}"
                );
            }
        }
    }

    #[test]
    fn reliable_transport_is_degenerate_dumpdiff() {
        reliable_is_degenerate(FrameSync::DumpDiff);
    }

    #[test]
    fn reliable_transport_is_degenerate_morph() {
        reliable_is_degenerate(FrameSync::Morph);
    }
}
