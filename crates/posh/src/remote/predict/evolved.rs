//! RFC 0007 §4.1: the evolved controller's mephisto `Domain`.
//!
//! The genome is a 4-root GP tuple (one root per [`PolicyKnobs`] field) over the
//! shared 48-leaf metric vector, via mephisto's `TupleBreeder`. We delegate the
//! variation operators to the breeder and implement only `evaluate` + `rank`:
//! `evaluate` replays a genome's policy over a recent live-outcome window and
//! scores its show/flicker/suppress decisions; `rank` scalarizes (lower is
//! better) and returns the `+inf` lethal sentinel on a leak — a policy that
//! would echo while the remote `ECHO` flag is off (RFC 0007 §5.2).
//!
//! Combinational only (mild-willow's guidance): the alphabet zeroes `Delay` and
//! the mutation controller zeroes wrap-in-`Delay`, so every root is a pure
//! `f(metrics)`. The 48 leaves already carry recent state (predictor feedback,
//! screen state), so no per-genome temporal registers are needed for v1.

// Wired into the client tick loop in a follow-up (RFC 0007 §7); the Domain +
// fitness exist and are tested now. Allow until the loop consumes them.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use mephisto::arena::NodeId;
use mephisto::domain::{
    evaluate_population, initial_population, step, Bred, Domain, LoopConfig, Recombinator, XoverOp,
};
use mephisto::gene_pool::content_id;
use mephisto::genome::{AlphabetWeights, ConstPolicy, MutParams, MutationWeights};
use mephisto::persist::{load_population, save_population};
use mephisto::rng::Rng;
use mephisto::tuple::TupleBreeder;

use crate::remote::display::Snapshot;

use super::metric::{MetricVector, TERMINAL_COUNT};
use super::species::PolicyKnobs;
use super::{
    EvolutionStats, MoshPredictor, OptimisticPredictor, PredictionModel, PredictionRenderer,
    Predictor, PredictorStats,
};

/// The schema tag for persisted controller populations (RFC 0007 §8); the
/// `_v{METRIC_SCHEMA_VERSION}` suffix guards the leaf-set the genome was evolved
/// against. On load, a mismatch means cold-start (the leaf indices would mean
/// something different).
pub const CONTROLLER_SCHEMA: &str = "mephisto-population-controller-schema_v2";

/// The schema tag for the periodically-persisted single-champion hyphence docs
/// (RFC 0007 §8): the same leaf-set version guard as the population blob, but a
/// one-genome document under `$XDG_DATA_HOME` — the durable, content-addressed
/// record of every distinct champion this host's evolution has produced.
pub const CHAMPION_SCHEMA: &str = "mephisto-champion-controller-schema_v2";

/// One recorded keystroke outcome the controller's fitness scores against: the
/// metric vector at that tick, whether the optimistic echo would have been
/// correct (matched the server's authoritative paint), and whether echo was
/// safe (`ECHO` on + primary screen).
#[derive(Clone, Copy, Debug)]
pub struct OutcomeSample {
    pub metrics: [f64; TERMINAL_COUNT],
    pub echoed_ok: bool,
    pub echo_safe: bool,
}

/// Raw fitness components (kept un-scalarized; [`ControllerDomain::rank`]
/// collapses them). A leaking policy is flagged lethal.
#[derive(Clone, Copy, Debug, Default)]
pub struct ControllerFitness {
    /// Predictions correctly shown (latency hidden) — reward.
    pub hits: u32,
    /// Predictions shown that were wrong (flicker) — penalty.
    pub flicker: u32,
    /// Correct predictions suppressed (missed latency-hide) — penalty.
    pub missed: u32,
    /// Window samples scored.
    pub n: u32,
    /// The policy would echo under `ECHO`-off on at least one sample (a leak).
    pub leaked: bool,
}

/// Tuple arity: one root per [`PolicyKnobs`] field.
const ARITY: usize = 4;

pub struct ControllerDomain {
    breeder: TupleBreeder,
    /// The recent outcome window the next `evaluate` pass scores against. The
    /// client sets this before each `evaluate_population` pass (RFC 0007 §7).
    pub window: Vec<OutcomeSample>,
}

