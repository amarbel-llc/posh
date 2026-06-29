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

/// The server end: a `Terminal` advanced by fed bytes, a frame encoder, and the
/// retained baselines its incremental bodies anchor against.
pub struct ServerSide {
    term: Terminal,
    enc: Box<dyn FrameEncoder>,
    next_num: u64,
    /// The client-acked baseline the encoder diffs against (`None` until the
    /// first frame is acked → that frame is a `Full`).
    acked: Option<Baseline>,
    /// Every produced-but-not-yet-superseded frame's baseline, so an ack for
    /// any of them can become the new `acked` base.
    produced: VecDeque<Baseline>,
}

impl ServerSide {
    fn new(rows: u16, cols: u16, sync: FrameSync) -> ServerSide {
        ServerSide {
            term: Terminal::with_scrollback(rows, cols, 0),
            enc: encoder_for(sync),
            next_num: 1,
            acked: None,
            produced: VecDeque::new(),
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

    /// Advance the server terminal by `bytes` and enqueue one frame toward the
    /// client, encoded against the currently-acked baseline.
    fn feed(&mut self, bytes: &[u8], channel: &mut impl FrameChannel) {
        self.term.process(bytes);
        let _ = self.term.take_responses();

        let num = self.next_num;
        self.next_num += 1;
        let dump = self.term.dump_vt();
        let snapshot = Snapshot::from_term(&self.term);
        let cur = CurrentFrame {
            dump: &dump,
            snapshot: &snapshot,
            alt_screen: self.term.is_alt_screen(),
            rows: self.term.rows(),
            cols: self.term.cols(),
        };
        let body = self.enc.encode(self.acked.as_ref(), &cur);
        self.produced.push_back(self.baseline_now(num));
        channel.send_frame(ServerFrame {
            flags: 0,
            caps: vec![],
            frame_num: num,
            input_ack: 0,
            echo_ack: 0,
            body,
        });
    }

    /// Drain every ack the channel has delivered, advancing the acked baseline
    /// to the highest acknowledged frame the server still retains.
    fn drain_acks(&mut self, channel: &mut impl FrameChannel) {
        while let Some(ack) = channel.recv_ack() {
            if let Some(base) = self.produced.iter().find(|b| b.num == ack.acked_frame) {
                self.acked = Some(base.clone());
            }
            while self.produced.front().is_some_and(|b| b.num < ack.acked_frame) {
                self.produced.pop_front();
            }
        }
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
    fn new(rows: u16, cols: u16, sync: FrameSync) -> ClientSide {
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
            FrameBody::Scrollback { .. } => {}
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

/// The in-memory network between the two ends. The server talks to it as a
/// [`FrameChannel`]; the harness drives delivery scheduling through the
/// inherent `deliver`/`drop_next` knobs. Owns the [`ClientSide`] because
/// delivering a frame is what makes the client apply it and produce an ack.
pub struct TestChannel {
    to_client: VecDeque<ServerFrame>,
    to_server: VecDeque<ClientAck>,
    client: ClientSide,
}

impl FrameChannel for TestChannel {
    fn send_frame(&mut self, frame: ServerFrame) {
        self.to_client.push_back(frame);
    }

    fn recv_ack(&mut self) -> Option<ClientAck> {
        self.to_server.pop_front()
    }
}

impl TestChannel {
    /// Deliver the next pending frame to the client; the client applies it and
    /// queues its ack. Returns false when nothing is pending.
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

    /// Drop the next pending frame (UDP loss). Returns false when nothing is
    /// pending.
    fn drop_next(&mut self) -> bool {
        self.to_client.pop_front().is_some()
    }
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
    channel: TestChannel,
}

impl FrameHarness {
    pub fn new(rows: u16, cols: u16, sync: FrameSync) -> FrameHarness {
        FrameHarness {
            server: ServerSide::new(rows, cols, sync),
            channel: TestChannel {
                to_client: VecDeque::new(),
                to_server: VecDeque::new(),
                client: ClientSide::new(rows, cols, sync),
            },
        }
    }

    /// Advance the server by `bytes` and enqueue one frame toward the client.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.server.feed(bytes, &mut self.channel);
    }

    /// Deliver the next pending frame to the client (it applies + acks).
    pub fn deliver(&mut self) -> bool {
        self.channel.deliver()
    }

    /// Drop the next pending frame, modelling UDP loss.
    pub fn drop_next(&mut self) -> bool {
        self.channel.drop_next()
    }

    /// Let the server learn every ack the client has made available. Withhold
    /// this between feeds to model ack-lag.
    pub fn ack(&mut self) {
        self.server.drain_acks(&mut self.channel);
    }

    /// Number of frames sent but not yet delivered or dropped.
    pub fn pending_frames(&self) -> usize {
        self.channel.to_client.len()
    }

    /// The server's authoritative visible screen state (cells, cursor, modes).
    pub fn server_snapshot(&self) -> Snapshot {
        Snapshot::from_term(&self.server.term)
    }

    /// The client mirror's visible screen state.
    pub fn client_snapshot(&self) -> Snapshot {
        Snapshot::from_term(&self.channel.client.term)
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
            String::from_utf8_lossy(&self.channel.client.term.dump_vt()),
        );
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
}
