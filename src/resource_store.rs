//! ResourceStore — captures POST response bodies keyed by extracted id and
//! serves them on subsequent GET /collection/{id} so the proxy returns 200
//! with the original payload instead of falling through to a 404 / upstream.
//!
//! Models the canonical "POST /charges → GET /charges/{id} → 404" failure mode.
//! When the proxy records a `POST /collection` whose response body contains
//! an id field, the store captures the response body keyed by `(service_id,
//! collection, id)`. Subsequent `GET /collection/{id}` requests look up the
//! captured body and serve it back — producing 200 + the original payload
//! instead of the legacy 404.
//!
//! Concurrency model:
//!   - Outer `RwLock<HashMap<service_id, ServiceResources>>` for per-service
//!     buckets. Reads dominate; writes happen only on a captured POST.
//!   - Buckets are flat: the (collection, id) key is collapsed to a 2-tuple
//!     under the `RwLock`. We keep the lock guard short — every public method
//!     acquires the lock, does an O(1) lookup or insert, and drops it before
//!     any I/O.
//!
//! Persistence:
//!   - Every write also appends a JSONL line to
//!     `{data_dir}/resources/{service_id}.jsonl`. On startup, `load_from_disk`
//!     replays the file to rebuild the in-memory map. Re-loading the same file
//!     produces the same map (last-write-wins per (collection, id)) — see the
//!     `load_then_load_again_is_idempotent` test.
//!
//! Error policy:
//!   - No `.unwrap()` / `.expect()` / `panic!` outside `#[cfg(test)]`.
//!   - I/O failures are logged via `tracing::error!` + a Prometheus counter
//!     and otherwise swallowed. The in-memory map stays authoritative — the
//!     JSONL is a recovery aid, not a write barrier.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::statechart::StatechartRegistry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;
use thiserror::Error;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;

/// Maximum recursion depth when extracting an id from a JSON body. Three
/// levels is enough to reach `data.attributes.id` style nesting (payment-API-like)
/// and `result.charge.id` patterns without making the search expensive at
/// scale.
const MAX_ID_EXTRACTION_DEPTH: usize = 3;

/// In-memory captured-resource state.
///
/// `body` is held as `serde_json::Value` so the proxy hot path can return
/// it as JSON without re-parsing on every read; the on-disk JSONL stores
/// the same thing serialised per line.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceState {
    /// e.g. "charges", "subscriptions"
    pub collection: String,
    /// e.g. "ch_abc123"
    pub id: String,
    /// Captured response body (JSON).
    pub body: serde_json::Value,
    /// Bound statechart name (e.g. "charge"). When set, PATCH-style requests
    /// can advance state through `apply_transition`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statechart: Option<String>,
    /// Current state inside the bound statechart, if any. None when no
    /// statechart is bound or no transitions have fired yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_state: Option<String>,
    /// Capture time. Serialised as RFC3339 in the JSONL via the
    /// `serialize_systemtime` helper so files are diffable.
    #[serde(with = "systemtime_rfc3339")]
    pub created_at: SystemTime,
    /// Last update time (transitions, body merges).
    #[serde(with = "systemtime_rfc3339")]
    pub updated_at: SystemTime,
    /// Original response status — defaults to 200 if not set explicitly so a
    /// captured POST can be replayed as a synthetic GET 200.
    #[serde(default = "default_status")]
    pub status: u16,
    /// Original response Content-Type so the proxy serves the right header
    /// on lookup. Defaults to `application/json` for captured POSTs.
    #[serde(default = "default_content_type")]
    pub content_type: String,
}

fn default_status() -> u16 {
    200
}

fn default_content_type() -> String {
    "application/json".to_string()
}

/// `(collection, id) → ResourceState` per service. Held under the outer
/// `RwLock<HashMap<service_id, ServiceResources>>`.
type ServiceResources = HashMap<(String, String), ResourceState>;

/// Errors raised by the store. Public so callers can match on them; the
/// proxy hot path currently only logs them.
#[derive(Debug, Error)]
pub enum ResourceStoreError {
    /// JSON body did not contain an extractable id field.
    #[error("no id field found in response body for collection {0}")]
    IdNotFound(String),

