//! Transport-agnostic visible-frame producer (#100 unified-session prep): the
//! frame-numbering + acked-base + `outstanding` + encoder-selection state
//! machine lifted verbatim out of `posh`'s `remote::server`. The roaming server
//! drives it today; the session daemon (per client) and the deterministic
//! harnesses reuse it tomorrow, so the loss machinery (`outstanding` /
//! retransmit re-encode) is shared rather than re-derived — over a reliable
//! transport it simply idles (the peer acks at once), which is the
//! reliable-as-degenerate thesis, not dead weight.
//!
//! ## Boundary
//!
//! The producer owns the per-frame state both visible AND scrollback frames
//! advance (`ProducedFrame`, `current`, `outstanding`, the acked baseline), plus
//! the swappable encoders. It does NOT own the scrollback *body* (the ring
//! reads), `acked_sb_total`/`sb_floor`/`sb_high`, the base-checksum stamping,
//! the per-strategy stats sampling, or the `ServerFrame` assembly (flags, caps,
//! input/echo acks) — those stay in `server.rs` where the live `Terminal`,
//! `Stats`, and transport live. A scrollback frame still advances the shared
//! frame-slot machinery through [`FrameProducer::advance_scrollback`]; the
//! caller builds the `Scrollback` body itself from [`FrameProducer::acked_num`]
//! and [`FrameProducer::current_sb_total`].
//!
//! Codec choice is per frame, not per session: the producer holds both encoders
//! and [`FrameProducer::encode_visible`] selects on each call, mirroring the
//! server's per-message `CAP_MORPH` reading.

use crate::display::Snapshot;
use crate::frame::FrameBody;

use super::{Baseline, CurrentFrame, DumpDiff, FrameEncoder, MorphDelta};

/// One produced frame's retained state — the diff/morph base a later frame is
/// encoded against, and the unit of `current`/`outstanding`. Visible and
/// scrollback frames share the struct (a scrollback frame inherits the visible
/// fields and only advances `sb_total`), so it is owned by the producer rather
/// than split.
struct ProducedFrame {
    num: u64,
    /// The visible-screen `dump_vt` bytes as of this frame — the diff base for a
    /// later `Diff`. A scrollback frame leaves the visible screen unchanged, so
    /// it records the same visible bytes as the frame before it, keeping the
    /// diff-base chain intact across interleaved scrollback frames.
    data: Vec<u8>,
    /// The rendered screen state as of this frame — the morph base for a later
    /// `Morph` (#15), captured alongside `data` so acking this frame gives the
    /// MorphDelta encoder both bases. Inherited identically by a scrollback frame.
    snapshot: Snapshot,
    /// Off-`Snapshot` terminal state at this frame: whether the alt screen is
    /// active and the dimensions. The MorphDelta encoder reads these to detect a
    /// transition a morph cannot express (alt-screen toggle, resize) and fall
    /// back to a `Full` keyframe (#15).
    alt_screen: bool,
    dims: (u16, u16),
    /// Scrollback rows the client will have accumulated after applying this
    /// frame (RFC 0002): the running high-water that only advances on a
    /// scrollback frame. Acking this frame tells the server the client holds
    /// scrollback through here, so the next body's appended count starts from it.
    sb_total: u64,
}

/// The visible-frame production state machine: frame numbering, the acked
/// baseline (byte-diff dump + morph snapshot + off-`Snapshot` alt/dims), the
/// retained `outstanding` window for retransmission, and the two swappable
/// encoders. See the module docs for the boundary against `server.rs`.
pub struct FrameProducer {
    /// The in-flight (not-yet-acked-newest) frame: its number is the next frame
    /// number sent, and its fields are the diff/morph base the *next* frame is
    /// encoded against once acked.
    current: ProducedFrame,
    /// Last frame the client confirmed.
    acked_num: u64,
    /// The acked frame's `dump_vt` bytes (the byte-diff base), or `None` when the
    /// server no longer holds the acked frame's state and must send a `Full`.
    acked_data: Option<Vec<u8>>,
    /// The acked frame's morph base — rendered snapshot + off-`Snapshot`
    /// alt-screen/dims. `Some` exactly when `acked_data` is.
    acked_baseline: Option<(Snapshot, bool, (u16, u16))>,
    /// Frames sent but not yet superseded by the acked-newest, retained so an
    /// ack landing on one of them can recover its base. Bounded to the most
    /// recent 8.
    outstanding: Vec<ProducedFrame>,
    /// The newest VISIBLE frame's number, and whether the client ever acked it
    /// DIRECTLY (`acked_frame == last_visible_num`) rather than leaping past it
    /// via a later scrollback frame's ack. An ack past a never-directly-acked
    /// visible frame is the #95/#117 leap signature: the client may have
    /// silently dropped that frame's content, and with the model idle no new
    /// frame would ever re-deliver it — the caller breaks that quiescence with
    /// one forced visible frame (`visible_frame_leapt`).
    last_visible_num: u64,
    visible_directly_acked: bool,
    dumpdiff: DumpDiff,
    morph: MorphDelta,
}

