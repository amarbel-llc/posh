//! Speculative local echo (port of mosh's PredictionEngine from
//! terminaloverlay.cc), split along two independent seams:
//!
//! - a [`Predictor`] *model* — keystrokes→overlay machinery + validation
//!   lifecycle (epochs, credit, cull); it knows WHAT is predicted, and hands
//!   the renderer a [`RenderAdvice`] — its RECOMMENDATION on showing (the
//!   adaptive srtt/glitch triggers, the tentative-epoch hold, the slow-link
//!   flag) — which the renderer may honor or disregard;
//! - a [`PredictionRenderer`] *render policy + style* — WHEN a held prediction
//!   appears ([`ShowPolicy`]: always, or as the model advises) and HOW it is
//!   painted (glyph replace + underline, dim, …).
//!
//! The one thing neither axis may override is the safety gate: the client
//! skips rendering entirely while the remote PTY has `ECHO` off or the
//! alternate screen is active (RFC 0007 §5.1), for every model.
//!
//! Models, show policies, and render styles are selected independently from
//! the environment (`POSH_PREDICTION_MODEL`, `POSH_PREDICTION_SHOW`,
//! `POSH_PREDICTION_RENDER`) and combined by [`build`]. Frame numbers from
//! mosh map onto the reliable input stream's byte
//! offsets: a prediction made for the byte at offset B expires at B+1 (the
//! server's ack of B+1 means it consumed that byte), the "acked" counter is the
//! frame's `input_ack`, and the "late acked" counter is the frame's `echo_ack`
//! (state reflecting the application's echo).

use posh_term::Cell;

use crate::remote::display::Snapshot;

mod evolved;
mod metric;
mod mosh;
mod optimistic;
mod overlay;
mod render;
mod species;
#[cfg(test)]
mod test_support;

#[allow(unused_imports)] // scaffold surface, referenced once the GP is wired (RFC 0007)
pub use metric::{
    gather_client_local, MetricSource, MetricVector, METRIC_SCHEMA_VERSION, TERMINAL_COUNT,
};
pub use evolved::ControllerPredictor;
pub use mosh::MoshPredictor;
pub use optimistic::OptimisticPredictor;
pub use render::{DimRenderer, ReplaceRenderer};
#[allow(unused_imports)] // PolicyKnobs referenced by the controller Domain (RFC 0007)
pub use species::{FromScratchPredictor, PolicyKnobs};

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
    /// Feeds the latest assembled metric vector (RFC 0007 §3) to a predictor
    /// that consumes it (the evolved controller). Default: ignored, so the
    /// non-GP models are unaffected.
    fn set_metrics(&mut self, _metrics: &MetricVector) {}
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
    /// Evolution-loop gauges for the evolved GP species (RFC 0007): generation
    /// count, champion rank/size, the §7.1 display decision, and the champion
    /// hyphence-doc record. `None` for the non-evolved models, so the palette's
    /// prediction-stats view can tell "no evolution running" apart from zeros.
    fn evolution(&self) -> Option<EvolutionStats> {
        None
    }
}

/// Gauges of the online evolution loop (RFC 0007 §7), sampled from an evolved
/// predictor for the palette's prediction-stats view and the debug log.
#[derive(Clone, Debug)]
pub struct EvolutionStats {
    /// Generations stepped this session.
    pub generations: u64,
    /// Current population size.
    pub population: usize,
    /// Outcome samples in the fitness window.
    pub window: usize,
    /// The champion's rank at the last evaluation (lower is better; `+inf`
    /// until the first scored window).
    pub champion_rank: f64,
    /// Total nodes across the champion's roots (a bloat/parsimony gauge).
    pub champion_size: u32,
    /// Whether the GP champion (vs the adaptive shadow) is displayed (§7.1).
    pub champion_displayed: bool,
    /// The §7.1 hysteresis counter (positive = champion winning).
    pub champion_streak: i32,
    /// Champion hyphence docs written (deduped) this session (RFC 0007 §8)...
    pub champion_saves: u64,
    /// ...and the most recent doc's path under `$XDG_DATA_HOME`.
    pub last_champion_doc: Option<std::path::PathBuf>,
}