impl ControllerDomain {
    /// Construct the controller domain over the 48-terminal leaf set. The
    /// alphabet is combinational (`Delay` zeroed; `cond`/`not`/`xor` enabled) so
    /// each root is a pure function of the current metrics.
    pub fn new() -> ControllerDomain {
        let alphabet = AlphabetWeights {
            delay: 0.0,
            cond: 1.0,
            not: 1.0,
            xor: 1.0,
            ..AlphabetWeights::default()
        };
        let weights = MutationWeights {
            // No wrap-in-Delay: keep mutated roots combinational too.
            wrap_delay: 0.0,
            ..MutationWeights::default()
        };
        let mut_params = MutParams {
            rate: 0.1,
            jitter: 0.5,
            n_inputs: TERMINAL_COUNT as u32,
            macro_depth: 3,
            weights,
            alphabet,
            // Metric terminals span ms/ratios/ids; the plain Gaussian nudge +
            // jitter cover them, so no curated constant library is needed.
            const_policy: ConstPolicy::default(),
        };
        ControllerDomain {
            // init_depth 3, per-root bloat cap 64 — starting points; tune later.
            breeder: TupleBreeder::new(ARITY, 3, 64, mut_params),
            window: Vec::new(),
        }
    }

    /// The [`PolicyKnobs`] a genome produces for one metric sample (RFC 0007
    /// §4.1): the 4 roots evaluated combinationally, coerced per field.
    pub fn knobs(&self, g: &[NodeId], metrics: &[f64; TERMINAL_COUNT]) -> PolicyKnobs {
        let raw = self.breeder.eval_outputs(g, metrics);
        PolicyKnobs::from_roots([raw[0], raw[1], raw[2], raw[3]])
    }
}

impl Default for ControllerDomain {
    fn default() -> ControllerDomain {
        ControllerDomain::new()
    }
}

impl Domain for ControllerDomain {
    type Genome = Vec<NodeId>;
    type Fitness = ControllerFitness;

    fn random(&mut self, rng: &mut Rng) -> Vec<NodeId> {
        self.breeder.random(rng)
    }

    fn crossover(&mut self, rng: &mut Rng, parents: &[Vec<NodeId>]) -> Vec<NodeId> {
        let a = &parents[0];
        let b = parents.get(1).unwrap_or(a);
        self.breeder.crossover(rng, a, b)
    }

    fn mutate(
        &mut self,
        rng: &mut Rng,
        g: &Vec<NodeId>,
        _parent: &ControllerFitness,
    ) -> Vec<NodeId> {
        self.breeder.mutate(rng, g)
    }

    /// Opt in to the engine's self-adaptive recombinator (mephisto #21): each
    /// individual carries its own crossover strategy (operator + mix + mate
    /// preference), inherited and mutated like a gene, so the reproductive
    /// strategy itself evolves alongside the policies.
    fn uses_recombinator(&self) -> bool {
        true
    }

    /// Dispatch the recombinator's chosen operator over the 4-root tuple:
    /// OnePoint/TwoPoint exchange whole policy roots as segments, Uniform mixes
    /// subtrees within each root (the classic component-wise operator).
    fn recombine(
        &mut self,
        rng: &mut Rng,
        op: XoverOp,
        mix: f64,
        a: &Vec<NodeId>,
        b: &Vec<NodeId>,
    ) -> Vec<NodeId> {
        self.breeder.recombine(rng, op, mix, a, b)
    }

    /// Assortative mate-choice signal (mephisto #21 Stage 2): mean per-root
    /// Jaccard overlap of reachable node sets, 1.0 for identical tuples.
    fn similarity(&self, a: &Vec<NodeId>, b: &Vec<NodeId>) -> f64 {
        self.breeder.similarity(a, b)
    }

    /// Arena GC (mephisto #33): re-express the live tuples in a fresh arena so a
    /// long-lived session's append-only arena stays bounded. Trajectory-neutral
    /// (a faithful structural copy, no rng); on the never-expected reintern
    /// failure the arena is left untouched and the live set returned unchanged.
    fn gc(&mut self, live: &[Vec<NodeId>]) -> Vec<Vec<NodeId>> {
        self.breeder.gc(live).unwrap_or_else(|| live.to_vec())
    }

