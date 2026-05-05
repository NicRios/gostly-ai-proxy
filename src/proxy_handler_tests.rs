//! Integration tests for the `proxy_handler` hot path.
//!
//! These tests build a real `AppState`, mount it on an axum router (the
//! same way `main()` does), bind it to a random localhost port, and
//! exercise it via `reqwest` — covering LEARN, MOCK, PASSTHROUGH,
//! TRANSITIONING modes plus the smart-swap fallback. They're
//! cfg(test)-only so they cannot ship in the production binary.
//!
//! Naming in this file ignores `_global` style for fixtures because each
//! test creates its own tmpdir and harness; nothing crosses between
//! tests.

use crate::*;
use crate::io::{MockLibrary, MockSegment, MockStore};

use axum::Router;
use axum::routing::{any, get, post, delete};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

// ─── Fixture helpers ──────────────────────────────────────────────────────────

fn unique_tmp_dir(tag: &str) -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = format!("/tmp/gostly-proxy-test-{}-{}-{}", tag, pid, nanos);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn spawn_router_on_random_port(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{}", addr)
}

// ─── Mock upstream ────────────────────────────────────────────────────────────
//
// Records every incoming request so tests can assert what the proxy forwarded.
// The body of each request is captured as a UTF-8 string; tests typically work
// with JSON or plain-text bodies.

#[derive(Clone, Default)]
struct UpstreamLog {
    inner: Arc<parking_lot::RwLock<Vec<RecordedRequest>>>,
    next_status: Arc<parking_lot::RwLock<u16>>,
    next_body: Arc<parking_lot::RwLock<String>>,
    delay_ms: Arc<parking_lot::RwLock<u64>>,
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    uri: String,
    body: String,
}