impl FrameProducer {
    /// A fresh producer at frame 0 — the implicit empty initial state shared
    /// with the client, so the very first real frame can already be expressed
    /// against it. `acked_data`/`acked_baseline` start `Some` (the empty/blank
    /// base).
    pub fn new(rows: u16, cols: u16) -> FrameProducer {
        FrameProducer {
            current: ProducedFrame {
                num: 0,
                data: Vec::new(),
                snapshot: Snapshot::blank(rows, cols),
                alt_screen: false,
                dims: (rows, cols),
                sb_total: 0,
            },
            acked_num: 0,
            acked_data: Some(Vec::new()),
            acked_baseline: Some((Snapshot::blank(rows, cols), false, (rows, cols))),
            outstanding: Vec::new(),
            // Frame 0 is the implicit blank base both sides share: visible and
            // attested by construction.
            last_visible_num: 0,
            visible_directly_acked: true,
            dumpdiff: DumpDiff,
            morph: MorphDelta::default(),
        }
    }

    /// The current (next-to-send) frame number.
    pub fn current_num(&self) -> u64 {
        self.current.num
    }

    /// The last frame the client confirmed.
    pub fn acked_num(&self) -> u64 {
        self.acked_num
    }

    /// Frames retained for retransmission recovery.
    pub fn outstanding_len(&self) -> usize {
        self.outstanding.len()
    }

    /// Scrollback total the current frame carries (RFC 0002): a visible frame
    /// inherits the acked base's total, a scrollback frame advances it.
    pub fn current_sb_total(&self) -> u64 {
        self.current.sb_total
    }

    /// Byte length of the current frame's visible dump.
    pub fn current_dump_len(&self) -> usize {
        self.current.data.len()
    }

    /// The acked diff base bytes, for the base-checksum stamp (RFC 0006) the
    /// caller applies after encoding.
    pub fn acked_dump(&self) -> Option<&[u8]> {
        self.acked_data.as_deref()
    }

    /// Whether an acked base is held — `false` after a lost base, when the next
    /// `encode_visible` is necessarily a `Full`.
    pub fn has_acked_base(&self) -> bool {
        self.acked_data.is_some()
    }

    /// Advance to a fresh visible frame: retire `current` into `outstanding` and
    /// install the supplied screen state as the new current frame. `sb_total` is
    /// the scrollback total the client holds at the diff base (the caller's
    /// `acked_sb_total`); a visible frame carries no rows of its own.
    pub fn advance_visible(
        &mut self,
        dump: Vec<u8>,
        snapshot: Snapshot,
        alt: bool,
        dims: (u16, u16),
        sb_total: u64,
    ) {
        let num = self.current.num + 1;
        // #117: this is now the newest visible frame; its delivery is
        // unattested until the client acks exactly this number.
        self.last_visible_num = num;
        self.visible_directly_acked = false;
        self.rotate_current(ProducedFrame {
            num,
            data: dump,
            snapshot,
            alt_screen: alt,
            dims,
            sb_total,
        });
    }

    /// Advance to a scrollback frame slot: retire `current` and install a new
    /// current that inherits the CONFIRMED visible base (acked dump + morph base,
    /// falling back to the live current when no base is held) so the diff-base
    /// chain stays unbroken, carrying `sb_total` forward. The caller builds the
    /// `FrameBody::Scrollback` itself.
    pub fn advance_scrollback(&mut self, sb_total: u64) {
        // #95: inherit the CONFIRMED baseline (acked_data / acked_baseline), NOT
        // the latest `current` — under loss the latest visible dump can be ahead
        // of what the client holds, and acking this scrollback frame would then
        // push the diff base past an unapplied visible frame, staling the
        // client's baseline. The caller gates on `has_acked_base()`; the fallback
        // is defensive and preserves the old behavior if that ever changes.
        let (visible, visible_snapshot, visible_alt, visible_dims) =
            match (self.acked_data.clone(), self.acked_baseline.clone()) {
                (Some(d), Some((s, a, dim))) => (d, s, a, dim),
                _ => (
                    self.current.data.clone(),
                    self.current.snapshot.clone(),
                    self.current.alt_screen,
                    self.current.dims,
                ),
            };
        let num = self.current.num + 1;
        self.rotate_current(ProducedFrame {
            num,
            data: visible,
            snapshot: visible_snapshot,
            alt_screen: visible_alt,
            dims: visible_dims,
            sb_total,
        });
    }

