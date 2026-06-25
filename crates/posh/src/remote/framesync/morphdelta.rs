//! `MorphDelta` codec (#15, the prototype): instead of shipping a full dump
//! for the client to reparse, the server ships the minimal forward escape-delta
//! (`display::new_frame`) that morphs the client's existing terminal model from
//! the acked frame to the current one. The client applies it with
//! `server_term.process(&escapes)` — no fresh `Terminal`, no O(whole-screen)
//! reparse — then re-`dump_vt`s the updated model so a later `Diff`/`Morph`
//! byte-diff base stays valid.
//!
//! ## Keyframe boundaries (the spike's critical refinement)
//!
//! `new_frame` diffs `Snapshot`s and cannot express state a `Snapshot` does not
//! hold. The encoder MUST fall back to a `Full` keyframe when the acked→current
//! transition is not morph-expressible:
//!   * **no baseline** — first frame, or post-loss (the server dropped the
//!     acked frame's state);
//!   * **alt-screen toggle** — `Snapshot` has no alt-screen field, so a morph
//!     would leave the client's buffer on the wrong screen and desync
//!     `is_alt_screen()` (which gates the wheel scroll-view and echo);
//!   * **dimensions changed** (resize) — `new_frame` would emit a self-clearing
//!     full repaint anyway, and the off-screen model state must be rebuilt.
//! The client's `server_term` therefore stays a faithful mirror across every
//! Morph, and any non-expressible jump is carried by a `Full` the DumpDiff
//! applier rebuilds from scratch.

use posh_term::Terminal;

use crate::remote::display;
use crate::remote::sync::FrameBody;

use super::dumpdiff::DumpDiff;
use super::{ApplyOutcome, Baseline, CurrentFrame, FrameApplier, FrameEncoder};

#[derive(Default)]
pub struct MorphDelta {
    /// The keyframe/full-dump path is literally DumpDiff: a forced `Full` (no
    /// baseline, alt-screen toggle, resize) and the client-side reparse of a
    /// `Full` are exactly its behavior, so MorphDelta delegates rather than
    /// duplicating it.
    fallback: DumpDiff,
}

/// Whether the acked→current transition can be expressed as a forward morph,
/// or must be a `Full` keyframe. Pulled out so the encoder-chooses-Full tests
/// can assert the boundary directly.
pub(super) fn morph_expressible(acked: &Baseline, cur: &CurrentFrame<'_>) -> bool {
    // `new_frame` syncs only the fields a Snapshot carries. Alt-screen and
    // dimensions are not among them, so a transition that changes either is not
    // morph-expressible and must be a keyframe.
    acked.alt_screen == cur.alt_screen && acked.rows == cur.rows && acked.cols == cur.cols
}

impl FrameEncoder for MorphDelta {
    fn encode(&mut self, acked: Option<&Baseline>, cur: &CurrentFrame<'_>) -> FrameBody {
        match acked {
            Some(base) if morph_expressible(base, cur) => {
                // initialized=true => a forward diff (morph), not a full repaint.
                let escapes = display::new_frame(true, &base.snapshot, cur.snapshot, false);
                FrameBody::Morph {
                    base: base.num,
                    base_sum: None, // server fills it when CAP_BASE_SUM is negotiated
                    escapes,
                }
            }
            // No baseline, or a non-morph-expressible transition: keyframe.
            _ => self.fallback.encode(None, cur),
        }
    }
}

impl FrameApplier for MorphDelta {
    fn apply(
        &mut self,
        rows: u16,
        cols: u16,
        applied_data: &[u8],
        server_term: &mut Terminal,
        body: &FrameBody,
    ) -> ApplyOutcome {
        match body {
            FrameBody::Morph { base: _, escapes, .. } => {
                // The caller has already confirmed base == applied_num before
                // dispatching here. Apply the forward morph to the EXISTING
                // model — the whole point of the codec. No `dump_vt` refresh and
                // no resize: a Morph is only emitted when dims are unchanged
                // (`morph_expressible`), and its base is the frame number plus
                // the server's acked snapshot, not the client's dump bytes — so
                // re-dumping the whole screen here would reintroduce the
                // O(whole-screen) per-frame cost #15 removes.
                let _ = (rows, cols);
                server_term.process(escapes);
                ApplyOutcome::AdvancedNoDump
            }
            // Full/Diff/Empty keyframes are the DumpDiff path verbatim.
            _ => self
                .fallback
                .apply(rows, cols, applied_data, server_term, body),
        }
    }
}

/// Test helper: a [`Baseline`] from a frame number and the terminal at that
/// frame. The production server builds its baseline from the acked
/// `FrameState` fields directly (`server.rs`), so this is test-only.
#[cfg(test)]
pub(super) fn baseline_from(num: u64, term: &Terminal) -> Baseline {
    Baseline {
        num,
        dump: term.dump_vt(),
        snapshot: crate::remote::display::Snapshot::from_term(term),
        alt_screen: term.is_alt_screen(),
        rows: term.rows(),
        cols: term.cols(),
    }
}
