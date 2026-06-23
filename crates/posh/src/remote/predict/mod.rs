//! Speculative local echo (port of mosh's PredictionEngine from
//! terminaloverlay.cc), split along two independent seams:
//!
//! - a [`Predictor`] *model* — keystrokes→overlay machinery + validation
//!   lifecycle (epochs, credit, cull) and the visibility gate; it knows WHAT
//!   is predicted and WHETHER it is currently showable;
//! - a [`PredictionRenderer`] *render style* — how one already-decided-visible
//!   prediction is painted (glyph replace + underline, dim, …).
//!
//! Models and render styles are selected independently from the environment
//! (`POSH_PREDICTION_MODEL` + `POSH_PREDICTION_RENDER`) and combined by
//! [`build`]. Frame numbers from mosh map onto the reliable input stream's byte
//! offsets: a prediction made for the byte at offset B expires at B+1 (the
//! server's ack of B+1 means it consumed that byte), the "acked" counter is the
//! frame's `input_ack`, and the "late acked" counter is the frame's `echo_ack`
//! (state reflecting the application's echo).

use posh_term::Cell;

use crate::remote::display::Snapshot;

mod mosh;
mod optimistic;
mod overlay;
mod render;
#[cfg(test)]
mod test_support;

pub use mosh::MoshPredictor;
pub use optimistic::OptimisticPredictor;
pub use render::{DimRenderer, ReplaceRenderer};

// Timing constants, verbatim from mosh terminaloverlay.h. Used by the mosh
// model; re-exported where tests reference them.
const SRTT_TRIGGER_LOW: u64 = 20; // <= ms cures the SRTT trigger
const SRTT_TRIGGER_HIGH: u64 = 30; // > ms starts the SRTT trigger
const FLAG_TRIGGER_LOW: u64 = 50; // <= ms cures flagging
const FLAG_TRIGGER_HIGH: u64 = 80; // > ms starts flagging
pub const GLITCH_THRESHOLD: u64 = 250; // prediction outstanding this long is a glitch
pub const GLITCH_REPAIR_COUNT: u32 = 10; // non-glitches required to cure the trigger
const GLITCH_REPAIR_MININTERVAL: u64 = 150; // ms between counted non-glitches
pub const GLITCH_FLAG_THRESHOLD: u64 = 5000; // outstanding this long => underline

/// The prediction model: keystrokes -> predictions, reconciliation against
/// server frames. Knows WHAT is predicted and WHETHER it is currently
/// showable; nothing about how it looks.
pub trait Predictor: Send {
    /// Records the reliable-input offset the next keystroke is sent at
    /// (mosh's local_frame_sent). Input path.
    fn set_frame_sent(&mut self, offset: u64);
    /// Feeds one user keystroke byte; `fb` is the locally displayed frame.
    fn on_user_byte(&mut self, byte: u8, fb: &Snapshot, now: u64);
    /// Folds one server frame's acks + send-interval into the model
    /// (mosh's local_frame_acked / local_frame_late_acked / send_interval).
    fn on_server_frame(&mut self, input_ack: u64, echo_ack: u64, send_interval: u64);
    /// Generalizes the optimistic alt-screen/ECHO gate: when `safe` is false
    /// the optimistic model drops its overlay; other models ignore it.
    fn set_echo_safe(&mut self, safe: bool);
    /// Validates predictions against the latest server framebuffer.
    fn cull(&mut self, fb: &Snapshot, now: u64);
    /// Overlays the surviving, currently-shown predictions onto `fb`,
    /// painting each through `renderer`.
    fn render(&self, fb: &mut Snapshot, renderer: &dyn PredictionRenderer);
    fn reset(&mut self);
    /// Any prediction outstanding at all?
    fn active(&self) -> bool;
    /// True when timing-based triggers may still fire and the caller should
    /// poll with a short timeout so glitches get detected.
    fn needs_timer(&self) -> bool;
    /// Instantaneous + cumulative display gauges for the stats log.
    fn stats(&self) -> PredictorStats;
}

/// The render UX: how one already-decided-visible prediction is painted.
/// Allocation-free (the model walks; the renderer mutates the cell), so dyn
/// dispatch cost is per painted cell, negligible.
pub trait PredictionRenderer: Send {
    fn paint_cell(&self, fb: &mut Snapshot, row: u16, col: u16, replacement: &Cell, hint: CellHint);
    fn paint_cursor(&self, fb: &mut Snapshot, row: u16, col: u16);
}

/// Model state a renderer MAY use when painting a cell: `flagged` =
/// slow-link/glitch, `unknown` = uncertain position (no glyph to draw).
#[derive(Clone, Copy)]
pub struct CellHint {
    pub flagged: bool,
    pub unknown: bool,
}