    /// Retire the current frame into the retransmission window (bounded to the
    /// most recent 8) and install `next` as the new current. Shared by both
    /// advance paths so the slot machinery is identical for visible and
    /// scrollback frames.
    fn rotate_current(&mut self, next: ProducedFrame) {
        self.outstanding.push(ProducedFrame {
            num: self.current.num,
            data: std::mem::take(&mut self.current.data),
            snapshot: std::mem::replace(&mut self.current.snapshot, Snapshot::blank(1, 1)),
            alt_screen: self.current.alt_screen,
            dims: self.current.dims,
            sb_total: self.current.sb_total,
        });
        if self.outstanding.len() > 8 {
            self.outstanding.remove(0);
        }
        self.current = next;
    }

    /// Encode the current visible frame's body against the acked baseline with
    /// the selected codec (`use_morph` mirrors the per-message `CAP_MORPH`). The
    /// base-checksum stamp and stats sampling stay with the caller.
    pub fn encode_visible(&mut self, use_morph: bool) -> FrameBody {
        // The acked baseline (Some exactly when acked_data is) gives the encoder
        // both the byte-diff base (dump) and the morph base (snapshot +
        // off-Snapshot alt/dims).
        let baseline = self.acked_data.as_ref().zip(self.acked_baseline.as_ref()).map(
            |(dump, (snapshot, alt, dims))| Baseline {
                num: self.acked_num,
                dump: dump.clone(),
                snapshot: snapshot.clone(),
                alt_screen: *alt,
                rows: dims.0,
                cols: dims.1,
            },
        );
        let cur = CurrentFrame {
            dump: &self.current.data,
            snapshot: &self.current.snapshot,
            alt_screen: self.current.alt_screen,
            rows: self.current.dims.0,
            cols: self.current.dims.1,
        };
        if use_morph {
            self.morph.encode(baseline.as_ref(), &cur)
        } else {
            self.dumpdiff.encode(baseline.as_ref(), &cur)
        }
    }

    /// Apply a client ack. Ignores acks for frames never sent (a future or
    /// already-passed frame). On a real ack, advances the acked baseline from
    /// `current`/`outstanding` and prunes superseded outstanding frames; returns
    /// the acked frame's `sb_total` so the caller folds it into `acked_sb_total`.
    /// Returns `None` when the ack is rejected OR the acked frame's state is no
    /// longer held (the lost-base path: the baseline drops to `None` and the next
    /// `encode_visible` is a `Full`).
    pub fn ack(&mut self, acked_frame: u64) -> Option<u64> {
        // Ignore acks for frames never sent: an authenticated client claiming a
        // future frame would otherwise clear `outstanding`, disable retransmits,
        // and satisfy the caller's shutdown gate without confirming the real
        // final state.
        if acked_frame <= self.acked_num || acked_frame > self.current.num {
            return None;
        }
        self.acked_num = acked_frame;
        // #117: only an ack landing EXACTLY on the newest visible frame attests
        // its delivery; an ack beyond it (a later scrollback slot) may have
        // leapt a silently-dropped visible frame.
        if acked_frame == self.last_visible_num {
            self.visible_directly_acked = true;
        }
        // The acked frame's bytes, morph base, and scrollback total, from
        // `current` or the retained outstanding frame.
        let acked = if acked_frame == self.current.num {
            Some((
                self.current.data.clone(),
                self.current.snapshot.clone(),
                self.current.alt_screen,
                self.current.dims,
                self.current.sb_total,
            ))
        } else {
            self.outstanding.iter().find(|f| f.num == acked_frame).map(|f| {
                (
                    f.data.clone(),
                    f.snapshot.clone(),
                    f.alt_screen,
                    f.dims,
                    f.sb_total,
                )
            })
        };
        let acked_sb_total = if let Some((data, snapshot, alt, dims, sb_total)) = acked {
            self.acked_data = Some(data);
            // The morph baseline tracks acked_data exactly (#15): both Some, or
            // both None when we no longer hold the acked frame's state.
            self.acked_baseline = Some((snapshot, alt, dims));
            Some(sb_total)
        } else {
            self.acked_data = None;
            self.acked_baseline = None;
            None
        };
        self.outstanding.retain(|f| f.num >= acked_frame);
        acked_sb_total
    }

