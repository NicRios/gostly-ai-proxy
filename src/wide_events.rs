//! Wide-events JSONL log (opt-in observability).
//!
//! A wide event is a flat JSON object with a small fixed envelope (`id`,
//! `ts`, `event_type`, `service_id`) plus a free-form `attrs` map. The
//! agent writes one event per recorded request to a local JSONL log that
//! can be tailed by an external observability pipeline.
//!
//! # Why a separate JSONL log
//!
//! The traffic JSONL (`traffic_{svc}.jsonl`) is the recorded-request
//! corpus and must stay byte-stable for replay. The wide-events log is a
//! lightweight observability surface: tiny rows, append-only, and
//! explicitly designed to be drop-in-compatible with the OTEL `gen_ai`
//! semantic-conventions wire shape so a future exporter is a passthrough.
//!
//! # File layout
//!
//! `{wide_events_dir}/wide_events.jsonl` — one JSON object per line. Newer
//! lines are always appended; the file is never rewritten in place.
//!
//! # Schema
//!
//! ```text
//! {
//!   "id":         "<u64 monotonic>",
//!   "ts":         "RFC3339 UTC",
//!   "event_type": "request_recorded",
//!   "service_id": "<svc>" | null,
//!   "attrs":      { "method": ..., "uri_pattern": ..., "workload_class": ...,
//!                   "status": ..., "latency_ms": ... }
//! }
//! ```
//!
//! `attrs` is a JSON object so future event types can add fields without a
//! schema migration.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

/// Filename of the single wide-events log. Single-file (rather than
/// per-service) because the row volume is small (~one per recorded
/// request) and the API sync needs a single stable watermark.
pub const WIDE_EVENTS_FILE: &str = "wide_events.jsonl";

/// One wide-event line as serialised to JSONL and shipped through the OTEL
/// bridge surface. Field names match the OTEL `gen_ai` semconv layout.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WideEvent {
    pub id:         String,
    pub ts:         String,
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_id: Option<String>,
    /// Free-form attribute map. Use a BTreeMap so the on-disk JSON has
    /// deterministic key ordering — easier to grep + diff in tests.
    pub attrs:      BTreeMap<String, serde_json::Value>,
}

impl WideEvent {
    /// Build the canonical `request_recorded` event. Attribute names match
    /// the OTEL `gen_ai` semconv mapping documented in the wiki page.
    pub fn request_recorded(
        service_id: Option<&str>,
        method: &str,
        uri_pattern: &str,
        status: u16,
        latency_ms: u64,
        workload_class: &str,
    ) -> Self {
        let mut attrs: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        attrs.insert("method".into(),         serde_json::Value::String(method.into()));
        attrs.insert("uri_pattern".into(),    serde_json::Value::String(uri_pattern.into()));
        attrs.insert("status".into(),         serde_json::Value::from(status));
        attrs.insert("latency_ms".into(),     serde_json::Value::from(latency_ms));
        attrs.insert("workload_class".into(), serde_json::Value::String(workload_class.into()));
        if let Some(svc) = service_id {
            attrs.insert("service_id".into(), serde_json::Value::String(svc.into()));
        }
        Self {
            id:         next_id(),
            ts:         chrono::Utc::now().to_rfc3339(),
            event_type: "request_recorded".to_string(),
            service_id: service_id.map(|s| s.to_string()),
            attrs,
        }
    }
}

/// Append one event to `{dir}/wide_events.jsonl`.
///
/// Best-effort by design — failures are logged at debug level and dropped.
/// The hot path (LEARN-mode response) must never block on this write, so
/// callers always invoke this from `tokio::spawn`.
pub async fn append(dir: &str, event: &WideEvent) {
    let path = Path::new(dir).join(WIDE_EVENTS_FILE);
    // Serialise outside the open() so a serialisation failure (impossible
    // for our schema, but compiler-checked) doesn't leave a half-open file
    // descriptor.
    let line = match serde_json::to_string(event) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "wide_event_serialize_failed");
            return;
        }
    };
    let mut file = match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(error = %e, path = %path.display(), "wide_event_open_failed");
            return;
        }
    };
    if let Err(e) = file.write_all(line.as_bytes()).await {
        tracing::debug!(error = %e, "wide_event_write_failed");
        return;
    }
    if let Err(e) = file.write_all(b"\n").await {
        tracing::debug!(error = %e, "wide_event_write_newline_failed");
        return;
    }
    // Flush so other readers (the API sync, tests) see the bytes.
    let _ = file.flush().await;
}

/// Monotonic in-process id generator. The id is unique within an agent
/// instance; cross-instance uniqueness is provided by the `ts` field.
fn next_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts_ms = chrono::Utc::now().timestamp_millis();
    format!("we_{}_{}", ts_ms, n)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests
mod tests {
    use super::*;

    #[test]
    fn request_recorded_attribute_layout_matches_semconv() {
        let evt = WideEvent::request_recorded(
            Some("svc-1"),
            "GET",
            "/v1/users/{id}",
            200,
            42,
            "agent",
        );
        assert_eq!(evt.event_type, "request_recorded");
        assert_eq!(evt.service_id.as_deref(), Some("svc-1"));
        assert_eq!(
            evt.attrs.get("workload_class"),
            Some(&serde_json::Value::String("agent".into())),
        );
        assert_eq!(
            evt.attrs.get("method"),
            Some(&serde_json::Value::String("GET".into())),
        );
        assert_eq!(
            evt.attrs.get("status"),
            Some(&serde_json::Value::from(200u16)),
        );
        assert_eq!(
            evt.attrs.get("latency_ms"),
            Some(&serde_json::Value::from(42u64)),
        );
    }

    #[test]
    fn id_is_unique_within_burst() {
        let mut ids = std::collections::HashSet::new();
        for _ in 0..100 {
            ids.insert(next_id());
        }
        assert_eq!(ids.len(), 100, "monotonic counter must guarantee uniqueness");
    }

    #[tokio::test]
    async fn append_creates_file_and_writes_one_line() {
        let dir = tempdir();
        let evt = WideEvent::request_recorded(
            Some("svc"),
            "POST",
            "/v1/things",
            201,
            7,
            "human",
        );
        append(&dir, &evt).await;

        let path = std::path::Path::new(&dir).join(WIDE_EVENTS_FILE);
        let body = std::fs::read_to_string(&path).expect("file exists");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: WideEvent = serde_json::from_str(lines[0]).expect("valid json");
        assert_eq!(parsed, evt);
    }

    fn tempdir() -> String {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = format!("/tmp/gostly-wide-events-{}-{}", pid, nanos);
        std::fs::create_dir_all(&dir).expect("create tmp dir");
        dir
    }
}