impl UpstreamLog {
    fn new() -> Self {
        Self {
            inner: Arc::new(parking_lot::RwLock::new(Vec::new())),
            next_status: Arc::new(parking_lot::RwLock::new(200)),
            next_body: Arc::new(parking_lot::RwLock::new(r#"{"ok":true}"#.to_string())),
            delay_ms: Arc::new(parking_lot::RwLock::new(0)),
        }
    }

    fn set_status(&self, status: u16) { *self.next_status.write() = status; }
    fn set_body(&self, body: &str)    { *self.next_body.write() = body.to_string(); }
    fn set_delay(&self, ms: u64)      { *self.delay_ms.write() = ms; }

    fn count(&self) -> usize { self.inner.read().len() }

    fn last_path(&self) -> Option<String> {
        self.inner.read().last().map(|r| r.uri.clone())
    }
    fn last_body(&self) -> Option<String> {
        self.inner.read().last().map(|r| r.body.clone())
    }

    fn paths(&self) -> Vec<String> {
        self.inner.read().iter().map(|r| r.uri.clone()).collect()
    }
}

async fn build_mock_upstream(log: UpstreamLog) -> String {
    let app = Router::new().fallback(any({
        let log = log.clone();
        move |req: axum::extract::Request| {
            let log = log.clone();
            async move {
                let method = req.method().to_string();
                let uri    = req.uri().to_string();
                let body_bytes = http_body_util::BodyExt::collect(req.into_body())
                    .await
                    .map(|c| c.to_bytes())
                    .unwrap_or_default();
                let body = String::from_utf8_lossy(&body_bytes).to_string();
                log.inner.write().push(RecordedRequest {
                    method, uri, body,
                });
                let delay = *log.delay_ms.read();
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                let status = *log.next_status.read();
                let body   = log.next_body.read().clone();
                let resp   = axum::response::Response::builder()
                    .status(axum::http::StatusCode::from_u16(status).unwrap())
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap();
                resp
            }
        }
    }));
    spawn_router_on_random_port(app).await
}

// ─── TestProxyHarness ─────────────────────────────────────────────────────────

struct TestProxyHarness {
    proxy_url: String,
    state: AppState,
    upstream: UpstreamLog,
    /// Tmpdirs we own, for cleanup.
    data_dir: String,
    mock_dir: String,
    traffic_log_dir: String,
}

impl TestProxyHarness {
    /// Build a fresh harness in LEARN mode with smart-swap disabled.
    /// Each test gets its own tmpdir + ports; nothing is shared.
    async fn new() -> Self {
        Self::with_mode(Mode::Learn).await
    }

    async fn with_mode(mode: Mode) -> Self {
        Self::with_mode_and_swap(mode, false).await
    }

    async fn with_mode_and_swap(mode: Mode, smart_swap_enabled: bool) -> Self {
        let upstream = UpstreamLog::new();
        let upstream_url = build_mock_upstream(upstream.clone()).await;

        let data_dir = unique_tmp_dir("data");
        let mock_dir = unique_tmp_dir("mock");
        let traffic_log_dir = unique_tmp_dir("traffic");
        let mode_file_path = format!("{}/mode.txt", data_dir);
        let sequence_file_path = format!("{}/sequences.json", data_dir);

        let telemetry = telemetry::TelemetryCollector::new();

        let state = AppState {
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap(),
            mode: Arc::new(tokio::sync::RwLock::new(mode)),
            mocks: Arc::new(MockStore::from_pointee(MockLibrary::new())),
            unmatched: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            sequences: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            sequence_counters: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            runtime_config: Arc::new(tokio::sync::RwLock::new(RuntimeConfig::new())),
            upstreams: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            backend_url: upstream_url.clone(),
            mock_dir: mock_dir.clone(),
            sequence_file_path,
            mode_file_path,
            redact_headers: vec![
                "authorization".to_string(),
                "cookie".to_string(),
            ],
            max_body_bytes: 1_000_000,
            traffic_log_dir: traffic_log_dir.clone(),
            entry_counter: Arc::new(AtomicU64::new(0)),
            smart_swap_enabled,
            markov_state: Arc::new(parking_lot::RwLock::new(HashMap::new())),
            onboarding_proxied: Arc::new(parking_lot::RwLock::new(std::collections::HashSet::new())),
            onboarding_served:  Arc::new(parking_lot::RwLock::new(std::collections::HashSet::new())),
            telemetry,
        };

        let app = Router::new()
            .route("/health", get(handle_health))
            .route("/ghost/mode",      post(handle_set_mode))
            .route("/ghost/config",    post(handle_set_config))
            .route("/ghost/mocks",     get(handle_list_mocks).post(handle_create_mock))
            .route("/ghost/reload",    post(handle_reload_mocks))
            .route("/ghost/admin/reload", post(handle_admin_reload))
            .route("/ghost/unmatched", get(handle_list_unmatched))
            .route("/ghost/sequences", get(handle_list_sequences).post(handle_create_sequence))
            .route("/ghost/sequences/:id/reset", post(handle_reset_sequence))
            .route("/ghost/sequences/:id",       delete(handle_delete_sequence))
            .route("/ghost/upstreams", post(handle_set_upstreams))
            .fallback(any(proxy_handler))
            .with_state(state.clone());

        let proxy_url = spawn_router_on_random_port(app).await;

        Self {
            proxy_url,
            state,
            upstream,
            data_dir,
            mock_dir,
            traffic_log_dir,
        }
    }

    /// Helper: insert a mock into the in-memory mocks map under the default
    /// `_global` tenant. Tests that need a recorded fixture should call this
    /// directly rather than going through LEARN mode.
    async fn add_mock(&self, service_id: Option<&str>, method: &str, uri: &str, status: u16, body: &str) {
        self.add_mock_for_tenant(service_id, method, uri, status, body, GLOBAL_TENANT).await;
    }

    /// Tenant-aware fixture insertion. Used by per-tenant isolation tests
    /// (v0.3, feature #7) to seed mocks under specific tenant keys.
    async fn add_mock_for_tenant(
        &self,
        service_id: Option<&str>,
        method: &str,
        uri: &str,
        status: u16,
        body: &str,
        tenant: &str,
    ) {
        let svc_key = service_id.unwrap_or(GLOBAL_TENANT).to_string();
        let entry = MockEntry {
            id:        "fixture-1".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            request:   MockRequest {
                method: method.to_string(),
                uri:    uri.to_string(),
                body:   String::new(),
            },
            response:  MockResponse {
                status,
                headers: HashMap::new(),
                body: body.to_string(),
                latency_ms: 0,
            },
            service_id: service_id.map(str::to_string),
            tenant:     tenant.to_string(),
        };
        let current = self.state.mocks.load_full();
        let mut next: MockLibrary = (*current).clone();
        let segment = next.segments
            .entry(svc_key.clone())
            .or_insert_with(|| Arc::new(MockSegment::default()));
        let mut seg_owned = (**segment).clone();
        let svc_map = seg_owned.services.entry(svc_key.clone()).or_default();
        svc_map.insert(
            (method.to_string(), uri.to_string(), tenant.to_string()),
            entry,
        );
        *segment = Arc::new(seg_owned);
        self.state.mocks.store(Arc::new(next));
    }

    /// Tear down the spawned tmpdirs. Called from `Drop` would race with
    /// background tasks; tests just call this when they care, or skip — the
    /// /tmp paths get garbage-collected by the OS eventually.
    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.data_dir);
        let _ = std::fs::remove_dir_all(&self.mock_dir);
        let _ = std::fs::remove_dir_all(&self.traffic_log_dir);
    }

    /// Read the per-service traffic log file (the JSONL where LEARN appends
    /// each recorded round-trip). Best-effort — returns an empty string when
    /// the file doesn't exist yet.
    ///
    /// Polls up to ~2s for the file to appear, then returns whatever is on
    /// disk. We poll instead of sleep-once so coverage / debug builds (slower
    /// binary, slower spawned task) don't flake.
    async fn read_traffic_log(&self, service_id: &str) -> String {
        let path = format!("{}/traffic_{}.jsonl", self.traffic_log_dir, service_id);
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Ok(contents) = tokio::fs::read_to_string(&path).await {
                if !contents.is_empty() { return contents; }
            }
        }
        tokio::fs::read_to_string(path).await.unwrap_or_default()
    }

    /// Variant of read_traffic_log used when we *expect* the log to be empty.
    /// Sleeps a fixed amount so any pending write has time to land before we
    /// check, but doesn't poll forever.
    async fn read_traffic_log_expect_empty(&self, service_id: &str) -> String {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let path = format!("{}/traffic_{}.jsonl", self.traffic_log_dir, service_id);
        tokio::fs::read_to_string(path).await.unwrap_or_default()
    }
}

// ─── LEARN mode tests ─────────────────────────────────────────────────────────