    fn serialize(&self, g: &Vec<NodeId>) -> Vec<f64> {
        self.breeder.serialize(g)
    }

    fn deserialize(&mut self, dna: &[f64]) -> Option<Vec<NodeId>> {
        self.breeder.deserialize(dna)
    }

    fn evaluate(&mut self, g: &Vec<NodeId>) -> ControllerFitness {
        let mut f = ControllerFitness::default();
        for s in &self.window {
            let k = self.knobs(g, &s.metrics);
            // RFC 0007 §5.2: showing a prediction while echo is unsafe is a leak
            // (disqualifying). Recorded as lethal; rank() maps it to +inf.
            if k.show && !s.echo_safe {
                f.leaked = true;
            }
            f.n += 1;
            if k.show && s.echo_safe {
                if s.echoed_ok {
                    f.hits += 1;
                } else {
                    f.flicker += 1;
                }
            } else if s.echo_safe && s.echoed_ok {
                // A correct prediction the policy suppressed: missed latency-hide.
                f.missed += 1;
            }
        }
        f
    }

    fn rank(&self, f: &ControllerFitness) -> f64 {
        if f.leaked {
            return f64::INFINITY; // lethal: never selected over a viable genome
        }
        if f.n == 0 {
            return 0.0; // no evidence yet — neutral
        }
        // Lower is better: reward hits, penalize flicker (weighted heavier than a
        // miss, since visible wrong echo is worse than a hidden-but-correct one).
        2.0 * f64::from(f.flicker) + f64::from(f.missed) - f64::from(f.hits)
    }
}

/// Frames between generations: a single frame's outcome is noisy, so accumulate
/// a window and `step` every N (RFC 0007 §7, mild-willow's cadence guidance).
const STEP_EVERY_N_FRAMES: u64 = 32;
/// Cap on the recent outcome window scored each `evaluate` pass.
const WINDOW_CAP: usize = 256;
/// Window samples before the champion is mature enough to be eligible for
/// display (RFC 0007 §7.1): below this the adaptive shadow always shows.
const MATURITY_FRAMES: usize = 32;
/// Sustained frames of champion-good (or -bad) before the §7.1 display flips —
/// the hysteresis that keeps the displayed predictor from flapping.
const HYSTERESIS_FRAMES: i32 = 16;
/// Generations between champion hyphence-doc saves (RFC 0007 §8): every N
/// `step`s the current champion is serialized (content-addressed, deduped) to
/// `$XDG_DATA_HOME`, so a crash never costs more than N generations of the
/// champion record. With `STEP_EVERY_N_FRAMES` = 32 this is every ~256 frames.
const CHAMPION_SAVE_EVERY_N_GENERATIONS: u64 = 8;

/// Starting LoopConfig for a small live population (mild-willow's suggested
/// values; tune later). `generations` is unused by `step`; `gc_interval` is
/// consumed by posh's own tick loop (the online lane drives GC itself, the
/// same cadence contract `evolve` applies in the batch lane).
fn loop_config() -> LoopConfig {
    LoopConfig {
        population: 32,
        generations: 0,
        survivor_fraction: 0.5,
        elitism: 2, // keep the live champion
        tournament: 3,
        crossover_rate: 0.7,
        immigrant_fraction: 0.1,
        local_opt_rate: 0.0,
        opaque_recombine_rate: 0.0,
        // A session ticks generations for hours; compact the arena every 64
        // generations (mephisto #33; trajectory-neutral) so it stays bounded.
        gc_interval: 64,
    }
}

/// Where the controller population persists across sessions (RFC 0007 §8):
/// `$XDG_STATE_HOME/posh/controller-population.hyph`, falling back to
/// `~/.local/state/...`. `None` when neither env var is set.
fn population_path() -> Option<PathBuf> {
    // Tests construct ControllerPredictors freely; never let their load/Drop
    // touch the user's real state dir. The persist logic is covered directly.
    if cfg!(test) {
        return None;
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("posh").join("controller-population.hyph"))
}