/// The render UX: how one already-decided-visible prediction is painted.
/// Allocation-free (the model walks; the renderer mutates the cell), so dyn
/// dispatch cost is per painted cell, negligible.
pub trait PredictionRenderer: Send {
    fn paint_cell(&self, fb: &mut Snapshot, row: u16, col: u16, replacement: &Cell, hint: CellHint);
    fn paint_cursor(&self, fb: &mut Snapshot, row: u16, col: u16);

    /// Render-axis policy: paint this render step at all? The model's
    /// [`RenderAdvice::show`] is its recommendation (adaptive's srtt/glitch
    /// triggers say "the link is fast, nothing to hide"); the default
    /// disregards it and always paints. [`Policed`] with
    /// [`ShowPolicy::Advised`] honors it.
    fn shows(&self, _advice: &RenderAdvice) -> bool {
        true
    }

    /// Render-axis policy: paint a prediction the model still holds as
    /// TENTATIVE (mosh's "hide a new epoch's first keystroke until the
    /// previous epoch confirms")? The default ignores the hold and paints
    /// immediately; a renderer honoring it uses [`RenderAdvice::confirmed_epoch`].
    fn shows_tentative(&self, _advice: &RenderAdvice) -> bool {
        true
    }

    /// Render-axis policy: mark this step's cells (underline/dim)? The model's
    /// [`RenderAdvice::flag`] is its slow-link/glitch recommendation; the
    /// default marks every predicted cell so a prediction is always visibly
    /// one.
    fn flags(&self, _advice: &RenderAdvice) -> bool {
        true
    }
}

/// What a model tells the renderer about the render step it is offering —
/// recommendations, not decisions. Every field is advisory: the renderer's
/// [`ShowPolicy`] and style decide what actually happens to the screen.
#[derive(Clone, Copy, Debug)]
pub struct RenderAdvice {
    /// The model recommends painting this step at all. `false` from the
    /// adaptive model on a fast link (nothing worth hiding a round trip for);
    /// always `true` from `always`/`experimental`/optimistic.
    pub show: bool,
    /// The model recommends marking the cells (its slow-link / glitch flag).
    pub flag: bool,
    /// The model's confirmed epoch; cells whose `tentative_until_epoch`
    /// exceeds it are the ones mosh would hold. `u64::MAX` = "hold nothing".
    pub confirmed_epoch: u64,
}

/// When the renderer paints what the model holds (`$POSH_PREDICTION_SHOW`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowPolicy {
    /// Paint every step the model holds, immediately (default). Under this
    /// policy `adaptive` and `always` look identical: both models record the
    /// same predictions; only their advice differs, and it is disregarded.
    Always,
    /// Honor the model's advice: paint a step only when `RenderAdvice::show`,
    /// hold tentative cells until `confirmed_epoch`, and mark cells only when
    /// the model flags them — mosh's original behavior, now a render choice.
    Advised,
}

impl ShowPolicy {
    /// Parses `$POSH_PREDICTION_SHOW` (default `always`).
    pub fn parse(value: Option<&str>) -> Result<ShowPolicy, String> {
        match value {
            None | Some("") | Some("always") => Ok(ShowPolicy::Always),
            Some("advised") => Ok(ShowPolicy::Advised),
            Some(other) => Err(format!("unknown prediction show policy ({other})")),
        }
    }

    /// The env selection, falling back to the default on a bad value (the
    /// client validates and errors at startup; this is the in-loop read).
    pub fn from_env() -> ShowPolicy {
        ShowPolicy::parse(std::env::var("POSH_PREDICTION_SHOW").ok().as_deref())
            .unwrap_or(ShowPolicy::Always)
    }

    pub fn name(self) -> &'static str {
        match self {
            ShowPolicy::Always => "always",
            ShowPolicy::Advised => "advised",
        }
    }
}

/// A look renderer wrapped in a [`ShowPolicy`]: delegates painting, decides
/// the show/hold/flag questions from the policy and the model's advice.
pub struct Policed<R: PredictionRenderer> {
    inner: R,
    show: ShowPolicy,
}

impl<R: PredictionRenderer> Policed<R> {
    pub fn new(inner: R, show: ShowPolicy) -> Policed<R> {
        Policed { inner, show }
    }
}

