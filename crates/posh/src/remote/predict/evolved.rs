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

use mephisto::arena::NodeId;
use mephisto::domain::Domain;
use mephisto::genome::{AlphabetWeights, MutParams, MutationWeights};
use mephisto::rng::Rng;
use mephisto::tuple::TupleBreeder;

use super::metric::TERMINAL_COUNT;
use super::species::PolicyKnobs;

/// The schema tag for persisted controller populations (RFC 0007 §8); the
/// `_v{METRIC_SCHEMA_VERSION}` suffix guards the leaf-set the genome was evolved
/// against. On load, a mismatch means cold-start (the leaf indices would mean
/// something different).
pub const CONTROLLER_SCHEMA: &str = "mephisto-population-controller-schema_v2";

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
}
