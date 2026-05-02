//! In-memory counter + observation collector backing the local
//! `/metrics` endpoint.
//!
//! Counters are absolute monotonic counts since process boot.
//! Observations are raw value lists (e.g. fidelity-ms samples), capped at
//! 1024 entries between drains so a runaway recording loop can't grow
//! the agent's RSS unboundedly.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

// ─── Counter / observation names ──────────────────────────────────────────────

pub mod counter_names {
    pub const AGENT_BOOTS_TOTAL:           &str = "gostly_agent_boots_total";
    pub const FIRST_REQUEST_PROXIED_TOTAL: &str = "gostly_first_request_proxied_total";
    pub const FIRST_MOCK_RECORDED_TOTAL:   &str = "gostly_first_mock_recorded_total";
    pub const FIRST_MOCK_SERVED_TOTAL:     &str = "gostly_first_mock_served_total";
}

pub mod observation_names {
    /// Mock fidelity histogram-style observations (raw ms samples).
    pub const MOCK_FIDELITY_MS: &str = "gostly_mock_fidelity_ms";
}

// ─── Collector ────────────────────────────────────────────────────────────────

#[derive(Default, Debug)]
struct CollectorInner {
    counters:     HashMap<&'static str, u64>,
    observations: HashMap<&'static str, Vec<f64>>,
}

/// Thread-safe owner of telemetry counters/observations.
///
/// Cloning is `Arc::clone` (cheap). The intended usage is:
/// 1. Construct once in `main()`.
/// 2. Stash an `Arc<TelemetryCollector>` in `AppState`.
/// 3. Increment from request handlers via `inc_counter` /
///    `record_observation`.
#[derive(Clone, Default)]
pub struct TelemetryCollector {
    inner: Arc<RwLock<CollectorInner>>,
}

impl TelemetryCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment a counter by `n`.
    pub fn inc_counter(&self, name: &'static str, n: u64) {
        let mut g = self.inner.write();
        *g.counters.entry(name).or_insert(0) += n;
    }

    /// Replace a counter with an absolute `value`.
    #[allow(dead_code)]
    pub fn set_counter(&self, name: &'static str, value: u64) {
        let mut g = self.inner.write();
        g.counters.insert(name, value);
    }

    /// Record a single observation. The bucket caps at 1024 samples
    /// between drains so a runaway recording loop can't grow the agent's
    /// RSS unboundedly.
    pub fn record_observation(&self, name: &'static str, value: f64) {
        let mut g = self.inner.write();
        let bucket = g.observations.entry(name).or_default();
        if bucket.len() < 1024 {
            bucket.push(value);
        }
    }

    /// Snapshot the current counter values. Counters are absolute and
    /// monotonic; callers that need deltas compute them from successive
    /// snapshots.
    #[allow(dead_code)]
    pub fn counters(&self) -> HashMap<&'static str, u64> {
        self.inner.read().counters.clone()
    }

    /// Drain accumulated observations. After this call the per-name
    /// buckets are empty.
    #[allow(dead_code)]
    pub fn drain_observations(&self) -> HashMap<&'static str, Vec<f64>> {
        std::mem::take(&mut self.inner.write().observations)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inc_counter_accumulates() {
        let c = TelemetryCollector::new();
        c.inc_counter(counter_names::AGENT_BOOTS_TOTAL, 1);
        c.inc_counter(counter_names::AGENT_BOOTS_TOTAL, 2);
        assert_eq!(c.counters().get(counter_names::AGENT_BOOTS_TOTAL), Some(&3));
    }

    #[test]
    fn set_counter_replaces_value() {
        let c = TelemetryCollector::new();
        c.inc_counter(counter_names::AGENT_BOOTS_TOTAL, 5);
        c.set_counter(counter_names::AGENT_BOOTS_TOTAL, 1);
        assert_eq!(c.counters().get(counter_names::AGENT_BOOTS_TOTAL), Some(&1));
    }

    #[test]
    fn observations_drain_between_calls() {
        let c = TelemetryCollector::new();
        c.record_observation(observation_names::MOCK_FIDELITY_MS, 12.0);
        c.record_observation(observation_names::MOCK_FIDELITY_MS, 18.0);
        let first = c.drain_observations();
        assert_eq!(first.get(observation_names::MOCK_FIDELITY_MS).map(|v| v.len()), Some(2));
        let second = c.drain_observations();
        assert!(second.get(observation_names::MOCK_FIDELITY_MS).is_none());
    }

    #[test]
    fn observation_bucket_caps_at_1024_samples() {
        let c = TelemetryCollector::new();
        for i in 0..2000 {
            c.record_observation(observation_names::MOCK_FIDELITY_MS, i as f64);
        }
        let drained = c.drain_observations();
        let bucket = drained.get(observation_names::MOCK_FIDELITY_MS).unwrap();
        assert_eq!(bucket.len(), 1024);
    }
}