/// Where champion hyphence docs accumulate (RFC 0007 §8): champions are user
/// data — the durable artifacts of the evolution, inspectable and shareable —
/// so they live under `$XDG_DATA_HOME/posh/predict/champions/`, falling back to
/// `~/.local/share/...` (the population's working state stays in
/// `$XDG_STATE_HOME`). `None` when neither env var is set.
fn champion_dir() -> Option<PathBuf> {
    // Same test guard as population_path(): never touch the user's real dirs.
    if cfg!(test) {
        return None;
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("posh").join("predict").join("champions"))
}

/// Write one champion hyphence doc into `dir`, content-addressed by the doc's
/// mephisto `content_id` (`<id>.hyph`), so re-saving an unchanged champion —
/// or the same champion re-evolved in a later session — dedups to one file.
/// Temp-then-rename so a crash mid-write can't leave a truncated doc. Returns
/// the doc path, or `None` if the write failed (best-effort, like the
/// population save).
fn write_champion_doc(dir: &Path, blob: &[u8]) -> Option<PathBuf> {
    let path = dir.join(format!("{}.hyph", content_id(blob)));
    if path.is_file() {
        return Some(path); // content-addressed: already recorded
    }
    std::fs::create_dir_all(dir).ok()?;
    let tmp = path.with_extension("hyph.tmp");
    std::fs::write(&tmp, blob).ok()?;
    std::fs::rename(&tmp, &path).ok()?;
    Some(path)
}

/// RFC 0007 §4.1 live controller: an evolved GP population whose champion's
/// [`PolicyKnobs`] gate a swappable base echo — optimistic by default, adaptive
/// via `POSH_PREDICTION_CONTROLLER_ECHO=adaptive` for A/B. Each server frame it
/// accumulates an outcome sample, re-evaluates the population (cheap), and steps
/// a generation every [`STEP_EVERY_N_FRAMES`].
///
/// The §5.1 runtime leak gate is preserved by the base predictor (its
/// `set_echo_safe(false)` drops the overlay), so a `show`-happy champion cannot
/// leak under `ECHO`-off; the fitness additionally penalizes it as lethal (§5.2).
pub struct ControllerPredictor {
    domain: ControllerDomain,
    /// The engine-held population: each individual is a genome plus its
    /// self-adaptive recombinator strategy (mephisto #21).
    population: Vec<Bred<ControllerDomain>>,
    cfg: LoopConfig,
    rng: Rng,
    /// The swappable echo machinery the champion's knobs drive.
    base: Box<dyn Predictor>,
    champion: Vec<NodeId>,
    /// The champion's rank at the most recent `evaluate_population` pass
    /// (lower is better; `+inf` until the first scored window).
    champion_rank: f64,
    metrics: [f64; TERMINAL_COUNT],
    /// The champion's `show` decision for the most recent keystroke.
    show: bool,
    echo_safe: bool,
    frames: u64,
    /// Generations stepped this session (each `step` call is one generation).
    generations: u64,
    /// Champion hyphence docs written (deduped) this session + the last path.
    champion_saves: u64,
    last_champion_doc: Option<PathBuf>,
    /// Base outcome counters at the previous frame, to delta the fitness signal.
    last_outcomes: (u64, u64, u64),
    /// RFC 0007 §7.1 adaptive shadow baseline, run in parallel and scored by the
    /// same fitness; the display falls back to it whenever the GP champion is
    /// immature or not net-beneficial, so the user never sees worse than
    /// `adaptive`.
    shadow: MoshPredictor,
    /// Whether the GP champion (vs the shadow) is currently displayed.
    display_champion: bool,
    /// Hysteresis counter for the champion-vs-shadow handover, clamped to
    /// ±[`HYSTERESIS_FRAMES`]; the display flips only at the extremes.
    champion_streak: i32,
}