    /// I/O failure persisting a write. Returned for callers that care; the
    /// in-memory state is already updated by the time this fires.
    #[error("io failure: {0}")]
    Io(#[from] std::io::Error),
}

/// The store itself — wrap in `Arc` so it can live on `AppState` without
/// blowing the field count budget.
#[derive(Default)]
pub struct ResourceStore {
    by_service: RwLock<HashMap<String, ServiceResources>>,
    /// Where to persist appends. Empty disables persistence (used in
    /// pure-unit tests).
    data_dir: String,
    /// Active statechart registry. Optional so unit tests can build the
    /// store without a registry. The proxy wires the bundled registry on
    /// boot.
    registry: Option<Arc<StatechartRegistry>>,
}

impl ResourceStore {
    /// Build a fresh, empty store with persistence rooted at `data_dir`. Pass
    /// an empty string to skip persistence (e.g. unit tests).
    pub fn new(data_dir: impl Into<String>, registry: Option<Arc<StatechartRegistry>>) -> Self {
        Self {
            by_service: RwLock::new(HashMap::new()),
            data_dir: data_dir.into(),
            registry,
        }
    }

    /// Look up a captured resource. O(1) under a read guard.
    pub async fn lookup(&self, service_id: &str, collection: &str, id: &str) -> Option<ResourceState> {
        let map = self.by_service.read().await;
        map.get(service_id)
            .and_then(|svc| svc.get(&(collection.to_string(), id.to_string())))
            .cloned()
    }

    /// Number of captured resources across all services. Mostly for
    /// observability and tests.
    pub async fn len(&self) -> usize {
        let map = self.by_service.read().await;
        map.values().map(|svc| svc.len()).sum()
    }