impl<R: PredictionRenderer> PredictionRenderer for Policed<R> {
    fn paint_cell(&self, fb: &mut Snapshot, row: u16, col: u16, replacement: &Cell, hint: CellHint) {
        self.inner.paint_cell(fb, row, col, replacement, hint);
    }
    fn paint_cursor(&self, fb: &mut Snapshot, row: u16, col: u16) {
        self.inner.paint_cursor(fb, row, col);
    }
    fn shows(&self, advice: &RenderAdvice) -> bool {
        match self.show {
            ShowPolicy::Always => true,
            ShowPolicy::Advised => advice.show,
        }
    }
    fn shows_tentative(&self, _advice: &RenderAdvice) -> bool {
        self.show == ShowPolicy::Always
    }
    fn flags(&self, advice: &RenderAdvice) -> bool {
        match self.show {
            ShowPolicy::Always => true,
            ShowPolicy::Advised => advice.flag,
        }
    }
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
/// adaptive/always/never/experimental variants drive [`MoshPredictor`],
/// optimistic drives [`OptimisticPredictor`] (FDR 0006), and the evolved
/// controller/from-scratch species drive the GP predictors (RFC 0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredictionModel {
    Always,
    Never,
    Adaptive,
    Experimental,
    Optimistic,
    /// RFC 0007 §4.1: evolved GP program outputs policy knobs that drive the
    /// existing echo machinery. The safe arm.
    Controller,
    /// RFC 0007 §4.2: evolved GP program emits predicted cells directly. The
    /// research arm.
    FromScratch,
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
            Some("controller") => Ok(PredictionModel::Controller),
            Some("scratch") => Ok(PredictionModel::FromScratch),
            Some(other) => Err(format!("unknown prediction model ({other})")),
        }
    }

    /// The model's user-facing spelling — the inverse of [`parse`](Self::parse),
    /// so banners, the palette heading, and the `Echo:` command labels all
    /// say the same word (`scratch`, not the Debug form `FromScratch`).
    pub fn name(self) -> &'static str {
        match self {
            PredictionModel::Adaptive => "adaptive",
            PredictionModel::Always => "always",
            PredictionModel::Never => "never",
            PredictionModel::Experimental => "experimental",
            PredictionModel::Optimistic => "optimistic",
            PredictionModel::Controller => "controller",
            PredictionModel::FromScratch => "scratch",
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
    pub fn name(self) -> &'static str {
        match self {
            RenderStyle::Replace => "replace",
            RenderStyle::Dim => "dim",
        }
    }

    /// Parses `$POSH_PREDICTION_RENDER` (default `replace`).
    pub fn parse(value: Option<&str>) -> Result<RenderStyle, String> {
        match value {
            None | Some("") | Some("replace") => Ok(RenderStyle::Replace),
            Some("dim") => Ok(RenderStyle::Dim),
            Some(other) => Err(format!("unknown prediction render style ({other})")),
        }
    }
}

// ---------------------------------------------------------------------------
// Slow-link echo escalation (FDR 0006's A/B, run live): with the model left
// on its default, a link whose SRTT holds past a threshold switches the
// session to `optimistic` — the one model that removes the tentative-epoch
// gap (the first keystrokes after Enter / any control key hidden for a full
// RTT; `always` keeps that gap, so on a slow link it shows nothing adaptive
// does not) — and switches back when the link recovers. Explicit choices
// (env model, palette `echo.set`) pin the model and bypass this. Pure and
// clock-free: the client feeds `srtt`/`now`, tests feed literals.

/// SRTT above which the link counts as slow (an RTT the user feels on every
/// keystroke that adaptive leaves hidden). Well clear of mosh's 30 ms SRTT
/// trigger — that one decides whether to SHOW predictions; this decides
/// whether to stop gating them.
pub const ESCALATE_SRTT_MS: f64 = 150.0;
/// SRTT below which the link counts as recovered — hysteresis so a jittery
/// link does not flap models (each switch repaints). Deliberately well
/// under the escalate threshold: once escalated, a link that settles IN the
/// band (say a 120 ms intercontinental baseline after a >150 ms burst) stays
/// optimistic — optimistic is no worse there, and recovering at the escalate
/// threshold would flap a link hovering around it. Only a genuinely fast
/// link, or an explicit `Echo:` choice, ends an escalation.
pub const DEESCALATE_SRTT_MS: f64 = 80.0;
/// How long the SRTT must hold on one side before acting: long enough that a
/// single retransmit spike does not escalate, short enough that a bad link
/// is fixed within a few keystrokes.
pub const ESCALATE_HOLD_MS: u64 = 3_000;
/// Recovery is slower than escalation: a link that looked good for a moment
/// mid-outage should not yank optimistic echo away.
pub const DEESCALATE_HOLD_MS: u64 = 15_000;

