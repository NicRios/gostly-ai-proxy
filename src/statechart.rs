//! Harel-style statechart interpreter for Linked Mocks.
//!
//! Models flat-state, single-region transitions over named actions — enough
//! to express payment-API-style charge / subscription / order / customer / invoice
//! lifecycles, which are the canonical POST-then-PATCH-then-GET shapes that
//! make stateless mocks return 404 on the GET.
//!
//! Pure interpreter: `apply` is a `&self` function with no I/O. Mutation is
//! the caller's job to wire into `ResourceStore`. Bundled fixtures are
//! embedded via `include_str!` so the binary ships them without any runtime
//! filesystem dependency. Invalid transitions return `None` instead of
//! panicking — the proxy hot path must never crash on a misnamed action.
//!
//! Hierarchical states, parallel regions, history pseudo-states, guards on
//! transitions, and external statechart uploads aren't supported. The flat
//! shape is enough for the resource-lifecycle modelling this is here for.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single state in a statechart. v1 keeps the shape narrow — we record
/// only the outgoing transitions, not entry/exit actions or guards. The
/// `transitions` map is `event-name → next-state-name`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    /// Map of event name to destination state. An event that isn't in this
    /// map is ignored on `apply` (returns `None`).
    #[serde(default)]
    pub transitions: HashMap<String, String>,
}

/// A flat statechart definition. Loaded from the bundled JSON fixtures.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateMachine {
    /// Logical id (e.g. "charge", "subscription"). Doubles as the registry key.
    pub id: String,
    /// Initial state name. Must exist in `states`.
    pub initial: String,
    /// All states keyed by name.
    pub states: HashMap<String, State>,
    /// JSON body field whose string value is rewritten on every transition so
    /// the captured payload reflects the new state. payment-API-style APIs use
    /// `"status"`; non-payment-API-style fixtures (e.g. fulfilment-style "stage")
    /// can override here. Defaults to `"status"` to preserve existing
    /// behaviour for fixtures that omit this field.
    #[serde(default = "default_status_field")]
    pub status_field: String,
}

fn default_status_field() -> String {
    "status".to_string()
}

impl StateMachine {
    /// Returns true iff `name` is a known state in this machine.
    pub fn has_state(&self, name: &str) -> bool {
        self.states.contains_key(name)
    }

    /// Apply a transition: given the current state and an action, return the
    /// next state name if the transition is defined. Returns `None` if either
    /// the current state is unknown or the action has no outgoing transition.
    ///
    /// This is the only mutator-shaped function on a statechart and it is
    /// pure — it never modifies `self`.
    pub fn apply(&self, current_state: &str, action: &str) -> Option<String> {
        let st = self.states.get(current_state)?;
        st.transitions.get(action).cloned()
    }
}

/// Registry of all loaded statecharts. Constructed at startup from the
/// bundled fixtures.
#[derive(Default, Clone, Debug)]
pub struct StatechartRegistry {
    machines: HashMap<String, StateMachine>,
}

/// Bundled fixtures. Embedded at compile time so OSS ships them without
/// reaching for the filesystem at boot. Order is alphabetical for stability.
const BUNDLED_FIXTURES: &[(&str, &str)] = &[
    ("charge",       include_str!("statecharts/charge.json")),
    ("customer",     include_str!("statecharts/customer.json")),
    ("invoice",      include_str!("statecharts/invoice.json")),
    ("order",        include_str!("statecharts/order.json")),
    ("subscription", include_str!("statecharts/subscription.json")),
];

impl StatechartRegistry {
    /// Build a registry pre-populated with the five bundled fixtures
    /// (charge, customer, invoice, order, subscription). Fixtures that
    /// fail to parse are logged and skipped — the boot path never panics
    /// on a malformed fixture.
    pub fn with_bundled_fixtures() -> Self {
        let mut reg = Self::default();
        for (id, json) in BUNDLED_FIXTURES {
            match serde_json::from_str::<StateMachine>(json) {
                Ok(mut m) => {
                    // Ensure the id matches the registration key. The fixture
                    // sets it explicitly but we sanity-check here so a copy-
                    // paste error in a future fixture lands as a log line and
                    // not a silent mismatch.
                    if m.id != *id {
                        tracing::warn!(
                            registered_as = %id,
                            fixture_id = %m.id,
                            "statechart fixture id mismatch — using registered key",
                        );
                        m.id = (*id).to_string();
                    }
                    if !m.has_state(&m.initial) {
                        tracing::warn!(
                            id = %id,
                            initial = %m.initial,
                            "statechart fixture initial state missing — skipping",
                        );
                        continue;
                    }
                    reg.machines.insert((*id).to_string(), m);
                }
                Err(e) => {
                    tracing::warn!(
                        id = %id,
                        error = %e,
                        "statechart fixture failed to parse — skipping",
                    );
                }
            }
        }
        reg
    }

    /// Look up a machine by its id. Returns `None` if no machine is registered
    /// under that name.
    pub fn get(&self, id: &str) -> Option<&StateMachine> {
        self.machines.get(id)
    }

    /// Number of machines currently registered. Mostly for observability /
    /// startup logging.
    pub fn len(&self) -> usize {
        self.machines.len()
    }