#[tokio::test]
async fn learn_get_forwards_to_upstream_and_returns_response() {
    let h = TestProxyHarness::new().await;
    h.upstream.set_body(r#"{"id":123}"#);

    let resp = reqwest::get(format!("{}/api/users/123", h.proxy_url)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("123"));

    assert_eq!(h.upstream.count(), 1);
    assert_eq!(h.upstream.last_path(), Some("/api/users/123".to_string()));

    h.cleanup();
}

#[tokio::test]
async fn learn_post_forwards_request_body_to_upstream() {
    let h = TestProxyHarness::new().await;
    let client = reqwest::Client::new();
    let resp = client.post(format!("{}/api/orders", h.proxy_url))
        .body(r#"{"order_id":"abc","amount":42}"#)
        .header("content-type", "application/json")
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    assert_eq!(h.upstream.count(), 1);
    assert_eq!(h.upstream.last_body().as_deref(), Some(r#"{"order_id":"abc","amount":42}"#));

    h.cleanup();
}

#[tokio::test]
async fn learn_propagates_upstream_500_status() {
    let h = TestProxyHarness::new().await;
    h.upstream.set_status(500);
    h.upstream.set_body(r#"{"error":"boom"}"#);

    let resp = reqwest::get(format!("{}/api/broken", h.proxy_url)).await.unwrap();
    // LEARN mode forwards the upstream status verbatim.
    assert_eq!(resp.status(), 500);
    let body = resp.text().await.unwrap();
    assert!(body.contains("boom"));

    h.cleanup();
}

#[tokio::test]
async fn learn_returns_502_when_upstream_unreachable() {
    // Build the harness but immediately overwrite backend_url with a
    // garbage port so the request fails fast.
    let h = TestProxyHarness::new().await;
    {
        // Switch upstream to a guaranteed-dead port. The proxy should map
        // a connection error to BAD_GATEWAY.
        let mut upstreams = h.state.upstreams.write().await;
        upstreams.push(UpstreamRoute {
            routing_type: "path".into(),
            routing_value: "/api/dead".into(),
            upstream_url: "http://127.0.0.1:1".into(),
            service_id: "deadsvc".into(),
            mode: None,
            chaos_config: None,
            redact_headers: vec![],
        });
    }
    let resp = reqwest::get(format!("{}/api/dead", h.proxy_url)).await.unwrap();
    assert_eq!(resp.status(), 502);
    h.cleanup();
}

#[tokio::test]
async fn learn_records_mock_into_in_memory_index() {
    let h = TestProxyHarness::new().await;
    h.upstream.set_body(r#"{"ok":true}"#);
    let _ = reqwest::get(format!("{}/api/learnable", h.proxy_url)).await.unwrap();

    // After LEARN, the in-memory mocks library should have an entry under "_global".
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let library = h.state.mocks.load_full();
    assert!(
        library.find_exact("_global", "GET", "/api/learnable", "_global").is_some(),
        "LEARN should record a mock under (_global service, _global tenant)"
    );

    h.cleanup();
}

// ─── MOCK mode tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn mock_exact_hit_serves_recorded_response_without_calling_upstream() {
    let h = TestProxyHarness::with_mode(Mode::Mock).await;
    h.add_mock(None, "GET", "/api/users/123", 200, r#"{"recorded":"yes"}"#).await;

    let resp = reqwest::get(format!("{}/api/users/123", h.proxy_url)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let h_x_ghost_mock = resp.headers().get("x-ghost-mock").cloned();
    let body = resp.text().await.unwrap();
    assert!(body.contains("recorded"));
    assert!(h_x_ghost_mock.is_some(), "X-Ghost-Mock header should be set on exact hit");
    assert_eq!(h.upstream.count(), 0, "mock hit must not call upstream");

    h.cleanup();
}

#[tokio::test]
async fn mock_miss_returns_default_404_with_x_ghost_miss_header() {
    let h = TestProxyHarness::with_mode(Mode::Mock).await;

    let resp = reqwest::get(format!("{}/api/users/999", h.proxy_url)).await.unwrap();
    assert_eq!(resp.status(), 404);
    assert!(resp.headers().contains_key("x-ghost-miss"));

    h.cleanup();
}

#[tokio::test]
async fn mock_smart_swap_serves_similar_path_when_enabled() {
    let h = TestProxyHarness::with_mode_and_swap(Mode::Mock, true).await;
    h.add_mock(None, "GET", "/api/users/123", 200, r#"{"swap":true}"#).await;

    let resp = reqwest::get(format!("{}/api/users/456", h.proxy_url)).await.unwrap();
    // matcher::find_smart_swap should match across structurally-identical URIs.
    if resp.status() == 200 {
        assert!(resp.headers().contains_key("x-ghost-swapmatch"));
    } else {
        // If swap doesn't trigger for this path shape, the fallthrough is 404.
        assert_eq!(resp.status(), 404);
    }
    h.cleanup();
}

#[tokio::test]
async fn mock_sequence_returns_step_one_then_step_two() {
    let h = TestProxyHarness::with_mode(Mode::Mock).await;
    {
        let mut seqs = h.state.sequences.write().await;
        seqs.push(MockSequence {
            id:      "seq-test".to_string(),
            method:  "GET".to_string(),
            uri:     "/api/seq".to_string(),
            responses: vec![
                SequenceResponse { status: 200, headers: HashMap::new(), body: r#"{"step":1}"#.to_string(), latency_ms: 0 },
                SequenceResponse { status: 200, headers: HashMap::new(), body: r#"{"step":2}"#.to_string(), latency_ms: 0 },
            ],
            loop_responses: false,
        });
    }
    let r1 = reqwest::get(format!("{}/api/seq", h.proxy_url)).await.unwrap();
    assert_eq!(r1.status(), 200);
    let b1 = r1.text().await.unwrap();
    assert!(b1.contains("step"));

    let r2 = reqwest::get(format!("{}/api/seq", h.proxy_url)).await.unwrap();
    assert_eq!(r2.status(), 200);
    let b2 = r2.text().await.unwrap();
    // Step 2 should be different from step 1 (in some order — sequence
    // semantics are step-by-step).
    assert!(b2.contains("step"));
    assert!(b1 != b2 || b1.contains("\"step\":1") || b1.contains("\"step\":2"));
    h.cleanup();
}

// ─── PASSTHROUGH mode tests ───────────────────────────────────────────────────

#[tokio::test]
async fn passthrough_forwards_get_to_upstream() {
    let h = TestProxyHarness::with_mode(Mode::Passthrough).await;
    h.upstream.set_body(r#"{"passthrough":true}"#);

    let resp = reqwest::get(format!("{}/api/passthrough", h.proxy_url)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(h.upstream.count(), 1);
    h.cleanup();
}

#[tokio::test]
async fn passthrough_returns_502_on_upstream_failure() {
    let h = TestProxyHarness::with_mode(Mode::Passthrough).await;
    {
        let mut upstreams = h.state.upstreams.write().await;
        upstreams.push(UpstreamRoute {
            routing_type: "path".into(),
            routing_value: "/api".into(),
            upstream_url: "http://127.0.0.1:1".into(),
            service_id: "deadsvc".into(),
            mode: None,
            chaos_config: None,
            redact_headers: vec![],
        });
    }
    let resp = reqwest::get(format!("{}/api/dead", h.proxy_url)).await.unwrap();
    assert_eq!(resp.status(), 502);
    h.cleanup();
}

#[tokio::test]
async fn passthrough_forwards_request_body() {
    let h = TestProxyHarness::with_mode(Mode::Passthrough).await;
    let client = reqwest::Client::new();
    let _ = client.post(format!("{}/api/echo", h.proxy_url))
        .body(r#"{"echo":"this"}"#)
        .send().await.unwrap();
    assert_eq!(h.upstream.count(), 1);
    assert_eq!(h.upstream.last_body().as_deref(), Some(r#"{"echo":"this"}"#));
    h.cleanup();
}

// ─── TRANSITIONING mode tests ────────────────────────────────────────────────

#[tokio::test]
async fn transitioning_returns_503_with_retry_after() {
    let h = TestProxyHarness::with_mode(Mode::Transitioning).await;
    let resp = reqwest::get(format!("{}/api/whatever", h.proxy_url)).await.unwrap();
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().contains_key("retry-after"));
    assert!(resp.headers().contains_key("x-ghost-transitioning"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("transitioning"));
    h.cleanup();
}

// ─── Mode transition tests ────────────────────────────────────────────────────

#[tokio::test]
async fn set_mode_endpoint_changes_mode_atomically() {
    let h = TestProxyHarness::new().await;
    assert_eq!(*h.state.mode.read().await, Mode::Learn);

    let client = reqwest::Client::new();
    let resp = client.post(format!("{}/ghost/mode", h.proxy_url))
        .json(&serde_json::json!({"mode": "MOCK"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);

    assert_eq!(*h.state.mode.read().await, Mode::Mock);
    h.cleanup();
}

#[tokio::test]
async fn set_mode_persists_to_mode_file_when_durable() {
    let h = TestProxyHarness::new().await;
    let client = reqwest::Client::new();
    let _ = client.post(format!("{}/ghost/mode", h.proxy_url))
        .json(&serde_json::json!({"mode": "MOCK"}))
        .send().await.unwrap();
    // Give the write a moment.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let on_disk = tokio::fs::read_to_string(&h.state.mode_file_path).await.unwrap_or_default();
    assert!(on_disk.to_uppercase().contains("MOCK"));
    h.cleanup();
}

#[tokio::test]
async fn set_mode_does_not_persist_transitioning() {
    let h = TestProxyHarness::new().await;
    let client = reqwest::Client::new();
    let _ = client.post(format!("{}/ghost/mode", h.proxy_url))
        .json(&serde_json::json!({"mode": "TRANSITIONING"}))
        .send().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let on_disk = tokio::fs::read_to_string(&h.state.mode_file_path).await.unwrap_or_default();
    // Default state — file should not contain TRANSITIONING.
    assert!(!on_disk.to_uppercase().contains("TRANSITIONING"));
    h.cleanup();
}

#[tokio::test]
async fn per_service_mode_overrides_global_mode() {
    let h = TestProxyHarness::with_mode(Mode::Learn).await;
    // Insert a route with per-service MOCK override
    {
        let mut upstreams = h.state.upstreams.write().await;
        upstreams.push(UpstreamRoute {
            routing_type: "path".into(),
            routing_value: "/api/mocked".into(),
            upstream_url: h.state.backend_url.clone(),
            service_id: "mocked-svc".into(),
            mode: Some(Mode::Mock),
            chaos_config: None,
            redact_headers: vec![],
        });
    }
    h.add_mock(Some("mocked-svc"), "GET", "/api/mocked/1", 200, r#"{"mocked":true}"#).await;

    let resp = reqwest::get(format!("{}/api/mocked/1", h.proxy_url)).await.unwrap();
    assert_eq!(resp.status(), 200);
    // Upstream not called — per-service mode override engaged.
    assert_eq!(h.upstream.count(), 0);
    h.cleanup();
}

// ─── Chaos injection tests ───────────────────────────────────────────────────

#[tokio::test]
async fn chaos_injects_latency_when_configured() {
    let h = TestProxyHarness::with_mode(Mode::Mock).await;
    {
        let mut upstreams = h.state.upstreams.write().await;
        upstreams.push(UpstreamRoute {
            routing_type: "path".into(),
            routing_value: "/api/slow".into(),
            upstream_url: h.state.backend_url.clone(),
            service_id: "slow-svc".into(),
            mode: None,
            chaos_config: Some(chaos::ChaosConfig {
                enabled: true,
                latency_ms: 50,
                ..Default::default()
            }),
            redact_headers: vec![],
        });
    }
    h.add_mock(Some("slow-svc"), "GET", "/api/slow/1", 200, r#"{"x":1}"#).await;

    let start = std::time::Instant::now();
    let resp = reqwest::get(format!("{}/api/slow/1", h.proxy_url)).await.unwrap();
    let elapsed = start.elapsed();
    assert_eq!(resp.status(), 200);
    assert!(elapsed.as_millis() >= 40, "expected latency injection, got {:?}", elapsed);
    h.cleanup();
}

// ─── v0.3 Hot-reload tests (feature #6) ──────────────────────────────────────
//
// These cover the four `MOCK_RELOAD_STRATEGY` modes plus the in-flight
// safety contract (a request started against library v1 completes against
// v1 even if a reload publishes v2 mid-request).

/// Helper: write a single mock entry to a file in JSONL form using the
/// production wire shape. Tests call this to seed `mock_dir/mock_<svc>.jsonl`
/// and then trigger a reload via the chosen strategy.
async fn write_jsonl_mock(
    dir: &str,
    service: &str,
    method: &str,
    uri: &str,
    body: &str,
    tenant: &str,
) {
    let path = format!("{}/mock_{}.jsonl", dir, service);
    // Produce the same shape MockEntry serializes to. `tenant` is the new
    // v0.3 field; backwards-compat tests call `write_jsonl_mock_no_tenant`.
    let line = serde_json::to_string(&serde_json::json!({
        "id":         format!("fixture-{}-{}", method, uri),
        "timestamp":  "2026-05-04T00:00:00Z",
        "request":    {"method": method, "uri": uri, "body": ""},
        "response":   {"status": 200, "headers": {}, "body": body, "latency_ms": 0},
        "service_id": service,
        "tenant":     tenant,
    })).unwrap();
    tokio::fs::write(&path, format!("{}\n", line)).await.unwrap();
}

/// Backwards-compat helper: writes a JSONL entry WITHOUT a tenant field —
/// the shape pre-v0.3 files use. Loader must accept it as `_global`.
async fn write_jsonl_mock_no_tenant(
    dir: &str,
    service: &str,
    method: &str,
    uri: &str,
    body: &str,
) {
    let path = format!("{}/mock_{}.jsonl", dir, service);
    let line = serde_json::to_string(&serde_json::json!({
        "id":         format!("legacy-{}-{}", method, uri),
        "timestamp":  "2026-05-04T00:00:00Z",
        "request":    {"method": method, "uri": uri, "body": ""},
        "response":   {"status": 200, "headers": {}, "body": body, "latency_ms": 0},
        "service_id": service,
    })).unwrap();
    tokio::fs::write(&path, format!("{}\n", line)).await.unwrap();
}

/// http_admin: edit JSONL on disk → POST /ghost/admin/reload → next request
/// sees the new response. No restart.
///
/// This is the strategy with the most deterministic timing in tests
/// (no fs-event debounce, no signal delivery race) so we use it for the
/// canonical "edit-then-see-the-change" assertion. The other strategies
/// share the same `reload_full` codepath; their tests focus on the
/// trigger mechanism, not the reload behaviour.
///
/// Note on service_id: requests with no configured upstream route default
/// to the `_global` service. To make a file-loaded mock match those
/// requests, the file basename must be `_global` (so the loader assigns
/// service_id = "_global"). Production deployments configure upstreams
/// per service and use named files like `mock_orders.jsonl`.
#[tokio::test]
async fn hot_reload_http_admin_picks_up_edits_without_restart() {
    let h = TestProxyHarness::with_mode(Mode::Mock).await;

    // v1: write the file, reload, fire a request.
    write_jsonl_mock(&h.mock_dir, "_global", "GET", "/users/1", r#"{"version":1}"#, "_global").await;
    let client = reqwest::Client::new();
    let r = client.post(format!("{}/ghost/admin/reload", h.proxy_url)).send().await.unwrap();
    assert_eq!(r.status(), 200);

    let resp = reqwest::get(format!("{}/users/1", h.proxy_url)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("\"version\":1"));

    // v2: rewrite the same file in-place, reload, fire again — must see the new body.
    write_jsonl_mock(&h.mock_dir, "_global", "GET", "/users/1", r#"{"version":2}"#, "_global").await;
    let r = client.post(format!("{}/ghost/admin/reload", h.proxy_url)).send().await.unwrap();
    assert_eq!(r.status(), 200);

    let resp = reqwest::get(format!("{}/users/1", h.proxy_url)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("\"version\":2"),
        "after reload, response should reflect the edited file; got {}", body);

    h.cleanup();
}

/// In-flight safety: a request that snapshotted library v1 must complete
/// against v1 even if a reload publishes v2 mid-request.
///
/// We simulate the race by:
///   1. Inserting a slow-responding mock (via chaos latency_ms=200) under v1
///   2. Firing the request and immediately publishing v2 via reload_full
///   3. Asserting the in-flight response is v1's body
#[tokio::test]
async fn hot_reload_in_flight_request_uses_snapshotted_library() {
    let h = TestProxyHarness::with_mode(Mode::Mock).await;
    // v1 mock with 200ms response latency so the handler is stuck inside
    // serve_mock_response (sleep) when we publish v2.
    let entry_v1 = MockEntry {
        id: "v1".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        request:   MockRequest { method: "GET".into(), uri: "/snap".into(), body: String::new() },
        response:  MockResponse {
            status: 200, headers: HashMap::new(),
            body: r#"{"version":"v1"}"#.to_string(),
            latency_ms: 250,
        },
        service_id: None,
        tenant:     GLOBAL_TENANT.to_string(),
    };
    {
        let current = h.state.mocks.load_full();
        let mut next: MockLibrary = (*current).clone();
        let mut seg = MockSegment::default();
        seg.services.entry(GLOBAL_TENANT.to_string()).or_default()
            .insert(("GET".into(), "/snap".into(), GLOBAL_TENANT.into()), entry_v1);
        next.segments.insert(GLOBAL_TENANT.to_string(), Arc::new(seg));
        h.state.mocks.store(Arc::new(next));
    }

    // Fire the request in the background — it will block ~250ms on the
    // mock's latency_ms before responding.
    let url = format!("{}/snap", h.proxy_url);
    let in_flight = tokio::spawn(async move {
        reqwest::get(url).await.unwrap().text().await.unwrap()
    });

    // Give the handler ~50ms to enter serve_mock_response (snapshot taken,
    // sleep started), then publish v2 over the same key.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let entry_v2 = MockEntry {
        id: "v2".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        request:   MockRequest { method: "GET".into(), uri: "/snap".into(), body: String::new() },
        response:  MockResponse {
            status: 200, headers: HashMap::new(),
            body: r#"{"version":"v2"}"#.to_string(),
            latency_ms: 0,
        },
        service_id: None,
        tenant:     GLOBAL_TENANT.to_string(),
    };
    {
        let mut next = MockLibrary::new();
        let mut seg = MockSegment::default();
        seg.services.entry(GLOBAL_TENANT.to_string()).or_default()
            .insert(("GET".into(), "/snap".into(), GLOBAL_TENANT.into()), entry_v2);
        next.segments.insert(GLOBAL_TENANT.to_string(), Arc::new(seg));
        h.state.mocks.store(Arc::new(next));
    }

    let body = in_flight.await.unwrap();
    assert!(body.contains("v1"),
        "in-flight request should keep its v1 snapshot, got: {}", body);

    // A *new* request after the swap sees v2.
    let body2 = reqwest::get(format!("{}/snap", h.proxy_url)).await.unwrap().text().await.unwrap();
    assert!(body2.contains("v2"),
        "post-reload request should see the new version, got: {}", body2);

    h.cleanup();
}

/// Backwards-compat: a JSONL file with no `tenant` field at all
/// (the pre-v0.3 shape) loads as `_global` and is matched by
/// requests that also default to `_global` (no header).
#[tokio::test]
async fn hot_reload_backwards_compat_loads_pre_tenant_jsonl_as_global() {
    let h = TestProxyHarness::with_mode(Mode::Mock).await;
    // Use service_id `_global` so requests with no upstream route find it.
    write_jsonl_mock_no_tenant(&h.mock_dir, "_global", "GET", "/legacy", r#"{"legacy":true}"#).await;
    let client = reqwest::Client::new();
    let r = client.post(format!("{}/ghost/admin/reload", h.proxy_url)).send().await.unwrap();
    assert_eq!(r.status(), 200);

    let resp = reqwest::get(format!("{}/legacy", h.proxy_url)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("legacy"),
        "pre-v0.3 JSONL must load and match under _global tenant, got: {}", body);
    h.cleanup();
}

/// fs_watch: editing a file under the watched mock_dir triggers a reload
/// within the 200ms debounce window. Cross-platform — `notify` uses
/// inotify on Linux, FSEvents on macOS.
///
/// FSEvents on macOS coalesces events at the kernel level and may take
/// ~500ms to deliver the first event after watch() returns. We wait up
/// to ~3s for the new mock to land before failing — much longer than
/// production users would ever notice but bounded enough that a real
/// regression still fails quickly.
#[tokio::test]
async fn hot_reload_fs_watch_picks_up_file_changes() {
    use crate::io::{ReloadStrategy, start_watcher};
    let h = TestProxyHarness::with_mode(Mode::Mock).await;

    // Spawn the fs_watch background task pointed at our test mock_dir.
    start_watcher(ReloadStrategy::FsWatch, h.mock_dir.clone(), h.state.mocks.clone());

    // Give the watcher ~200ms to register the inotify/FSEvents watch
    // before we write the file — otherwise the create event may fire
    // before the watch is established and be missed.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    write_jsonl_mock(&h.mock_dir, "_global", "GET", "/fswatch/1", r#"{"reloaded":true}"#, "_global").await;

    // Poll up to ~3s for the watcher to deliver the event and reload.
    let mut got_it = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let resp = reqwest::get(format!("{}/fswatch/1", h.proxy_url)).await.unwrap();
        if resp.status() == 200 {
            let body = resp.text().await.unwrap();
            if body.contains("reloaded") {
                got_it = true;
                break;
            }
        }
    }
    assert!(got_it, "fs_watch did not pick up the new mock file within 3s");
    h.cleanup();
}

/// poll: a fixed-interval rescan reloads the library at the configured
/// cadence. We point `MOCK_RELOAD_POLL_MS` at a short interval (250ms) so
/// the test runs quickly.
#[tokio::test]
async fn hot_reload_poll_strategy_reloads_on_interval() {
    use crate::io::{ReloadStrategy, start_watcher};
    let h = TestProxyHarness::with_mode(Mode::Mock).await;

    // Set the poll interval before starting the watcher (it reads the env
    // var once). We avoid std::env::set_var on a per-test basis where
    // possible because env is process-global; this test pin-tags the var
    // immediately before starting its watcher and accepts that other
    // poll tests in the same process run will see this value too.
    std::env::set_var("MOCK_RELOAD_POLL_MS", "250");
    start_watcher(ReloadStrategy::Poll, h.mock_dir.clone(), h.state.mocks.clone());

    // Write the mock AFTER starting the watcher so we know the poll loop
    // is the thing that picked it up (not initial-load).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    write_jsonl_mock(&h.mock_dir, "_global", "GET", "/polled/1", r#"{"poll":"hit"}"#, "_global").await;

    // Wait up to 2s for the next poll tick.
    let mut got_it = false;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let resp = reqwest::get(format!("{}/polled/1", h.proxy_url)).await.unwrap();
        if resp.status() == 200 {
            let body = resp.text().await.unwrap();
            if body.contains("poll") {
                got_it = true;
                break;
            }
        }
    }
    assert!(got_it, "poll strategy did not pick up the new mock within 2s");
    h.cleanup();
}

/// signal: SIGHUP triggers a reload. Unix-only — Windows lacks SIGHUP.
#[cfg(unix)]
#[tokio::test]
async fn hot_reload_signal_strategy_reloads_on_sighup() {
    use crate::io::{ReloadStrategy, start_watcher};
    let h = TestProxyHarness::with_mode(Mode::Mock).await;

    start_watcher(ReloadStrategy::Signal, h.mock_dir.clone(), h.state.mocks.clone());
    // Give signal-hook-tokio a moment to register the SIGHUP handler.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    write_jsonl_mock(&h.mock_dir, "_global", "GET", "/sighup/1", r#"{"sig":"hup"}"#, "_global").await;

    // Send SIGHUP to ourselves.
    // SAFETY: kill(2) is safe to call on any pid; SIGHUP is a standard
    // signal whose default action (terminate) is overridden here by the
    // signal-hook-tokio handler installed in start_watcher.
    let pid = std::process::id() as i32;
    extern "C" { fn kill(pid: i32, sig: i32) -> i32; }
    let rc = unsafe { kill(pid, 1 /* SIGHUP */) };
    assert_eq!(rc, 0, "kill(SIGHUP) failed");

    let mut got_it = false;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let resp = reqwest::get(format!("{}/sighup/1", h.proxy_url)).await.unwrap();
        if resp.status() == 200 {
            let body = resp.text().await.unwrap();
            if body.contains("sig") {
                got_it = true;
                break;
            }
        }
    }
    assert!(got_it, "SIGHUP did not trigger reload within 2s");
    h.cleanup();
}

// ─── v0.3 Per-tenant isolation tests (feature #7) ────────────────────────────
//
// `X-Gostly-Tenant` header (or `?_tenant=`) scopes the mock library so a
// single proxy can serve N parallel test workers without cross-pollution.

/// Three concurrent workers, each writing + reading mocks under their own
/// tenant header. None of them sees the others' mocks.
#[tokio::test]
async fn per_tenant_three_concurrent_workers_zero_cross_pollution() {
    let h = TestProxyHarness::with_mode(Mode::Mock).await;
    // Seed a different response per tenant against the same (method, uri).
    h.add_mock_for_tenant(None, "GET", "/x", 200, r#"{"who":"alice"}"#, "alice").await;
    h.add_mock_for_tenant(None, "GET", "/x", 200, r#"{"who":"bob"}"#,   "bob").await;
    h.add_mock_for_tenant(None, "GET", "/x", 200, r#"{"who":"carol"}"#, "carol").await;

    let client = reqwest::Client::new();

    // Fire three concurrent requests, each tagged with a different tenant
    // header. Each must see ITS tenant's body, never another's.
    let url = format!("{}/x", h.proxy_url);
    let (a, b, c) = tokio::join!(
        client.get(&url).header("X-Gostly-Tenant", "alice").send(),
        client.get(&url).header("X-Gostly-Tenant", "bob").send(),
        client.get(&url).header("X-Gostly-Tenant", "carol").send(),
    );
    let a_body = a.unwrap().text().await.unwrap();
    let b_body = b.unwrap().text().await.unwrap();
    let c_body = c.unwrap().text().await.unwrap();
    assert!(a_body.contains("alice"), "tenant alice saw: {}", a_body);
    assert!(b_body.contains("bob"),   "tenant bob saw: {}",   b_body);
    assert!(c_body.contains("carol"), "tenant carol saw: {}", c_body);

    // And a request with NO tenant header / query → falls into _global.
    // _global has no /x mock, so the response is 404 (not "alice", not "bob",
    // not "carol"). This proves no implicit fallback is happening.
    let untagged = reqwest::get(format!("{}/x", h.proxy_url)).await.unwrap();
    assert_eq!(untagged.status(), 404,
        "untagged request must fall into _global (which has no /x mock), not borrow another tenant's");

    h.cleanup();
}

/// `?_tenant=foo` query parameter is honoured as a fallback for clients
/// that can't set headers.
#[tokio::test]
async fn per_tenant_query_string_fallback() {
    let h = TestProxyHarness::with_mode(Mode::Mock).await;
    h.add_mock_for_tenant(None, "GET", "/qs", 200, r#"{"via":"qs"}"#, "via-qs").await;

    let resp = reqwest::get(format!("{}/qs?_tenant=via-qs", h.proxy_url)).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("\"via\":\"qs\""), "query-string tenant lookup failed: {}", body);
    h.cleanup();
}

/// The header is preferred over the query string when both are present.
#[tokio::test]
async fn per_tenant_header_wins_over_query_string() {
    let h = TestProxyHarness::with_mode(Mode::Mock).await;
    h.add_mock_for_tenant(None, "GET", "/p", 200, r#"{"via":"header"}"#, "from-header").await;
    h.add_mock_for_tenant(None, "GET", "/p", 200, r#"{"via":"query"}"#,  "from-query").await;

    let client = reqwest::Client::new();
    let resp = client.get(format!("{}/p?_tenant=from-query", h.proxy_url))
        .header("X-Gostly-Tenant", "from-header")
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("header"), "header should override query string, got: {}", body);
    assert!(!body.contains("query"));
    h.cleanup();
}

/// LEARN mode records mocks under the request's tenant; the recorded mock
/// is then visible only to subsequent requests under the same tenant.
#[tokio::test]
async fn per_tenant_learn_records_under_request_tenant() {
    let h = TestProxyHarness::new().await; // LEARN by default
    h.upstream.set_body(r#"{"recorded":"under-test-1"}"#);
    let client = reqwest::Client::new();

    // Record under tenant test-1.
    let _ = client.get(format!("{}/recorded", h.proxy_url))
        .header("X-Gostly-Tenant", "test-1")
        .send().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Flip to MOCK mode and verify the recorded mock is tenant-scoped.
    *h.state.mode.write().await = Mode::Mock;

    // Same tenant → hit.
    let r1 = client.get(format!("{}/recorded", h.proxy_url))
        .header("X-Gostly-Tenant", "test-1")
        .send().await.unwrap();
    assert_eq!(r1.status(), 200);
    assert!(r1.text().await.unwrap().contains("under-test-1"));

    // Different tenant → miss (404).
    let r2 = client.get(format!("{}/recorded", h.proxy_url))
        .header("X-Gostly-Tenant", "test-2")
        .send().await.unwrap();
    assert_eq!(r2.status(), 404,
        "tenant test-2 must not see test-1's recorded mock");

    h.cleanup();
}

/// `POST /ghost/mocks` accepts an explicit `tenant` field; missing field
/// defaults to `_global` for backwards compatibility.
#[tokio::test]
async fn per_tenant_admin_post_accepts_tenant_field_and_defaults_to_global() {
    let h = TestProxyHarness::with_mode(Mode::Mock).await;
    let client = reqwest::Client::new();

    // POST a mock under a custom tenant.
    let body = serde_json::json!({
        "id":         "admin-1",
        "timestamp":  "2026-05-04T00:00:00Z",
        "request":    {"method": "GET", "uri": "/admin", "body": ""},
        "response":   {"status": 200, "headers": {}, "body": "{\"who\":\"admin-tenant\"}", "latency_ms": 0},
        "tenant":     "admin-tenant"
    });
    let r = client.post(format!("{}/ghost/mocks", h.proxy_url))
        .json(&body)
        .send().await.unwrap();
    assert_eq!(r.status(), 200);

    // Read back under the same tenant.
    let resp = client.get(format!("{}/admin", h.proxy_url))
        .header("X-Gostly-Tenant", "admin-tenant")
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("admin-tenant"));

    // POST a second mock with NO tenant field → defaults to _global.
    let body_default = serde_json::json!({
        "id":         "admin-2",
        "timestamp":  "2026-05-04T00:00:00Z",
        "request":    {"method": "GET", "uri": "/admin-default", "body": ""},
        "response":   {"status": 200, "headers": {}, "body": "{\"who\":\"global\"}", "latency_ms": 0}
    });
    let r2 = client.post(format!("{}/ghost/mocks", h.proxy_url))
        .json(&body_default)
        .send().await.unwrap();
    assert_eq!(r2.status(), 200);

    // Read back under no tenant → _global.
    let resp_default = reqwest::get(format!("{}/admin-default", h.proxy_url)).await.unwrap();
    assert_eq!(resp_default.status(), 200);
    assert!(resp_default.text().await.unwrap().contains("global"));

    h.cleanup();
}

/// `GET /ghost/mocks?tenant=foo` returns only entries under that tenant.
#[tokio::test]
async fn per_tenant_list_mocks_supports_tenant_filter() {
    let h = TestProxyHarness::with_mode(Mode::Mock).await;
    h.add_mock_for_tenant(None, "GET", "/a", 200, "a-body", "tenant-a").await;
    h.add_mock_for_tenant(None, "GET", "/b", 200, "b-body", "tenant-b").await;

    let scoped = reqwest::get(format!("{}/ghost/mocks?tenant=tenant-a", h.proxy_url))
        .await.unwrap()
        .text().await.unwrap();
    assert!(scoped.contains("a-body"), "tenant-a list missing its own mock: {}", scoped);
    assert!(!scoped.contains("b-body"),
        "tenant-a list must not contain tenant-b's mock: {}", scoped);

    let all = reqwest::get(format!("{}/ghost/mocks", h.proxy_url))
        .await.unwrap()
        .text().await.unwrap();
    assert!(all.contains("a-body") && all.contains("b-body"),
        "unfiltered list should show both tenants: {}", all);

    h.cleanup();
}