impl ControllerPredictor {
    pub fn new(predict_overwrite: bool) -> ControllerPredictor {
        let adapt = std::env::var("POSH_PREDICTION_CONTROLLER_ECHO")
            .map(|v| v == "adaptive")
            .unwrap_or(false);
        let base: Box<dyn Predictor> = if adapt {
            Box::new(MoshPredictor::new(PredictionModel::Adaptive, predict_overwrite))
        } else {
            Box::new(OptimisticPredictor::new(predict_overwrite))
        };
        let mut domain = ControllerDomain::new();
        let cfg = loop_config();
        // Fixed seed: reproducible evolution (the A/B the FDR requires).
        let mut rng = Rng::new(0x05f7_0007);
        // Seed from the persisted population if its schema still matches the
        // current leaf set (RFC 0007 §8); otherwise cold-start. A mismatch means
        // the metric vector changed, so old genomes' leaf indices are stale.
        let loaded = population_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| {
                let l = load_population(&mut domain, &bytes).ok()?;
                (l.schema == CONTROLLER_SCHEMA && !l.genomes.is_empty()).then_some(l.genomes)
            });
        // The persisted blob carries genomes only; loaded individuals draw a
        // fresh random recombinator strategy (the engine re-adapts it quickly,
        // and randomizing preserves strategy diversity across sessions).
        let mut population: Vec<Bred<ControllerDomain>> = match loaded {
            Some(p) => p
                .into_iter()
                .map(|genome| Bred {
                    genome,
                    recomb: Recombinator::random(&mut rng),
                })
                .collect(),
            None => initial_population(&mut domain, &cfg, &mut rng),
        };
        // Normalize to the configured population size (a persisted blob may differ).
        while population.len() < cfg.population {
            population.push(Bred {
                genome: domain.random(&mut rng),
                recomb: Recombinator::random(&mut rng),
            });
        }
        population.truncate(cfg.population.max(1));
        let champion = population[0].genome.clone();
        ControllerPredictor {
            domain,
            population,
            cfg,
            rng,
            base,
            champion,
            champion_rank: f64::INFINITY,
            metrics: [f64::NAN; TERMINAL_COUNT],
            show: true,
            echo_safe: false,
            frames: 0,
            generations: 0,
            champion_saves: 0,
            last_champion_doc: None,
            last_outcomes: (0, 0, 0),
            // The §7.1 shadow is always the adaptive model (the floor we must beat).
            shadow: MoshPredictor::new(PredictionModel::Adaptive, predict_overwrite),
            display_champion: false,
            champion_streak: 0,
        }
    }

    /// Push one outcome sample for the just-finished frame and, every N frames,
    /// re-evaluate the population and step a generation (RFC 0007 §7).
    fn tick(&mut self) {
        // Frame-granular fitness proxy (v1): the delta in the base predictor's
        // correct/incorrect counters since last frame says whether recent echo
        // was good. TODO: per-keystroke ground truth tied to each sample.
        let (c, n, i) = self.base.stats().outcomes;
        let dc = c.saturating_sub(self.last_outcomes.0);
        let di = i.saturating_sub(self.last_outcomes.2);
        self.last_outcomes = (c, n, i);
        if dc + di > 0 {
            self.domain.window.push(OutcomeSample {
                metrics: self.metrics,
                echoed_ok: dc >= di,
                echo_safe: self.echo_safe,
            });
            if self.domain.window.len() > WINDOW_CAP {
                let excess = self.domain.window.len() - WINDOW_CAP;
                self.domain.window.drain(0..excess);
            }
        }
        self.frames = self.frames.wrapping_add(1);
        if !self.domain.window.is_empty() {
            let scored = evaluate_population(&mut self.domain, &self.population);
            self.champion = scored[0].genome.clone();
            self.champion_rank = scored[0].rank;
            // §7.1 best-of vs the adaptive shadow: the champion is eligible to
            // display only once its window is mature AND its fitness is
            // net-beneficial (rank < 0 means hits outweigh flicker + miss).
            // Hysteresis keeps the handover from flapping. (v1: a net-benefit
            // gate; a direct shadow-rank comparison is a follow-up.)
            let champion_good =
                self.domain.window.len() >= MATURITY_FRAMES && scored[0].rank < 0.0;
            self.champion_streak = (self.champion_streak + if champion_good { 1 } else { -1 })
                .clamp(-HYSTERESIS_FRAMES, HYSTERESIS_FRAMES);
            if self.champion_streak >= HYSTERESIS_FRAMES {
                self.display_champion = true;
            } else if self.champion_streak <= -HYSTERESIS_FRAMES {
                self.display_champion = false;
            }
            if self.frames.is_multiple_of(STEP_EVERY_N_FRAMES) {
                self.population = step(&mut self.domain, &self.cfg, &mut self.rng, &scored);
                self.generations += 1;
                // Arena GC (mephisto #33): a session ticks generations for
                // hours; compact the append-only arena to the live set on the
                // same cadence contract `evolve` applies in the batch lane.
                if self.cfg.gc_interval > 0
                    && self.generations.is_multiple_of(self.cfg.gc_interval as u64)
                {
                    self.gc_arena();
                }
                // Periodic champion record (RFC 0007 §8): a crash never costs
                // more than N generations of the champion lineage.
                if self
                    .generations
                    .is_multiple_of(CHAMPION_SAVE_EVERY_N_GENERATIONS)
                {
                    self.save_champion();
                }
            }
        }
    }

    /// Compact the domain's arena to the live set (population + champion),
    /// adopting the re-expressed genomes. Mirrors `evolve`'s defensive posture:
    /// the compaction is adopted only if it round-tripped the whole set 1:1.
    fn gc_arena(&mut self) {
        let mut live: Vec<Vec<NodeId>> =
            self.population.iter().map(|b| b.genome.clone()).collect();
        live.push(self.champion.clone());
        let compacted = self.domain.gc(&live);
        if compacted.len() == live.len() {
            for (slot, g) in self.population.iter_mut().zip(&compacted) {
                slot.genome = g.clone();
            }
            self.champion = compacted[self.population.len()].clone();
        }
    }

    /// Serialize the current champion as a content-addressed hyphence doc under
    /// `$XDG_DATA_HOME/posh/predict/champions/` (RFC 0007 §8). Best-effort:
    /// a failed write costs only this save, never the session.
    fn save_champion(&mut self) {
        let Some(dir) = champion_dir() else {
            return;
        };
        let blob = save_population(
            &self.domain,
            CHAMPION_SCHEMA,
            std::slice::from_ref(&self.champion),
        );
        if let Some(path) = write_champion_doc(&dir, &blob) {
            if self.last_champion_doc.as_ref() != Some(&path) {
                self.champion_saves += 1;
            }
            self.last_champion_doc = Some(path);
        }
    }
}