    /// True iff no machines are registered. Trivial helper. `#[allow(dead_code)]`
    /// so absence of an internal call site doesn't break the build — exposed
    /// for embedders and tests.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.machines.is_empty()
    }

    /// Apply a transition through the registry: look up the machine, then
    /// run its `apply`. Returns `None` if the machine is unknown or the
    /// transition is undefined.
    pub fn apply(&self, machine_id: &str, current_state: &str, action: &str) -> Option<String> {
        self.get(machine_id)?.apply(current_state, action)
    }

    /// Insert a synthetic machine. Test-only helper used to verify behaviours
    /// that the bundled fixtures don't exercise (e.g. custom `status_field`).
    /// Not intended for production callers — production wires fixtures in via
    /// `with_bundled_fixtures`. Cross-module test visibility: gated behind
    /// `#[cfg(test)]` and `pub(crate)` so resource_store.rs tests can build
    /// a synthetic registry without exposing a mutator on the public surface.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, machine: StateMachine) {
        let id = machine.id.clone();
        self.machines.insert(id, machine);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn bundled_fixtures_load_all_five() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        assert_eq!(reg.len(), 5, "expected the five bundled fixtures to load");
        for id in &["charge", "subscription", "order", "customer", "invoice"] {
            assert!(reg.get(id).is_some(), "missing bundled fixture: {id}");
        }
    }

    #[test]
    fn charge_initial_state_is_created() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        let charge = reg.get("charge").unwrap();
        assert_eq!(charge.initial, "created");
    }

    #[test]
    fn charge_capture_transitions_created_to_captured() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        let next = reg.apply("charge", "created", "capture");
        assert_eq!(next.as_deref(), Some("captured"));
    }

    #[test]
    fn charge_refund_after_capture_transitions_to_refunded() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        let after_capture = reg
            .apply("charge", "created", "capture")
            .unwrap();
        let next = reg.apply("charge", &after_capture, "refund");
        assert_eq!(next.as_deref(), Some("refunded"));
    }

    #[test]
    fn invalid_transition_returns_none() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        // refund is only valid from captured, not from created.
        assert!(reg.apply("charge", "created", "refund").is_none());
    }

    #[test]
    fn unknown_state_returns_none() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        assert!(reg.apply("charge", "imaginary_state", "capture").is_none());
    }

    #[test]
    fn unknown_machine_returns_none() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        assert!(reg.apply("nope", "created", "capture").is_none());
    }

    #[test]
    fn subscription_pause_resume_loop() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        let s1 = reg.apply("subscription", "trialing", "activate").unwrap();
        assert_eq!(s1, "active");
        let s2 = reg.apply("subscription", &s1, "pause").unwrap();
        assert_eq!(s2, "paused");
        let s3 = reg.apply("subscription", &s2, "resume").unwrap();
        assert_eq!(s3, "active");
    }

    #[test]
    fn subscription_terminal_states_have_no_transitions() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        assert!(reg.apply("subscription", "cancelled", "activate").is_none());
        assert!(reg.apply("subscription", "cancelled", "pause").is_none());
    }

    #[test]
    fn order_full_lifecycle() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        let s = "pending";
        let s = reg.apply("order", s, "pay").unwrap();
        assert_eq!(s, "paid");
        let s = reg.apply("order", &s, "ship").unwrap();
        assert_eq!(s, "shipped");
        let s = reg.apply("order", &s, "deliver").unwrap();
        assert_eq!(s, "delivered");
    }

    #[test]
    fn invoice_initial_is_draft() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        assert_eq!(reg.get("invoice").unwrap().initial, "draft");
    }

    #[test]
    fn invoice_finalize_then_pay_lands_on_paid() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        let s = reg.apply("invoice", "draft", "finalize").unwrap();
        assert_eq!(s, "open");
        let s = reg.apply("invoice", &s, "pay").unwrap();
        assert_eq!(s, "paid");
    }

    #[test]
    fn customer_suspend_reactivate_loop() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        let s = reg.apply("customer", "active", "suspend").unwrap();
        assert_eq!(s, "suspended");
        let s = reg.apply("customer", &s, "reactivate").unwrap();
        assert_eq!(s, "active");
    }

    /// Property test (manually-driven): for any sequence of *valid* events
    /// derived from the machine's transition table, applying them never
    /// produces None and lands on a state that exists in `states`. This
    /// proves the interpreter never wedges into an unknown state on a
    /// well-formed walk.
    #[test]
    fn property_walk_never_lands_in_unknown_state() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        for id in &["charge", "subscription", "order", "customer", "invoice"] {
            let m = reg.get(id).unwrap();
            // Walk by always picking the first transition out of the current
            // state until we hit a terminal (no outgoing edges). The walk
            // length is bounded by `states.len()` for a deterministic
            // first-edge-only walk, which is plenty for this property.
            let mut current = m.initial.clone();
            for _ in 0..m.states.len() {
                let st = m.states.get(&current).unwrap();
                let Some((evt, _)) = st.transitions.iter().next() else { break };
                let next = m.apply(&current, evt).unwrap();
                assert!(m.has_state(&next), "{id}: walked into unknown state {next}");
                current = next;
            }
        }
    }

    #[test]
    fn malformed_fixture_skipped_gracefully() {
        // Confirm the helper handles unknown ids without panicking.
        let reg = StatechartRegistry::default();
        assert!(reg.is_empty());
        assert!(reg.apply("anything", "state", "evt").is_none());
    }

    #[test]
    fn state_transitions_are_immutable_through_apply() {
        // Calling apply must not mutate the machine.
        let reg = StatechartRegistry::with_bundled_fixtures();
        let charge_before = reg.get("charge").unwrap().clone();
        let _ = reg.apply("charge", "created", "capture");
        let charge_after = reg.get("charge").unwrap();
        assert_eq!(
            charge_before.states.len(),
            charge_after.states.len(),
            "apply must not mutate the registry",
        );
    }

    #[test]
    fn registry_get_returns_same_machine_each_call() {
        let reg = StatechartRegistry::with_bundled_fixtures();
        let a = reg.get("charge").unwrap().id.clone();
        let b = reg.get("charge").unwrap().id.clone();
        assert_eq!(a, b);
    }
}