/// Display gauges sampled from a predictor (mirrors the old engine getters).
/// `active`/`shown_cells`/`epoch_lag` are instantaneous; the rest are
/// cumulative counters. `outcomes` is (correct, nocredit, incorrect).
pub struct PredictorStats {
    pub active: bool,
    pub shown_cells: u64,
    pub epoch_lag: u64,
    pub mispredict_resets: u64,
    pub outcomes: (u64, u64, u64),
    /// `outcomes.1` (nocredit) split by cause: (unknown, blank, matched_original).
    /// `matched_original` dominating is the field credit-starvation signature
    /// (#predict-echo).
    pub nocredit_reasons: (u64, u64, u64),
    pub srtt_trigger: bool,
}

/// Prediction model selection. Mirrors mosh's display-preference set; the
/// adaptive/always/never/experimental variants drive [`MoshPredictor`], while
/// optimistic drives [`OptimisticPredictor`] (FDR 0006).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredictionModel {
    Always,
    Never,
    Adaptive,
    Experimental,
    Optimistic,
}

impl PredictionModel {
    /// Parses `$POSH_PREDICTION_MODEL`, falling back to the deprecated
    /// `$POSH_PREDICTION` alias when `_MODEL` is unset (mosh:
    /// `$MOSH_PREDICTION_DISPLAY`). Both share the same value set.
    pub fn parse(value: Option<&str>) -> Result<PredictionModel, String> {
        match value {
            None | Some("") | Some("adaptive") => Ok(PredictionModel::Adaptive),
            Some("always") => Ok(PredictionModel::Always),
            Some("never") => Ok(PredictionModel::Never),
            Some("experimental") => Ok(PredictionModel::Experimental),
            Some("optimistic") => Ok(PredictionModel::Optimistic),
            Some(other) => Err(format!("unknown prediction model ({other})")),
        }
    }
}

/// Prediction render-style selection (`$POSH_PREDICTION_RENDER`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderStyle {
    /// Today's look: replace the glyph, underline when flagged.
    Replace,
    /// Replace the glyph but mark predicted cells with a dim/faint rendition
    /// instead of an underline.
    Dim,
}

impl RenderStyle {
    /// Parses `$POSH_PREDICTION_RENDER` (default `replace`).
    pub fn parse(value: Option<&str>) -> Result<RenderStyle, String> {
        match value {
            None | Some("") | Some("replace") => Ok(RenderStyle::Replace),
            Some("dim") => Ok(RenderStyle::Dim),
            Some(other) => Err(format!("unknown prediction render style ({other})")),
        }
    }
}

/// Combines a parsed model + render style into the boxed trait objects the
/// client holds. `predict_overwrite` (mosh insert-vs-overwrite) threads into
/// the model.
pub fn build(
    model: PredictionModel,
    render: RenderStyle,
    predict_overwrite: bool,
) -> (Box<dyn Predictor>, Box<dyn PredictionRenderer>) {
    let predictor: Box<dyn Predictor> = match model {
        PredictionModel::Optimistic => Box::new(OptimisticPredictor::new(predict_overwrite)),
        other => Box::new(MoshPredictor::new(other, predict_overwrite)),
    };
    let renderer: Box<dyn PredictionRenderer> = match render {
        RenderStyle::Replace => Box::new(ReplaceRenderer),
        RenderStyle::Dim => Box::new(DimRenderer),
    };
    (predictor, renderer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prediction_model_parsing() {
        assert_eq!(PredictionModel::parse(None), Ok(PredictionModel::Adaptive));
        assert_eq!(
            PredictionModel::parse(Some("always")),
            Ok(PredictionModel::Always)
        );
        assert_eq!(
            PredictionModel::parse(Some("never")),
            Ok(PredictionModel::Never)
        );
        assert_eq!(
            PredictionModel::parse(Some("experimental")),
            Ok(PredictionModel::Experimental)
        );
        assert_eq!(
            PredictionModel::parse(Some("optimistic")),
            Ok(PredictionModel::Optimistic)
        );
        assert!(PredictionModel::parse(Some("sometimes")).is_err());
    }

    #[test]
    fn render_style_parsing() {
        assert_eq!(RenderStyle::parse(None), Ok(RenderStyle::Replace));
        assert_eq!(RenderStyle::parse(Some("replace")), Ok(RenderStyle::Replace));
        assert_eq!(RenderStyle::parse(Some("dim")), Ok(RenderStyle::Dim));
        assert!(RenderStyle::parse(Some("sparkly")).is_err());
    }
}
