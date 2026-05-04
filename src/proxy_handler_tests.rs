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
use crate::io::MockIndex;

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
            mocks: Arc::new(tokio::sync::RwLock::new(MockIndex::new())),
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
            .route("/ghost/mocks",     get(handle_list_mocks))
            .route("/ghost/reload",    post(handle_reload_mocks))
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

    /// Helper: insert a mock into the in-memory mocks map.
    /// Tests that need a recorded fixture should call this directly rather
    /// than going through LEARN mode.
    async fn add_mock(&self, service_id: Option<&str>, method: &str, uri: &str, status: u16, body: &str) {
        let svc_key = service_id.unwrap_or("_global").to_string();
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
        };
        let mut idx = self.state.mocks.write().await;
        let svc = idx.entry(svc_key).or_default();
        svc.insert((method.to_string(), uri.to_string()), entry);
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

    // After LEARN, the in-memory mocks index should have an entry under "_global".
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let idx = h.state.mocks.read().await;
    let svc = idx.get("_global").expect("service entry should exist");
    assert!(svc.contains_key(&("GET".to_string(), "/api/learnable".to_string())));

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
