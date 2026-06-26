//! RFC 0007 §4: the two evolved GP predictor species, user-selectable via
//! `POSH_PREDICTION_MODEL=controller` / `=scratch`.
//!
//! - [`ControllerPredictor`] (the safe arm) — an evolved program maps the metric
//!   vector to bounded [`PolicyKnobs`] that drive posh's existing echo machinery.
//! - [`FromScratchPredictor`] (the research arm) — an evolved program emits the
//!   predicted cells directly.
//!
//! Both wrap an inner `adaptive` [`MoshPredictor`] as the RFC 0007 §7.1 shadow
//! baseline. While no evolved program is wired (the `program` seam is `None`),
//! each delegates entirely to the shadow, so selecting either model today
//! behaves exactly like `adaptive` — the permanent floor the GP must beat
//! before its output is ever shown.
//!
//! The mephisto genome plugs in behind the `*Program` seams. On mephisto's side
//! the genome is NOT a single-root DSP program: the controller needs four typed
//! outputs and from-scratch a bounded cell list. See RFC 0007 §4 and the
//! genome-shape note — the controller genome is a fixed tuple of single-root
//! programs (one per knob); from-scratch's variable-length output is deferred.

// Scaffold contract surface (RFC 0007 §4): the evolved-program seams and output
// types are referenced once the mephisto genome is wired. Allow until then.
#![allow(dead_code)]

use crate::remote::display::Snapshot;

use super::metric::MetricVector;
use super::{MoshPredictor, PredictionModel, PredictionRenderer, Predictor, PredictorStats};

/// RFC 0007 §4.1 controller output: bounded policy knobs the existing
/// overlay/cull/render pipeline consumes. The controller MUST NOT emit cells
/// itself; out-of-range scalars are clamped, never rejected.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PolicyKnobs {
    /// Display the prediction this tick at all.
    pub show: bool,
    /// Render the prediction flagged (underline/dim).
    pub flag: bool,
    /// Effective confirmation hold before a prediction is shown, clamped to
    /// `[0, 5000]` ms by [`PolicyKnobs::clamped`].
    pub confirm_gate_ms: f64,
    /// Drop the prediction when the local frame already matches (the
    /// autosuggestion case).
    pub suppress_on_ambiguity: bool,
}

impl PolicyKnobs {
    /// The knob set that reproduces adaptive-like behavior: show, don't flag,
    /// no extra gate, no ambiguity suppression. The seam default until an
    /// evolved program overrides it.
    pub fn adaptive_like() -> PolicyKnobs {
        PolicyKnobs {
            show: true,
            flag: false,
            confirm_gate_ms: 0.0,
            suppress_on_ambiguity: false,
        }
    }

    /// Clamp out-of-range scalar fields into their RFC 0007 §4.1 ranges.
    pub fn clamped(self) -> PolicyKnobs {
        PolicyKnobs {
            confirm_gate_ms: self.confirm_gate_ms.clamp(0.0, 5000.0),
            ..self
        }
    }
}

/// RFC 0007 §4.1 controller seam: an evolved program mapping the metric vector
/// to policy knobs. Backed by a mephisto genome once wired.
pub trait ControllerProgram: Send {
    fn decide(&self, metrics: &MetricVector) -> PolicyKnobs;
}

/// One predicted overlay cell from the from-scratch species (RFC 0007 §4.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayOp {
    pub row: u16,
    pub col: u16,
    pub glyph: char,
}

/// RFC 0007 §4.2 from-scratch output: a bounded list of overlay ops plus an
/// optional predicted cursor position. Lists longer than
/// [`FROM_SCRATCH_OP_CAP`] are truncated, never rejected.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FromScratchOutput {
    pub ops: Vec<OverlayOp>,
    pub cursor: Option<(u16, u16)>,
}

/// Upper bound on overlay ops a from-scratch program may emit per keystroke
/// (RFC 0007 §4.2). A concrete cap; tuned once the species is real.
pub const FROM_SCRATCH_OP_CAP: usize = 4096;