    /// Convenience: true iff `len()` is zero. Trivial helper for clippy.
    /// `#[allow(dead_code)]` so absence of an internal call site doesn't
    /// break the build — exposed for embedders and tests.
    #[allow(dead_code)]
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// List all captured resources for a service. Returns owned clones for
    /// callers that need a snapshot without holding the read guard.
    #[allow(dead_code)]
    pub async fn list(&self, service_id: &str) -> Vec<ResourceState> {
        let map = self.by_service.read().await;
        map.get(service_id)
            .map(|svc| svc.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Capture a successful POST response: extract an id from the body, store
    /// the resource, and append to the JSONL. Returns the extracted id on
    /// success. The caller already pre-determined the collection (last-segment
    /// of the request URI typically) and confirmed the response is 2xx.
    pub async fn capture_create(
        &self,
        service_id: &str,
        collection: &str,
        body: &serde_json::Value,
        id_field_hint: Option<&str>,
        content_type: Option<&str>,
        status: u16,
    ) -> Result<String, ResourceStoreError> {
        let id = extract_id(body, collection, id_field_hint)
            .ok_or_else(|| ResourceStoreError::IdNotFound(collection.to_string()))?;

        // Bind a statechart if the collection name happens to match a known
        // machine. Singular vs plural: collection names are typically plural
        // ("charges"), machine ids are singular ("charge"). Try both.
        let statechart = self.registry.as_ref().and_then(|reg| {
            if reg.get(collection).is_some() {
                Some(collection.to_string())
            } else if let Some(singular) = collection.strip_suffix('s') {
                if reg.get(singular).is_some() {
                    Some(singular.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        });

        let current_state = statechart.as_ref().and_then(|m| {
            self.registry
                .as_ref()
                .and_then(|reg| reg.get(m))
                .map(|sm| sm.initial.clone())
        });

        let now = SystemTime::now();
        let state = ResourceState {
            collection: collection.to_string(),
            id: id.clone(),
            body: body.clone(),
            statechart,
            current_state,
            created_at: now,
            updated_at: now,
            status,
            content_type: content_type
                .unwrap_or("application/json")
                .to_string(),
        };

        // Update in-memory map first.
        {
            let mut map = self.by_service.write().await;
            map.entry(service_id.to_string())
                .or_default()
                .insert((collection.to_string(), id.clone()), state.clone());
        }

        // Persist (best-effort). Persistence failure is logged + counted but
        // does not unwind the in-memory write — the alternative is dropping
        // a successfully captured resource on transient disk errors, which
        // is worse for the proxy hot path.
        if !self.data_dir.is_empty() {
            if let Err(e) = persist_jsonl(&self.data_dir, service_id, &state).await {
                tracing::error!(
                    service_id = %service_id,
                    collection = %collection,
                    id = %id,
                    error = %e,
                    "resource_store_persist_failed",
                );
                metrics::counter!(
                    "gostly_resource_store_total",
                    "operation" => "persist",
                    "outcome" => "error",
                )
                .increment(1);
            }
        }

        metrics::counter!(
            "gostly_resource_store_total",
            "operation" => "capture",
            "outcome" => "ok",
        )
        .increment(1);

        Ok(id)
    }

    /// Apply a statechart transition to a captured resource. If the resource
    /// has a bound statechart and the action is valid in the current state,
    /// updates `current_state` + `updated_at` and persists. Returns the new
    /// state on success.
    pub async fn apply_transition(
        &self,
        service_id: &str,
        collection: &str,
        id: &str,
        action: &str,
    ) -> Option<String> {
        let registry = self.registry.as_ref()?;
        let next_state: String;
        {
            let mut map = self.by_service.write().await;
            let svc = map.get_mut(service_id)?;
            let state = svc.get_mut(&(collection.to_string(), id.to_string()))?;
            let machine_id = state.statechart.clone()?;
            let current = state.current_state.clone()?;
            next_state = registry.apply(&machine_id, &current, action)?;
            state.current_state = Some(next_state.clone());
            state.updated_at = SystemTime::now();

            // Mutate the body to reflect the new state if its configured
            // status_field is a top-level string. The field name is
            // statechart-driven (see StateMachine::status_field) so fixtures
            // can name the column "stage", "phase", etc. Defaults to "status"
            // for fixtures that don't override. We only touch the field when
            // it is already a string; non-string payloads stay untouched so
            // captured shapes aren't corrupted.
            let status_field = registry
                .get(&machine_id)
                .map(|sm| sm.status_field.clone())
                .unwrap_or_else(|| "status".to_string());
            if let Some(obj) = state.body.as_object_mut() {
                if let Some(serde_json::Value::String(_)) = obj.get(&status_field) {
                    obj.insert(
                        status_field,
                        serde_json::Value::String(next_state.clone()),
                    );
                }
            }
        }

        // Persist outside the lock.
        if !self.data_dir.is_empty() {
            // Re-fetch under read guard to snapshot the just-written state.
            // The double-acquire is intentional — keeps write-guard hold time
            // short and matches the spec's "never await while holding a lock"
            // rule.
            if let Some(snapshot) = self.lookup(service_id, collection, id).await {
                if let Err(e) = persist_jsonl(&self.data_dir, service_id, &snapshot).await {
                    tracing::error!(
                        service_id = %service_id,
                        collection = %collection,
                        id = %id,
                        action = %action,
                        error = %e,
                        "resource_store_transition_persist_failed",
                    );
                }
            }
        }

        metrics::counter!(
            "gostly_resource_store_total",
            "operation" => "transition",
            "outcome" => "ok",
        )
        .increment(1);

        Some(next_state)
    }

    /// Delete a captured resource. Returns true if a resource was actually
    /// removed. Soft delete from the in-memory side; the on-disk JSONL is
    /// append-only and is not truncated, so a delete followed by a process
    /// restart will re-load the resource. Durable delete requires
    /// compaction of the JSONL, which isn't implemented.
    #[allow(dead_code)]
    pub async fn delete(&self, service_id: &str, collection: &str, id: &str) -> bool {
        let mut map = self.by_service.write().await;
        let Some(svc) = map.get_mut(service_id) else { return false };
        svc.remove(&(collection.to_string(), id.to_string())).is_some()
    }

    /// Replay the JSONL files under `{data_dir}/resources/` into the
    /// in-memory map. Idempotent — calling twice produces the same map
    /// because last-write-wins per (collection, id).
    ///
    /// Also performs a compaction pass: after replay, the per-service file
    /// is rewritten with one line per unique (collection, id), mirroring
    /// `io::load_all_service_mocks`. Without this, re-captures of the same
    /// resource accumulate unbounded across restarts. Compaction is
    /// best-effort — a write failure is logged via `tracing::error!` and
    /// counted, but does not unwind the in-memory state.
    pub async fn load_from_disk(&self) {
        if self.data_dir.is_empty() {
            return;
        }
        let dir = format!("{}/resources", self.data_dir);
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(_) => return,
        };
        while let Ok(Some(de)) = rd.next_entry().await {
            let path = de.path();
            let fname = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !fname.ends_with(".jsonl") {
                continue;
            }
            let svc_id = fname.trim_end_matches(".jsonl").to_string();
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            let raw_lines = content.lines().filter(|l| !l.trim().is_empty()).count();
            let mut bucket: ServiceResources = HashMap::new();
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(rs) = serde_json::from_str::<ResourceState>(line) {
                    bucket.insert((rs.collection.clone(), rs.id.clone()), rs);
                }
            }
            if !bucket.is_empty() {
                // Compaction: rewrite the file iff the parsed bucket has
                // fewer entries than the raw line count (i.e. there were
                // duplicate or malformed lines worth dropping). Mirrors the
                // mocks store's approach in io::load_all_service_mocks.
                if bucket.len() < raw_lines {
                    let tmp = format!("{}.tmp", path.display());
                    let compacted = bucket
                        .values()
                        .filter_map(|r| serde_json::to_string(r).ok())
                        .collect::<Vec<_>>()
                        .join("\n")
                        + "\n";
                    match tokio::fs::write(&tmp, &compacted).await {
                        Ok(()) => {
                            if let Err(e) = tokio::fs::rename(&tmp, &path).await {
                                tracing::error!(
                                    path = %path.display(),
                                    error = %e,
                                    "resource_store_compaction_rename_failed",
                                );
                                metrics::counter!(
                                    "gostly_resource_store_total",
                                    "operation" => "compact",
                                    "outcome" => "error",
                                )
                                .increment(1);
                            } else {
                                metrics::counter!(
                                    "gostly_resource_store_total",
                                    "operation" => "compact",
                                    "outcome" => "ok",
                                )
                                .increment(1);
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                path = %tmp,
                                error = %e,
                                "resource_store_compaction_write_failed",
                            );
                            metrics::counter!(
                                "gostly_resource_store_total",
                                "operation" => "compact",
                                "outcome" => "error",
                            )
                            .increment(1);
                        }
                    }
                }
                let mut map = self.by_service.write().await;
                map.insert(svc_id, bucket);
            }
        }
    }
}

/// Walk a JSON value looking for an id field. Tries the explicit hint first,
/// then a default sequence: "id", "_id", "{singular}_id". Recurses into nested
/// objects up to `MAX_ID_EXTRACTION_DEPTH` levels — nests like
/// `data.id` are the motivating cases.
///
/// Returns the id as a string when the matched field is a string OR a JSON
/// number (numbers get stringified — useful for autoincrement APIs).
pub fn extract_id(
    body: &serde_json::Value,
    collection: &str,
    hint: Option<&str>,
) -> Option<String> {
    let singular = collection.strip_suffix('s').unwrap_or(collection);
    let singular_id = format!("{singular}_id");
    let mut candidates: Vec<&str> = Vec::with_capacity(4);
    if let Some(h) = hint {
        candidates.push(h);
    }
    candidates.push("id");
    candidates.push("_id");
    candidates.push(&singular_id);

    walk_for_id(body, &candidates, 0)
}

fn walk_for_id(value: &serde_json::Value, candidates: &[&str], depth: usize) -> Option<String> {
    if depth > MAX_ID_EXTRACTION_DEPTH {
        return None;
    }
    match value {
        serde_json::Value::Object(map) => {
            // Top-of-this-object check first — id at this level always wins
            // over nested ids.
            for cand in candidates {
                if let Some(v) = map.get(*cand) {
                    if let Some(s) = stringify_id(v) {
                        return Some(s);
                    }
                }
            }
            // Then recurse.
            for v in map.values() {
                if let Some(found) = walk_for_id(v, candidates, depth + 1) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                if let Some(found) = walk_for_id(v, candidates, depth + 1) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn stringify_id(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Persist a single resource as a JSONL append. Mirrors the io.rs
/// create_dir_all-then-OpenOptions(append) pattern.
async fn persist_jsonl(
    data_dir: &str,
    service_id: &str,
    state: &ResourceState,
) -> Result<(), std::io::Error> {
    let dir = format!("{data_dir}/resources");
    tokio::fs::create_dir_all(&dir).await?;
    let path = format!("{dir}/{service_id}.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    let line = serde_json::to_string(state).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })?;
    file.write_all(line.as_bytes()).await?;
    file.write_all(b"\n").await?;
    Ok(())
}

mod systemtime_rfc3339 {
    //! Serialize SystemTime as RFC3339 strings on the wire so the on-disk
    //! JSONL is human-readable and diff-friendly.
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::SystemTime;

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let dt: chrono::DateTime<chrono::Utc> = (*t).into();
        s.serialize_str(&dt.to_rfc3339())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let s = String::deserialize(d)?;
        let dt = chrono::DateTime::parse_from_rfc3339(&s).map_err(serde::de::Error::custom)?;
        Ok(dt.with_timezone(&chrono::Utc).into())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn unique_dir(tag: &str) -> String {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = format!("/tmp/gostly-resource-store-test-{tag}-{pid}-{nanos}");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn lookup_returns_none_when_empty() {
        let store = ResourceStore::new("", None);
        assert!(store.lookup("svc", "charges", "ch_abc").await.is_none());
    }

    #[tokio::test]
    async fn capture_then_lookup_round_trip() {
        let store = ResourceStore::new("", None);
        let body = json!({"id":"ch_abc","amount":4200});
        let id = store
            .capture_create("svc", "charges", &body, None, Some("application/json"), 200)
            .await
            .unwrap();
        assert_eq!(id, "ch_abc");
        let resource = store.lookup("svc", "charges", "ch_abc").await.unwrap();
        assert_eq!(resource.collection, "charges");
        assert_eq!(resource.body["amount"], 4200);
    }

    #[tokio::test]
    async fn capture_misses_when_no_id_field() {
        let store = ResourceStore::new("", None);
        let body = json!({"amount": 4200});
        let result = store
            .capture_create("svc", "charges", &body, None, None, 200)
            .await;
        assert!(matches!(result, Err(ResourceStoreError::IdNotFound(_))));
    }

    #[tokio::test]
    async fn capture_falls_back_to_singular_id_field() {
        let store = ResourceStore::new("", None);
        let body = json!({"charge_id": "ch_xyz", "amount": 100});
        let id = store
            .capture_create("svc", "charges", &body, None, None, 201)
            .await
            .unwrap();
        assert_eq!(id, "ch_xyz");
    }

    #[tokio::test]
    async fn capture_uses_explicit_hint_first() {
        let store = ResourceStore::new("", None);
        // Body has both "id" and "object_id"; hint says "object_id".
        let body = json!({"id": "wrong", "object_id": "right"});
        let id = store
            .capture_create("svc", "things", &body, Some("object_id"), None, 201)
            .await
            .unwrap();
        assert_eq!(id, "right");
    }

    #[tokio::test]
    async fn capture_walks_nested_body_for_id() {
        let store = ResourceStore::new("", None);
        let body = json!({"data": {"attributes": {"id": "ch_nested"}}});
        let id = store
            .capture_create("svc", "charges", &body, None, None, 201)
            .await
            .unwrap();
        assert_eq!(id, "ch_nested");
    }

    #[tokio::test]
    async fn capture_id_can_be_numeric() {
        let store = ResourceStore::new("", None);
        let body = json!({"id": 42});
        let id = store
            .capture_create("svc", "items", &body, None, None, 201)
            .await
            .unwrap();
        assert_eq!(id, "42");
    }

    #[tokio::test]
    async fn statechart_bound_to_collection_singular() {
        let registry = Arc::new(StatechartRegistry::with_bundled_fixtures());
        let store = ResourceStore::new("", Some(registry));
        let body = json!({"id": "ch_state"});
        store
            .capture_create("svc", "charges", &body, None, None, 201)
            .await
            .unwrap();
        let resource = store.lookup("svc", "charges", "ch_state").await.unwrap();
        assert_eq!(resource.statechart.as_deref(), Some("charge"));
        assert_eq!(resource.current_state.as_deref(), Some("created"));
    }

    #[tokio::test]
    async fn apply_transition_advances_state() {
        let registry = Arc::new(StatechartRegistry::with_bundled_fixtures());
        let store = ResourceStore::new("", Some(registry));
        store
            .capture_create("svc", "charges", &json!({"id":"ch_trans"}), None, None, 201)
            .await
            .unwrap();
        let next = store
            .apply_transition("svc", "charges", "ch_trans", "capture")
            .await
            .unwrap();
        assert_eq!(next, "captured");
        let resource = store.lookup("svc", "charges", "ch_trans").await.unwrap();
        assert_eq!(resource.current_state.as_deref(), Some("captured"));
    }

    #[tokio::test]
    async fn invalid_transition_returns_none_and_keeps_state() {
        let registry = Arc::new(StatechartRegistry::with_bundled_fixtures());
        let store = ResourceStore::new("", Some(registry));
        store
            .capture_create("svc", "charges", &json!({"id":"ch_inv"}), None, None, 201)
            .await
            .unwrap();
        // refund is not valid from "created".
        let result = store
            .apply_transition("svc", "charges", "ch_inv", "refund")
            .await;
        assert!(result.is_none());
        let resource = store.lookup("svc", "charges", "ch_inv").await.unwrap();
        assert_eq!(resource.current_state.as_deref(), Some("created"));
    }

    #[tokio::test]
    async fn body_status_field_advances_with_transition() {
        let registry = Arc::new(StatechartRegistry::with_bundled_fixtures());
        let store = ResourceStore::new("", Some(registry));
        store
            .capture_create(
                "svc",
                "charges",
                &json!({"id":"ch_body","status":"created","amount":100}),
                None,
                None,
                201,
            )
            .await
            .unwrap();
        store
            .apply_transition("svc", "charges", "ch_body", "capture")
            .await
            .unwrap();
        let resource = store.lookup("svc", "charges", "ch_body").await.unwrap();
        assert_eq!(resource.body["status"], "captured");
    }

    #[tokio::test]
    async fn body_status_field_uses_configured_field_name() {
        // Build a registry with a synthetic machine that names its field
        // "stage" instead of the default "status". The store should rewrite
        // the "stage" field on transition and leave "status" untouched.
        use crate::statechart::{State, StateMachine, StatechartRegistry};
        let mut reg = StatechartRegistry::default();
        let mut states: HashMap<String, State> = HashMap::new();
        states.insert(
            "queued".to_string(),
            State {
                transitions: HashMap::from([("ship".to_string(), "shipped".to_string())]),
            },
        );
        states.insert("shipped".to_string(), State { transitions: HashMap::new() });
        reg.insert_for_test(StateMachine {
            id: "package".to_string(),
            initial: "queued".to_string(),
            states,
            status_field: "stage".to_string(),
        });
        let store = ResourceStore::new("", Some(Arc::new(reg)));
        store
            .capture_create(
                "svc",
                "package",
                &json!({"id":"pkg_1","stage":"queued","status":"queued"}),
                None,
                None,
                201,
            )
            .await
            .unwrap();
        store
            .apply_transition("svc", "package", "pkg_1", "ship")
            .await
            .unwrap();
        let resource = store.lookup("svc", "package", "pkg_1").await.unwrap();
        assert_eq!(resource.body["stage"], "shipped", "configured stage field should advance");
        assert_eq!(resource.body["status"], "queued", "non-configured field should not be touched");
    }

    #[tokio::test]
    async fn delete_removes_the_resource() {
        let store = ResourceStore::new("", None);
        store
            .capture_create("svc", "charges", &json!({"id":"ch_del"}), None, None, 201)
            .await
            .unwrap();
        assert!(store.delete("svc", "charges", "ch_del").await);
        assert!(store.lookup("svc", "charges", "ch_del").await.is_none());
        assert!(!store.delete("svc", "charges", "ch_del").await);
    }

    #[tokio::test]
    async fn persistence_writes_jsonl_on_capture() {
        let dir = unique_dir("persist");
        let store = ResourceStore::new(dir.clone(), None);
        store
            .capture_create("svc-a", "charges", &json!({"id":"ch_p"}), None, None, 201)
            .await
            .unwrap();
        let path = format!("{dir}/resources/svc-a.jsonl");
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(contents.contains("ch_p"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn load_from_disk_restores_in_memory_map() {
        let dir = unique_dir("load");
        {
            let store = ResourceStore::new(dir.clone(), None);
            store
                .capture_create("svc-l", "charges", &json!({"id":"ch_l1"}), None, None, 201)
                .await
                .unwrap();
            store
                .capture_create("svc-l", "charges", &json!({"id":"ch_l2"}), None, None, 201)
                .await
                .unwrap();
        }
        let store2 = ResourceStore::new(dir.clone(), None);
        store2.load_from_disk().await;
        assert!(store2.lookup("svc-l", "charges", "ch_l1").await.is_some());
        assert!(store2.lookup("svc-l", "charges", "ch_l2").await.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn load_from_disk_compacts_duplicate_lines() {
        // Write the same resource id three times to simulate re-captures
        // accumulating across restarts. After load_from_disk, the file
        // should hold exactly one line per (collection, id).
        let dir = unique_dir("compact");
        let resources_dir = format!("{dir}/resources");
        std::fs::create_dir_all(&resources_dir).unwrap();
        let path = format!("{resources_dir}/svc-c.jsonl");

        // Synthesize three serialized ResourceState lines for the same id,
        // by capturing through the store and reading back the produced file.
        {
            let store = ResourceStore::new(dir.clone(), None);
            store
                .capture_create("svc-c", "charges", &json!({"id":"ch_dupe","v":1}), None, None, 201)
                .await
                .unwrap();
            store
                .capture_create("svc-c", "charges", &json!({"id":"ch_dupe","v":2}), None, None, 201)
                .await
                .unwrap();
            store
                .capture_create("svc-c", "charges", &json!({"id":"ch_dupe","v":3}), None, None, 201)
                .await
                .unwrap();
        }
        let before = tokio::fs::read_to_string(&path).await.unwrap();
        let before_lines = before.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(before_lines, 3, "three captures should produce three appended lines");

        // Reload — load_from_disk should compact the file in place.
        let store2 = ResourceStore::new(dir.clone(), None);
        store2.load_from_disk().await;

        let after = tokio::fs::read_to_string(&path).await.unwrap();
        let after_lines = after.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(after_lines, 1, "compaction should leave one line per unique (collection,id)");

        // Last-write-wins is preserved — v=3 is the surviving body.
        let resource = store2.lookup("svc-c", "charges", "ch_dupe").await.unwrap();
        assert_eq!(resource.body["v"], 3);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn load_then_load_again_is_idempotent() {
        let dir = unique_dir("idem");
        {
            let store = ResourceStore::new(dir.clone(), None);
            store
                .capture_create("svc", "charges", &json!({"id":"ch_i"}), None, None, 201)
                .await
                .unwrap();
        }
        let store2 = ResourceStore::new(dir.clone(), None);
        store2.load_from_disk().await;
        let count_a = store2.len().await;
        store2.load_from_disk().await;
        let count_b = store2.len().await;
        assert_eq!(count_a, count_b, "second load_from_disk should not duplicate entries");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn list_returns_all_resources_for_service() {
        let store = ResourceStore::new("", None);
        store
            .capture_create("svc", "charges", &json!({"id":"ch_a"}), None, None, 201)
            .await
            .unwrap();
        store
            .capture_create("svc", "charges", &json!({"id":"ch_b"}), None, None, 201)
            .await
            .unwrap();
        store
            .capture_create("other", "charges", &json!({"id":"ch_c"}), None, None, 201)
            .await
            .unwrap();
        let list = store.list("svc").await;
        assert_eq!(list.len(), 2);
        let ids: Vec<&str> = list.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"ch_a"));
        assert!(ids.contains(&"ch_b"));
    }

    #[test]
    fn extract_id_handles_top_level_id() {
        let body = json!({"id": "x"});
        assert_eq!(extract_id(&body, "things", None).as_deref(), Some("x"));
    }

    #[test]
    fn extract_id_prefers_top_level_over_nested() {
        let body = json!({"id": "outer", "data": {"id": "inner"}});
        assert_eq!(extract_id(&body, "things", None).as_deref(), Some("outer"));
    }

    #[test]
    fn extract_id_respects_max_depth() {
        // Build a 5-deep object — the inner id should NOT be reachable.
        let body = json!({
            "a": {"b": {"c": {"d": {"e": {"id": "deep"}}}}}
        });
        // No id on the way down → returns None at depth limit.
        assert!(extract_id(&body, "things", None).is_none());
    }

    #[tokio::test]
    async fn capture_marks_metric_with_correct_outcome_label() {
        // Smoke test — we don't intercept the metrics registry here, but the
        // call must not panic. (Actual metric assertion lives in integration
        // tests with a Prometheus-rendering exporter.)
        let store = ResourceStore::new("", None);
        let _ = store
            .capture_create("svc", "charges", &json!({"id":"ch_m"}), None, None, 201)
            .await;
    }
}