/// The `POSH_ECHO_ESCALATE` gate (default ON; `0`/`false`/`off`/`no` opt out
/// — the `POSH_MUX` off-switch shape): whether the default adaptive model
/// may auto-escalate to optimistic on a slow link.
pub fn escalation_selected() -> bool {
    crate::util::parse_default_on_gate(std::env::var("POSH_ECHO_ESCALATE").ok().as_deref())
}

/// The escalation state machine. `governing` is false once the user pinned a
/// model explicitly (or the gate is off); `escalated` is whether the switch
/// to optimistic is currently in effect (what the palette header reports as
/// `echo: optimistic (auto: slow link)`).
#[derive(Debug)]
pub struct EchoEscalation {
    governing: bool,
    escalated: bool,
    slow_since: Option<u64>,
    fast_since: Option<u64>,
}

impl EchoEscalation {
    /// `governing`: the default (adaptive) model is in effect and the
    /// `POSH_ECHO_ESCALATE` gate is on.
    pub fn new(governing: bool) -> EchoEscalation {
        EchoEscalation {
            governing,
            escalated: false,
            slow_since: None,
            fast_since: None,
        }
    }

    /// Whether the auto switch to optimistic is currently applied.
    pub fn escalated(&self) -> bool {
        self.escalated
    }

    /// Whether this machine decides the model at all.
    pub fn governing(&self) -> bool {
        self.governing
    }

    /// The user chose a model: `Adaptive` hands control back to the machine
    /// (fresh, un-escalated — it re-evaluates from the live SRTT); anything
    /// else pins that model and the machine steps aside.
    pub fn on_explicit(&mut self, model: PredictionModel, gate_on: bool) {
        *self = EchoEscalation::new(gate_on && model == PredictionModel::Adaptive);
    }