/// RFC 0007 §4.2 from-scratch seam: an evolved program emitting predicted cells
/// from the input byte, the current screen, and the metric vector.
pub trait FromScratchProgram: Send {
    fn predict(&self, byte: u8, screen: &Snapshot, metrics: &MetricVector) -> FromScratchOutput;
}

/// RFC 0007 §4.1 controller species. Delegates to the adaptive shadow until an
/// evolved [`ControllerProgram`] is wired (RFC 0007 §7.1).
pub struct ControllerPredictor {
    shadow: MoshPredictor,
    // TODO(RFC 0007 §4.1/§7): drive policy from this evolved program and choose
    // the better-ranked of {shadow, evolved} for display (§7.1 best-of).
    #[allow(dead_code)]
    program: Option<Box<dyn ControllerProgram>>,
}

impl ControllerPredictor {
    pub fn new(predict_overwrite: bool) -> ControllerPredictor {
        ControllerPredictor {
            shadow: MoshPredictor::new(PredictionModel::Adaptive, predict_overwrite),
            program: None,
        }
    }
}

/// RFC 0007 §4.2 from-scratch species. Delegates to the adaptive shadow until an
/// evolved [`FromScratchProgram`] is wired (RFC 0007 §7.1).
pub struct FromScratchPredictor {
    shadow: MoshPredictor,
    // TODO(RFC 0007 §4.2/§7): emit cells from this evolved program, gated by the
    // runtime safety gate (§5.1) and the §7.1 best-of selector.
    #[allow(dead_code)]
    program: Option<Box<dyn FromScratchProgram>>,
}

impl FromScratchPredictor {
    pub fn new(predict_overwrite: bool) -> FromScratchPredictor {
        FromScratchPredictor {
            shadow: MoshPredictor::new(PredictionModel::Adaptive, predict_overwrite),
            program: None,
        }
    }
}

// Until the evolved programs are wired, both species are pure pass-throughs to
// the adaptive shadow. A macro keeps the two delegations in lockstep.
macro_rules! delegate_to_shadow {
    ($ty:ty) => {
        impl Predictor for $ty {
            fn set_frame_sent(&mut self, offset: u64) {
                self.shadow.set_frame_sent(offset);
            }
            fn on_user_byte(&mut self, byte: u8, fb: &Snapshot, now: u64) {
                self.shadow.on_user_byte(byte, fb, now);
            }
            fn on_server_frame(&mut self, input_ack: u64, echo_ack: u64, send_interval: u64) {
                self.shadow.on_server_frame(input_ack, echo_ack, send_interval);
            }
            fn set_echo_safe(&mut self, safe: bool) {
                self.shadow.set_echo_safe(safe);
            }
            fn cull(&mut self, fb: &Snapshot, now: u64) {
                self.shadow.cull(fb, now);
            }
            fn render(&self, fb: &mut Snapshot, renderer: &dyn PredictionRenderer) {
                self.shadow.render(fb, renderer);
            }
            fn reset(&mut self) {
                self.shadow.reset();
            }
            fn active(&self) -> bool {
                self.shadow.active()
            }
            fn needs_timer(&self) -> bool {
                self.shadow.needs_timer()
            }
            fn stats(&self) -> PredictorStats {
                self.shadow.stats()
            }
        }
    };
}

delegate_to_shadow!(ControllerPredictor);
delegate_to_shadow!(FromScratchPredictor);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_knobs_clamp_confirm_gate() {
        let k = PolicyKnobs {
            confirm_gate_ms: 9999.0,
            ..PolicyKnobs::adaptive_like()
        }
        .clamped();
        assert_eq!(k.confirm_gate_ms, 5000.0);
    }

    #[test]
    fn controller_scaffold_behaves_like_adaptive_floor() {
        // With no evolved program wired, the controller must echo exactly what
        // the adaptive shadow would (RFC 0007 §7.1 floor).
        let mut c = ControllerPredictor::new(false);
        let fb = Snapshot::blank(24, 80);
        c.set_frame_sent(0);
        c.on_user_byte(b'l', &fb, 1000);
        // Delegation is opaque here; the meaningful assertion is that the
        // scaffold constructs and drives without panicking. Behavioral parity
        // with adaptive is covered by the shared PredictHarness once the
        // species are registered there.
        let _ = c.active();
    }
}
