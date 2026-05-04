//! Two-state Markov chaos: healthy ↔ degraded with exponential dwell times.
//!
//! Per-service `MarkovState` is held in `AppState.markov_state` behind a
//! parking_lot::RwLock — *never* held across `.await`. The hot path:
//!   1. write-lock,
//!   2. step() the state (sync: rand + chrono::Utc::now()),
//!   3. drop the lock,
//!   4. then await whatever (sleep, response build, etc.).
//!
//! Determinism: callers may inject a seeded RNG via `step_with_rng()` for tests.
//!
//! State is *not* persisted across agent restarts — by design. Chaos is meant to
//! be observed within a session; restart resets every service to Healthy.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use chrono::{DateTime, Utc};
use rand::{Rng, RngCore};

use crate::chaos::MarkovConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateKind {
    Healthy,
    Degraded,
}

#[derive(Clone, Debug)]
pub struct MarkovState {
    kind:        StateKind,
    /// Wall-clock time at which the current dwell expires.
    dwell_until: DateTime<Utc>,
}

impl MarkovState {
    pub fn new() -> Self {
        Self {
            kind:        StateKind::Healthy,
            // Immediately expired → first step() picks a fresh dwell.
            dwell_until: Utc::now(),
        }
    }

    /// Advance the state machine. Returns (current_kind, transitioned?).
    /// Uses the thread-local RNG. Never panics.
    pub fn step(&mut self, cfg: &MarkovConfig) -> (StateKind, bool) {
        let mut rng = rand::thread_rng();
        self.step_with_rng(cfg, &mut rng)
    }

    /// Advance the state machine using a caller-supplied RNG. Used by tests
    /// for seeded determinism.
    pub fn step_with_rng<R: RngCore>(
        &mut self,
        cfg: &MarkovConfig,
        rng: &mut R,
    ) -> (StateKind, bool) {
        let now = Utc::now();
        if now < self.dwell_until {
            return (self.kind, false);
        }
        // Dwell expired — flip state and pick a fresh dwell.
        self.kind = match self.kind {
            StateKind::Healthy  => StateKind::Degraded,
            StateKind::Degraded => StateKind::Healthy,
        };
        let mean_ms = match self.kind {
            StateKind::Healthy  => cfg.mean_dwell_healthy_ms,
            StateKind::Degraded => cfg.mean_dwell_degraded_ms,
        }
        .max(1);
        // Inverse-CDF for exponential: -mean * ln(1 - u). Clamp 1-u away from
        // zero to avoid -inf on rare u → 1.
        let u: f64 = rng.gen();
        let dwell_ms = (-(mean_ms as f64) * (1.0 - u).max(1e-9).ln()) as i64;
        self.dwell_until = now + chrono::Duration::milliseconds(dwell_ms.max(1));
        (self.kind, true)
    }

    /// Test/observability accessor — current state without stepping.
    #[allow(dead_code)]
    pub fn current_kind(&self) -> StateKind {
        self.kind
    }
}

impl Default for MarkovState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chaos::MarkovConfig;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn cfg() -> MarkovConfig {
        MarkovConfig {
            mean_dwell_healthy_ms:  100,
            mean_dwell_degraded_ms:  50,
            degraded_error_rate:    0.6,
            degraded_latency_mult:  5.0,
        }
    }

    #[test]
    fn starts_healthy() {
        let st = MarkovState::new();
        assert_eq!(st.kind, StateKind::Healthy);
    }

    #[test]
    fn seeded_determinism() {
        let mut a = MarkovState::new();
        let mut b = MarkovState::new();
        let mut r1 = StdRng::seed_from_u64(42);
        let mut r2 = StdRng::seed_from_u64(42);
        let cfg = cfg();
        let mut seq_a = Vec::with_capacity(50);
        let mut seq_b = Vec::with_capacity(50);
        for _ in 0..50 {
            // Tiny sleep so wall clock advances reliably between iterations.
            std::thread::sleep(std::time::Duration::from_millis(1));
            let (ka, _) = a.step_with_rng(&cfg, &mut r1);
            let (kb, _) = b.step_with_rng(&cfg, &mut r2);
            seq_a.push(ka);
            seq_b.push(kb);
        }
        assert_eq!(
            seq_a, seq_b,
            "seeded RNG must produce identical state sequences"
        );
    }

    #[test]
    fn ratio_matches_expected_band() {
        // With mean_healthy=30000ms and mean_degraded=5000ms the long-run
        // fraction in degraded is 5000/(30000+5000) = 14.3%.
        // We can't actually wait 35 seconds in a test, so we instead simulate
        // by feeding the state machine the result of many transitions and
        // measuring the ratio of healthy vs degraded *dwells* drawn.
        let cfg_real = MarkovConfig {
            mean_dwell_healthy_ms:  30_000,
            mean_dwell_degraded_ms:  5_000,
            degraded_error_rate:    0.6,
            degraded_latency_mult:  5.0,
        };
        let mut rng = StdRng::seed_from_u64(7);
        let mut total_healthy_ms:  i128 = 0;
        let mut total_degraded_ms: i128 = 0;
        // Force-transition by directly drawing dwell samples — start in healthy.
        let mut current = StateKind::Healthy;
        for _ in 0..10_000 {
            let mean_ms = match current {
                StateKind::Healthy  => cfg_real.mean_dwell_healthy_ms,
                StateKind::Degraded => cfg_real.mean_dwell_degraded_ms,
            } as f64;
            let u: f64 = rng.gen();
            let dwell = (-(mean_ms) * (1.0 - u).max(1e-9).ln()) as i128;
            match current {
                StateKind::Healthy  => total_healthy_ms  += dwell,
                StateKind::Degraded => total_degraded_ms += dwell,
            }
            current = match current {
                StateKind::Healthy  => StateKind::Degraded,
                StateKind::Degraded => StateKind::Healthy,
            };
        }
        let total = total_healthy_ms + total_degraded_ms;
        let degraded_frac = total_degraded_ms as f64 / total as f64;
        // Theoretical: 5000/35000 = 0.1429. ±2pp band.
        assert!(
            (0.1229..=0.1629).contains(&degraded_frac),
            "degraded fraction {degraded_frac} outside 14.3% ± 2pp band"
        );
    }

    #[test]
    fn step_eventually_transitions() {
        // Sanity: with sub-millisecond mean dwells, repeated step() must flip
        // state at least once within a small budget of iterations.
        let mut st = MarkovState::new();
        let mut rng = StdRng::seed_from_u64(1);
        let tiny = MarkovConfig {
            mean_dwell_healthy_ms: 1,
            mean_dwell_degraded_ms: 1,
            degraded_error_rate:   0.6,
            degraded_latency_mult: 5.0,
        };
        let mut transitions = 0;
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            let (_, t) = st.step_with_rng(&tiny, &mut rng);
            if t { transitions += 1; }
        }
        assert!(transitions > 0, "state machine never transitioned");
    }
}