impl Default for ControllerPredictor {
    fn default() -> ControllerPredictor {
        ControllerPredictor::new(false)
    }
}

impl Drop for ControllerPredictor {
    /// Best-effort persist on graceful exit / model switch (RFC 0007 §8): the
    /// next session seeds from it (schema-guarded), and the final champion is
    /// recorded as a hyphence doc. Errors are swallowed — a failed save just
    /// costs the accumulated evolution, never the session.
    fn drop(&mut self) {
        self.save_champion();
        let Some(path) = population_path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let genomes: Vec<Vec<NodeId>> =
            self.population.iter().map(|b| b.genome.clone()).collect();
        let blob = save_population(&self.domain, CONTROLLER_SCHEMA, &genomes);
        // Write a temp then rename so a crash mid-write can't truncate the blob.
        let tmp = path.with_extension("hyph.tmp");
        if std::fs::write(&tmp, &blob).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

impl Predictor for ControllerPredictor {
    fn set_metrics(&mut self, metrics: &MetricVector) {
        self.metrics = metrics.to_terminals();
    }

    fn set_frame_sent(&mut self, offset: u64) {
        self.base.set_frame_sent(offset);
        self.shadow.set_frame_sent(offset);
    }

    fn on_user_byte(&mut self, byte: u8, fb: &Snapshot, now: u64) {
        // The champion's policy for this keystroke's metric vector. Both the GP
        // base and the §7.1 shadow are fed every keystroke so either is ready to
        // display.
        let knobs = self.domain.knobs(&self.champion, &self.metrics);
        self.show = knobs.show;
        self.base.on_user_byte(byte, fb, now);
        self.shadow.on_user_byte(byte, fb, now);
    }

    fn on_server_frame(&mut self, input_ack: u64, echo_ack: u64, send_interval: u64) {
        self.base.on_server_frame(input_ack, echo_ack, send_interval);
        self.shadow.on_server_frame(input_ack, echo_ack, send_interval);
        self.tick();
    }

    fn set_echo_safe(&mut self, safe: bool) {
        self.echo_safe = safe;
        self.base.set_echo_safe(safe);
        self.shadow.set_echo_safe(safe);
    }

    fn cull(&mut self, fb: &Snapshot, now: u64) {
        self.base.cull(fb, now);
        self.shadow.cull(fb, now);
    }

    fn render(&self, fb: &mut Snapshot, renderer: &dyn PredictionRenderer) {
        // §7.1 best-of: display the GP champion (gated by its `show` knob, §4.1)
        // when it has earned it, else the adaptive shadow floor. The runtime leak
        // gate holds for both (set_echo_safe drops their overlays under ECHO-off).
        if self.display_champion {
            if self.show {
                self.base.render(fb, renderer);
            }
        } else {
            self.shadow.render(fb, renderer);
        }
    }

    fn reset(&mut self) {
        self.base.reset();
        self.shadow.reset();
    }

    fn active(&self) -> bool {
        self.base.active() || self.shadow.active()
    }

    fn needs_timer(&self) -> bool {
        self.base.needs_timer() || self.shadow.needs_timer()
    }

    fn stats(&self) -> PredictorStats {
        // Report the currently-displayed predictor's gauges.
        if self.display_champion {
            self.base.stats()
        } else {
            self.shadow.stats()
        }
    }

    fn evolution(&self) -> Option<EvolutionStats> {
        // Champion size: total nodes across the 4 policy roots (a parsimony /
        // bloat gauge alongside the fitness rank).
        let champion_size = self
            .champion
            .iter()
            .map(|&root| self.domain.breeder.arena().measure(root).0)
            .sum();
        Some(EvolutionStats {
            generations: self.generations,
            population: self.population.len(),
            window: self.domain.window.len(),
            champion_rank: self.champion_rank,
            champion_size,
            champion_displayed: self.display_champion,
            champion_streak: self.champion_streak,
            champion_saves: self.champion_saves,
            last_champion_doc: self.last_champion_doc.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(echo_safe: bool, echoed_ok: bool) -> OutcomeSample {
        OutcomeSample {
            metrics: [0.5; TERMINAL_COUNT],
            echoed_ok,
            echo_safe,
        }
    }

    #[test]
    fn domain_constructs_and_evaluates_over_a_window() {
        let mut dom = ControllerDomain::new();
        let mut rng = Rng::new(1);
        let g = dom.random(&mut rng);
        dom.window = vec![sample(true, true), sample(true, false)];
        let f = dom.evaluate(&g);
        assert_eq!(f.n, 2);
        // echo_safe samples can never leak.
        assert!(!f.leaked);
        assert!(dom.rank(&f).is_finite());
    }

    #[test]
    fn rank_treats_a_leak_as_lethal() {
        let dom = ControllerDomain::new();
        let leaked = ControllerFitness {
            leaked: true,
            ..ControllerFitness::default()
        };
        assert_eq!(dom.rank(&leaked), f64::INFINITY);
    }

    #[test]
    fn serialize_round_trips_through_the_breeder() {
        let mut dom = ControllerDomain::new();
        let mut rng = Rng::new(7);
        let g = dom.random(&mut rng);
        let dna = dom.serialize(&g);
        assert_eq!(dom.deserialize(&dna), Some(g));
    }

    #[test]
    fn controller_starts_on_the_adaptive_shadow_floor() {
        // RFC 0007 §7.1: the GP champion never displays until it earns it; an
        // immature window keeps the adaptive shadow on, and driving a keystroke
        // + frame must not panic across both base and shadow.
        let mut c = ControllerPredictor::new(false);
        assert!(!c.display_champion);
        let fb = Snapshot::blank(24, 80);
        c.set_echo_safe(true);
        c.set_frame_sent(0);
        c.on_user_byte(b'x', &fb, 1000);
        c.on_server_frame(1, 0, 50);
        assert!(!c.display_champion, "immature champion stays on the shadow floor");
    }

    #[test]
    fn population_persists_and_the_schema_guard_detects_a_mismatch() {
        let mut dom = ControllerDomain::new();
        let cfg = loop_config();
        let mut rng = Rng::new(3);
        let pop: Vec<Vec<NodeId>> = initial_population(&mut dom, &cfg, &mut rng)
            .into_iter()
            .map(|b| b.genome)
            .collect();

        let blob = save_population(&dom, CONTROLLER_SCHEMA, &pop);
        let loaded = load_population(&mut dom, &blob).expect("loads a well-formed blob");
        assert_eq!(loaded.schema, CONTROLLER_SCHEMA);
        assert_eq!(loaded.genomes.len(), pop.len());

        // RFC 0007 §8: a blob saved under a different schema is recognized as
        // such (the caller cold-starts instead of feeding stale leaf wiring).
        let stale = save_population(&dom, "mephisto-population-controller-schema_v999", &pop);
        let l2 = load_population(&mut dom, &stale).expect("loads");
        assert_ne!(l2.schema, CONTROLLER_SCHEMA);
    }

    #[test]
    fn gc_preserves_the_champion_policy_and_bounds_the_arena() {
        // The online-lane GC must be trajectory-neutral: the champion's policy
        // outputs are identical before and after compaction (mephisto #33).
        let mut dom = ControllerDomain::new();
        let mut rng = Rng::new(11);
        // Grow the arena with discarded material, keeping one live genome.
        let mut live = dom.random(&mut rng);
        for _ in 0..20 {
            live = dom.breeder.mutate(&mut rng, &live);
        }
        let metrics = [0.25; TERMINAL_COUNT];
        let before = dom.knobs(&live, &metrics);
        let arena_before = dom.breeder.arena().len();

        let compacted = dom.gc(std::slice::from_ref(&live));
        assert_eq!(compacted.len(), 1);
        assert!(dom.breeder.arena().len() <= arena_before);
        assert_eq!(dom.knobs(&compacted[0], &metrics), before);
    }

    #[test]
    fn champion_doc_round_trips_and_dedups_by_content() {
        // RFC 0007 §8: a champion is serialized as a schema-tagged hyphence doc,
        // content-addressed so re-saving the same champion is one file.
        let mut dom = ControllerDomain::new();
        let mut rng = Rng::new(19);
        let champion = dom.random(&mut rng);
        let blob = save_population(&dom, CHAMPION_SCHEMA, std::slice::from_ref(&champion));

        let dir = std::env::temp_dir().join(format!(
            "posh-champion-doc-test-{}-{}",
            std::process::id(),
            content_id(&blob)
        ));
        let p1 = write_champion_doc(&dir, &blob).expect("first write");
        let p2 = write_champion_doc(&dir, &blob).expect("dedup write");
        assert_eq!(p1, p2, "same content must address the same doc");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "re-saving an unchanged champion must not add files"
        );

        let loaded =
            load_population(&mut dom, &std::fs::read(&p1).unwrap()).expect("doc loads back");
        assert_eq!(loaded.schema, CHAMPION_SCHEMA);
        assert_eq!(loaded.genomes, vec![champion]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn controller_reports_evolution_stats() {
        // The palette's prediction-stats view reads these gauges; a fresh
        // controller reports a full population, zero generations, and no
        // champion doc yet.
        let c = ControllerPredictor::new(false);
        let ev = c.evolution().expect("the GP controller exposes evolution");
        assert_eq!(ev.generations, 0);
        assert_eq!(ev.population, loop_config().population);
        assert_eq!(ev.window, 0);
        assert!(!ev.champion_displayed);
        assert!(ev.champion_size > 0, "the champion has at least its roots");
        assert_eq!(ev.champion_saves, 0);
        assert!(ev.last_champion_doc.is_none());
    }
}