    /// Drop the acked baseline so the next `encode_visible` is a forced `Full`
    /// keyframe — the RESYNC unwedge (the caller also forces a frame out).
    pub fn drop_acked_base(&mut self) {
        self.acked_data = None;
        self.acked_baseline = None;
    }

    /// The #95/#117 leap signature: the client's ack has moved PAST the newest
    /// visible frame (via a later scrollback slot) without ever acking it
    /// directly — its content may have been silently dropped (stale/base
    /// mismatch), and with the model idle nothing would re-deliver it. The
    /// caller breaks that quiescence by forcing one fresh visible frame: a
    /// no-op repaint for a healthy client, the missing content for a wedged
    /// one. Self-clearing — the forced frame becomes the newest visible frame,
    /// and its direct ack re-attests delivery.
    pub fn visible_frame_leapt(&self) -> bool {
        self.acked_num > self.last_visible_num && !self.visible_directly_acked
    }

    /// The newest visible frame's number (diagnostics: names the frame a leap
    /// jumped over).
    pub fn last_visible_num(&self) -> u64 {
        self.last_visible_num
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: u16 = 24;
    const COLS: u16 = 80;

    /// A representative `dump_vt`-shaped payload: long enough that a diff against
    /// the empty base is never a win (so the first frame is a `Full`), and a
    /// later small edit against it IS a win (so the second frame is a `Diff`).
    fn screen(line: &str) -> Vec<u8> {
        let mut v = b"\x1b[2J\x1b[H".to_vec();
        for _ in 0..20 {
            v.extend_from_slice(b"filler line of representative screen content\r\n");
        }
        v.extend_from_slice(line.as_bytes());
        v
    }

    /// (a) The first real frame, with only the empty frame-0 base acked, is a
    /// `Full`: a DumpDiff against the empty base is never economical, so the
    /// codec ships the whole dump — today's server behavior.
    #[test]
    fn first_visible_frame_with_no_real_ack_is_full() {
        let mut p = FrameProducer::new(ROWS, COLS);
        let dump = screen("prompt$ ");
        let snap = Snapshot::blank(ROWS, COLS);
        p.advance_visible(dump, snap, false, (ROWS, COLS), 0);
        assert_eq!(p.current_num(), 1, "advancing must bump the frame number");
        assert!(
            matches!(p.encode_visible(false), FrameBody::Full(_)),
            "first frame against the empty base must be a Full"
        );
    }

    /// (b) After acking frame 1, the second frame encodes incrementally against
    /// base 1 — a `Diff` under DumpDiff, a `Morph` under MorphDelta, both
    /// anchored at the acked frame number.
    #[test]
    fn second_frame_after_ack_encodes_against_base_one() {
        let mut p = FrameProducer::new(ROWS, COLS);
        p.advance_visible(screen("prompt$ "), Snapshot::blank(ROWS, COLS), false, (ROWS, COLS), 0);
        // Encoding the first frame does not advance state; ack it to make frame 1
        // the diff base.
        let _ = p.encode_visible(false);
        assert_eq!(p.ack(1), Some(0), "acking frame 1 confirms its (zero) sb_total");

        // A second frame: a small edit on the same screen so the diff is a win.
        p.advance_visible(screen("prompt$ ls"), Snapshot::blank(ROWS, COLS), false, (ROWS, COLS), 0);
        assert_eq!(p.current_num(), 2);
        match p.encode_visible(false) {
            FrameBody::Diff { base, .. } => assert_eq!(base, 1, "DumpDiff anchors at base 1"),
            other => panic!("expected a Diff against base 1, got {other:?}"),
        }
        match p.encode_visible(true) {
            FrameBody::Morph { base, .. } => assert_eq!(base, 1, "MorphDelta anchors at base 1"),
            other => panic!("expected a Morph against base 1, got {other:?}"),
        }
    }

    /// (c) Acking a frame that has aged out of `outstanding` is the lost-base
    /// path: the producer no longer holds that frame's bytes, so the baseline
    /// drops to `None` and the next `encode_visible` is a forced `Full` — exactly
    /// `server.rs`'s `update_acks` setting `acked_data = None`.
    #[test]
    fn ack_of_evicted_frame_loses_base_and_forces_full() {
        let mut p = FrameProducer::new(ROWS, COLS);
        // Advance well past the 8-frame outstanding window without acking, so the
        // early frames are evicted.
        for i in 0..10 {
            let body = screen(&format!("frame {i}"));
            p.advance_visible(body, Snapshot::blank(ROWS, COLS), false, (ROWS, COLS), 0);
        }
        assert_eq!(p.current_num(), 10);

        // Frame 1 is a legitimate (unacked, <= current) frame, but it has aged
        // out of the retained window — acking it cannot recover a base.
        assert_eq!(p.ack(1), None, "an evicted frame's ack recovers no base");
        assert!(!p.has_acked_base(), "the base must drop after a lost-base ack");
        assert!(
            matches!(p.encode_visible(false), FrameBody::Full(_)),
            "with no base the next frame must be a Full"
        );
    }

    /// The `update_acks` coverage moved verbatim from `server.rs`: an ack for a
    /// frame never sent is ignored (state held, `None` returned), and a real ack
    /// of the newest frame advances the baseline and returns its `sb_total`.
    /// Pins the reject/retain and advance semantics the producer now owns.
    #[test]
    fn ack_rejects_frames_never_sent_then_advances_on_a_real_ack() {
        let frame = |num: u64, data: &[u8], sb_total: u64| ProducedFrame {
            num,
            data: data.to_vec(),
            snapshot: Snapshot::blank(ROWS, COLS),
            alt_screen: false,
            dims: (ROWS, COLS),
            sb_total,
        };
        let mut p = FrameProducer {
            current: frame(3, b"current", 7),
            acked_num: 1,
            acked_data: Some(b"one".to_vec()),
            acked_baseline: Some((Snapshot::blank(ROWS, COLS), false, (ROWS, COLS))),
            outstanding: vec![frame(1, b"one", 2), frame(2, b"two", 5)],
            last_visible_num: 3,
            visible_directly_acked: false,
            dumpdiff: DumpDiff,
            morph: MorphDelta::default(),
        };

        // Ack for a frame never sent: ignored, nothing moves, no sb_total.
        assert_eq!(p.ack(u64::MAX), None, "ack for a frame never sent is ignored");
        assert_eq!(p.acked_num, 1);
        assert_eq!(p.acked_data.as_deref(), Some(b"one".as_slice()));
        assert_eq!(p.outstanding.len(), 2, "outstanding frames are kept");

        // A legitimate ack of the newest frame advances and carries its
        // scrollback coverage forward (RFC 0002 §2).
        assert_eq!(p.ack(3), Some(7), "a real ack returns the frame's sb_total");
        assert_eq!(p.acked_num, 3);
        assert_eq!(p.acked_data.as_deref(), Some(b"current".as_slice()));
        assert!(p.acked_baseline.is_some(), "the morph baseline tracks acked_data");
        assert!(p.outstanding.is_empty());
    }

    /// #117: the leap detector. A direct ack of the newest visible frame
    /// attests its delivery; an ack that jumps past it via a later scrollback
    /// slot leaves it unattested (`visible_frame_leapt`), and producing a fresh
    /// visible frame self-clears the signal.
    #[test]
    fn visible_frame_leapt_tracks_direct_acks() {
        let mut p = FrameProducer::new(ROWS, COLS);
        let snap = || Snapshot::blank(ROWS, COLS);
        assert!(!p.visible_frame_leapt(), "frame 0 is attested by construction");

        // Visible frame 1, acked directly: attested, no leap.
        p.advance_visible(screen("a"), snap(), false, (ROWS, COLS), 0);
        assert!(!p.visible_frame_leapt(), "unacked but not yet leapt");
        assert_eq!(p.ack(1), Some(0));
        assert!(!p.visible_frame_leapt(), "direct ack attests delivery");

        // Visible frame 2 then scrollback frame 3; the client's cumulative ack
        // lands on 3 only — the #95 leap: frame 2 was never directly acked.
        p.advance_visible(screen("b"), snap(), false, (ROWS, COLS), 0);
        p.advance_scrollback(5);
        assert_eq!(p.last_visible_num(), 2);
        assert_eq!(p.ack(3), Some(5));
        assert!(p.visible_frame_leapt(), "ack 3 leapt visible frame 2");

        // The nudge produces a fresh visible frame: the signal self-clears
        // (nothing to re-force while it is in flight)...
        p.advance_visible(screen("b"), snap(), false, (ROWS, COLS), 5);
        assert!(!p.visible_frame_leapt(), "a fresh visible frame clears the leap");
        // ...and its direct ack re-attests delivery.
        assert_eq!(p.ack(4), Some(5));
        assert!(!p.visible_frame_leapt());
    }
}