    /// Feed one SRTT reading. Returns the model to switch to when the hold
    /// on one side of the hysteresis band has elapsed: `Some(Optimistic)` to
    /// escalate, `Some(Adaptive)` to recover, `None` to leave things be.
    pub fn tick(&mut self, srtt_ms: f64, now: u64) -> Option<PredictionModel> {
        if !self.governing {
            return None;
        }
        if srtt_ms > ESCALATE_SRTT_MS {
            self.fast_since = None;
            if self.escalated {
                return None;
            }
            let since = *self.slow_since.get_or_insert(now);
            if now.saturating_sub(since) >= ESCALATE_HOLD_MS {
                self.escalated = true;
                self.slow_since = None;
                return Some(PredictionModel::Optimistic);
            }
        } else if srtt_ms < DEESCALATE_SRTT_MS {
            self.slow_since = None;
            if !self.escalated {
                return None;
            }
            let since = *self.fast_since.get_or_insert(now);
            if now.saturating_sub(since) >= DEESCALATE_HOLD_MS {
                self.escalated = false;
                self.fast_since = None;
                return Some(PredictionModel::Adaptive);
            }
        } else {
            // Inside the band: neither side's hold accumulates.
            self.slow_since = None;
            self.fast_since = None;
        }
        None
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
        PredictionModel::Controller => Box::new(ControllerPredictor::new(predict_overwrite)),
        PredictionModel::FromScratch => Box::new(FromScratchPredictor::new(predict_overwrite)),
        other => Box::new(MoshPredictor::new(other, predict_overwrite)),
    };
    let show = ShowPolicy::from_env();
    let renderer: Box<dyn PredictionRenderer> = match render {
        RenderStyle::Replace => Box::new(Policed::new(ReplaceRenderer, show)),
        RenderStyle::Dim => Box::new(Policed::new(DimRenderer, show)),
    };
    (predictor, renderer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_escalation_holds_hysteresis_and_yields_to_explicit_choices() {
        let mut esc = EchoEscalation::new(true);
        // Below the band: nothing, ever.
        assert_eq!(esc.tick(40.0, 0), None);
        assert_eq!(esc.tick(40.0, 100_000), None);
        assert!(!esc.escalated());

        // Slow, but not yet for the hold: no switch; one fast reading inside
        // the band does not reset the slow clock, a reading BELOW the band does.
        assert_eq!(esc.tick(400.0, 100_000), None);
        assert_eq!(esc.tick(400.0, 100_000 + ESCALATE_HOLD_MS - 1), None);
        assert_eq!(esc.tick(120.0, 100_000 + ESCALATE_HOLD_MS - 1), None, "in-band resets");
        assert_eq!(esc.tick(400.0, 110_000), None, "slow clock restarts");
        assert_eq!(
            esc.tick(400.0, 110_000 + ESCALATE_HOLD_MS),
            Some(PredictionModel::Optimistic),
            "held slow for the hold ⇒ escalate"
        );
        assert!(esc.escalated());
        // Still slow: no repeated switch.
        assert_eq!(esc.tick(400.0, 200_000), None);

        // A brief dip below the band does not de-escalate until the longer
        // recovery hold elapses — a mid-outage good moment must not yank it.
        assert_eq!(esc.tick(50.0, 200_000), None);
        assert_eq!(esc.tick(50.0, 200_000 + DEESCALATE_HOLD_MS - 1), None);
        assert_eq!(esc.tick(400.0, 200_000 + DEESCALATE_HOLD_MS), None, "slow again: recovery clock reset");
        assert!(esc.escalated());
        assert_eq!(esc.tick(50.0, 300_000), None);
        assert_eq!(
            esc.tick(50.0, 300_000 + DEESCALATE_HOLD_MS),
            Some(PredictionModel::Adaptive),
            "held fast for the recovery hold ⇒ back to adaptive"
        );
        assert!(!esc.escalated());

        // In-band stickiness (by design): a link that escalates on a burst
        // and then settles INSIDE the band never recovers on its own —
        // optimistic is no worse at 120 ms, and recovering at the escalate
        // threshold would flap a link hovering around it.
        let mut sticky = EchoEscalation::new(true);
        assert_eq!(sticky.tick(200.0, 0), None);
        assert_eq!(sticky.tick(200.0, ESCALATE_HOLD_MS), Some(PredictionModel::Optimistic));
        for t in 1..=100u64 {
            assert_eq!(sticky.tick(120.0, ESCALATE_HOLD_MS + t * DEESCALATE_HOLD_MS), None);
        }
        assert!(sticky.escalated(), "in-band forever ⇒ stays escalated");

        // An explicit non-adaptive choice pins: the machine steps aside even
        // on a terrible link; choosing adaptive again hands control back,
        // un-escalated, and the gate being off keeps it aside.
        esc.on_explicit(PredictionModel::Always, true);
        assert!(!esc.governing());
        assert_eq!(esc.tick(900.0, 400_000), None);
        assert_eq!(esc.tick(900.0, 400_000 + ESCALATE_HOLD_MS), None);
        esc.on_explicit(PredictionModel::Adaptive, true);
        assert!(esc.governing() && !esc.escalated());
        esc.on_explicit(PredictionModel::Adaptive, false);
        assert!(!esc.governing(), "gate off ⇒ never governs");
        assert_eq!(esc.tick(900.0, 500_000 + ESCALATE_HOLD_MS), None);
    }

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

    #[test]
    fn show_policy_parsing_defaults_to_always() {
        assert_eq!(ShowPolicy::parse(None), Ok(ShowPolicy::Always));
        assert_eq!(ShowPolicy::parse(Some("")), Ok(ShowPolicy::Always));
        assert_eq!(ShowPolicy::parse(Some("always")), Ok(ShowPolicy::Always));
        assert_eq!(ShowPolicy::parse(Some("advised")), Ok(ShowPolicy::Advised));
        assert!(ShowPolicy::parse(Some("sometimes")).is_err());
    }
}
