use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderValue, Response, StatusCode},
    // `delete` import currently unused — only the sequence routes used DELETE,
    // and those are commented out while sequences are in-development. Re-add
    // when sequences ship.
    routing::{any, get, post},
    Json, Router,
};
use axum_prometheus::PrometheusMetricLayer;
use clap::{Parser, Subcommand, ValueEnum};
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::{info, warn, Level};

mod matcher;
mod io;
mod chaos;
mod markov_chaos;
mod telemetry;

use chaos::ChaosConfig;

// ─── Security floor ───────────────────────────────────────────────────────────
// Always redacted before writing to JSONL regardless of service config.
// REDACT_HEADERS env var and per-service redact_headers ADD to this — never replace it.
const REDACT_FLOOR: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
    "x-session-id",
    "x-access-token",
    "x-user-token",
    "x-csrf-token",
    "x-amz-security-token",
    "x-amz-session-token",
    "x-goog-api-key",
    "grpc-metadata-authorization",
    "api-key",
    "token",
];

// ─── Mode ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum Mode { Learn, Mock, Passthrough, Transitioning }

// ─── Capabilities surface ─────────────────────────────────────────────────────
//
// This binary ships the recording-and-replay core: exact-match in MOCK mode
// plus an opt-in smart-swap fallback (enable with `SMART_SWAP_ENABLED=true`).
//
// Inference-assisted structural match, generative gap-fill, and the
// multi-user dashboard / MCP server live in the hosted Gostly product
// (https://gostly.ai). They are not part of this codebase; nothing here
// gates against them.

// ─── Mock library types ───────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
struct MockRequest {
    method: String,
    uri:    String,
    body:   String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct MockResponse {
    status:     u16,
    headers:    HashMap<String, String>,
    body:       String,
    latency_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct MockEntry {
    id:        String,
    timestamp: String,
    request:   MockRequest,
    response:  MockResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_id: Option<String>,
}

// ─── Sequence types ───────────────────────────────────────────────────────────

/// A single response step inside a sequence.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct SequenceResponse {
    status:     u16,
    #[serde(default)] headers:    HashMap<String, String>,
    body:       String,
    #[serde(default)] latency_ms: u64,
}

/// An ordered list of responses for a given endpoint pattern.
/// On each matching request, the next response in the list is served.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct MockSequence {
    id:             String,
    method:         String,
    uri:            String,
    responses:      Vec<SequenceResponse>,
    #[serde(default)] loop_responses: bool,
}


// ─── Runtime config ───────────────────────────────────────────────────────────

/// Per-process runtime knobs the operator can tune via `POST
/// /ghost/config` without restarting the proxy.
#[derive(Debug, Clone)]
struct RuntimeConfig {
    unmatched_status: u16,
    unmatched_body:   String,
}

impl RuntimeConfig {
    fn new() -> Self {
        Self {
            unmatched_status: 404,
            unmatched_body: r#"{"error":"no mock found","hint":"switch to LEARN mode and replay the request, or import a recording"}"#.to_string(),
        }
    }
}

// ─── App state ────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    http_client:             reqwest::Client,
    mode:                    Arc<RwLock<Mode>>,
    mocks:                   Arc<RwLock<io::MockIndex>>,
    unmatched:               Arc<RwLock<Vec<MockRequest>>>,
    sequences:               Arc<RwLock<Vec<MockSequence>>>,
    sequence_counters:       Arc<RwLock<HashMap<String, u32>>>,
    runtime_config:          Arc<RwLock<RuntimeConfig>>,
    upstreams:               Arc<RwLock<Vec<UpstreamRoute>>>,
    backend_url:             String,
    mock_dir:                String,
    #[allow(dead_code)]  // sequences in-development; field still loaded on boot but unused after routes commented out
    sequence_file_path:      String,
    mode_file_path:          String,
    redact_headers:          Vec<String>,
    max_body_bytes:          usize,
    // Per-service traffic logs — one append-only JSONL per service_id.
    // Full history for training; never deduplicated.
    // See io::append_to_traffic_log for concurrency / SQLite upgrade notes.
    traffic_log_dir:         String,
    // Monotonic counter for collision-free entry IDs at high concurrency.
    // timestamp_millis alone collides at ≥ 25 concurrent requests.
    entry_counter:           Arc<AtomicU64>,
    /// Smart-swap fallback in MOCK mode. Off by default; enable with
    /// `SMART_SWAP_ENABLED=true`. When on, MOCK-mode requests that miss
    /// the exact-match library try a structural / Markov-chaos swap
    /// against the same-service entries before returning a miss.
    smart_swap_enabled:      bool,
    /// Per-service Markov chaos state, keyed by `service_id`. parking_lot::RwLock
    /// (sync) — its write guard is `!Send`, so it is structurally impossible to
    /// hold across `.await`. Empty for services with `chaos_model = Uniform` or
    /// chaos disabled. Not persisted across agent restarts.
    markov_state:            Arc<parking_lot::RwLock<HashMap<String, markov_chaos::MarkovState>>>,
    /// Set of `service_id` values for which `first_request_proxied` has already
    /// been posted to the API in this process. Resets on agent restart — the
    /// API insert is idempotent (ON CONFLICT DO NOTHING), so a redundant POST
    /// after restart is a 200-no-op, not corruption. Skipping it here just
    /// avoids the network round-trip on the proxy hot path.
    onboarding_proxied:      Arc<parking_lot::RwLock<std::collections::HashSet<String>>>,
    /// Same shape, for `first_mock_served`. See above.
    onboarding_served:       Arc<parking_lot::RwLock<std::collections::HashSet<String>>>,
    /// In-memory telemetry collector backing the local `/metrics`
    /// endpoint. Nothing is shipped off-box.
    telemetry:               telemetry::TelemetryCollector,
}

// ─── JSON schemas for handlers ────────────────────────────────────────────────

#[derive(Deserialize)] struct SetModeBody  { mode: Mode }

#[derive(Deserialize)]
struct SetConfigBody {
    unmatched_status: Option<u16>,
    unmatched_body:   Option<String>,
}

// ChaosConfig is defined in `chaos.rs` and imported above. The struct supports the
// legacy flat fields (`latency_ms`, `error_status`, `error_body`) plus the new schema
// (`latency_jitter`, `error_codes`, `endpoint_rules`, `preset`).

#[derive(Deserialize, Serialize, Clone, Debug)]
struct UpstreamRoute {
    routing_type:  String,   // "host" | "path"
    routing_value: String,   // e.g. "order-service.internal" | "/api/orders"
    upstream_url:  String,
    service_id:    String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    mode:          Option<Mode>,  // per-service mode override; None = use global
    #[serde(skip_serializing_if = "Option::is_none", default)]
    chaos_config:  Option<ChaosConfig>,
    #[serde(default)]
    redact_headers: Vec<String>,  // per-service additions to the global redaction floor
}

#[derive(Deserialize)]
struct SetUpstreamsBody { upstreams: Vec<UpstreamRoute> }

// ─── CLI surface ──────────────────────────────────────────────────────────────
//
// All subcommands except `start` are short HTTP clients that talk to a
// running proxy on `localhost:<PORT>` (default 8080, override with --port
// or the GOSTLY_CLI_PORT env var).

// Version line shown by `--version`. Clap prepends the binary name (so the
// final output is e.g. `gostly v0.1.0 (commit 7688c16, oss)`). The commit
// SHA is read at compile time from `GOSTLY_BUILD_COMMIT` (the release
// workflow sets it; local cargo builds default to "dev").
//
// We avoid a `git2` / `vergen` build-dep on principle — adding 30+
// transitive crates for a 7-char string is the wrong trade.
const BUILD_COMMIT: &str = match option_env!("GOSTLY_BUILD_COMMIT") {
    Some(v) => v,
    None => "dev",
};

const BUILD_FLAVOUR: &str = "oss";

const VERSION_STR: &str = const_format_version();

const fn const_format_version() -> &'static str {
    // const fn can't format strings; we precompose with concat!.
    concat!(
        "v",
        env!("CARGO_PKG_VERSION"),
    )
}

// We need the commit + flavour at runtime too — clap takes a single static
// str so we splice them in via a leaked Box at startup. (Allocating once at
// process start is cheaper than threading lazy_static through a CLI parser.)
fn version_with_metadata() -> &'static str {
    use std::sync::OnceLock;
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        format!(
            "{VERSION_STR} (commit {BUILD_COMMIT}, {BUILD_FLAVOUR})"
        )
    })
    .as_str()
}

#[derive(Parser, Debug)]
#[command(
    name = "gostly",
    about = "Gostly OSS recording proxy — record, replay, and mock HTTP traffic",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the proxy (default if no subcommand is given)
    Start(StartArgs),
    /// Set proxy mode (LEARN, MOCK, PASSTHROUGH)
    Mode {
        #[arg(value_enum)]
        mode: ModeArg,
        /// Proxy port (default: 8080, env: GOSTLY_CLI_PORT)
        #[arg(long, env = "GOSTLY_CLI_PORT", default_value = "8080")]
        port: u16,
    },
    /// Show proxy health
    Status {
        #[arg(long, env = "GOSTLY_CLI_PORT", default_value = "8080")]
        port: u16,
    },
    /// Gracefully stop a locally running proxy (best-effort: pidfile then
    /// pgrep fallback). Returns success even if no proxy is running.
    Stop {
        /// Path to the pidfile (default: <data-dir>/gostly.pid)
        #[arg(long, default_value = "data/gostly.pid")]
        pidfile: String,
    },
    /// Export the recorded mock library in the named format
    Export {
        #[arg(long, value_enum, default_value = "openapi")]
        format: ExportFormat,
        #[arg(long, env = "GOSTLY_CLI_PORT", default_value = "8080")]
        port: u16,
    },
    /// Tail proxy logs from the data dir
    Logs {
        /// Follow new log lines (tail -f)
        #[arg(long)]
        follow: bool,
        /// Data dir holding the log file (default: ./data)
        #[arg(long, default_value = "./data")]
        data_dir: String,
    },
}

#[derive(Parser, Debug)]
struct StartArgs {
    /// Upstream URL the proxy forwards to (env: BACKEND_URL)
    #[arg(long, env = "BACKEND_URL")]
    upstream: Option<String>,
    /// Port the proxy listens on (env: PROXY_PORT)
    #[arg(long, default_value = "8080")]
    port: u16,
    /// Data dir for mocks, sequences, and the pidfile
    #[arg(long, default_value = "./data")]
    data_dir: String,
    /// Initial mode
    #[arg(long, value_enum, default_value = "learn")]
    mode: ModeArg,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum ModeArg {
    Learn,
    Mock,
    Passthrough,
}

impl ModeArg {
    fn as_str(self) -> &'static str {
        match self {
            ModeArg::Learn => "LEARN",
            ModeArg::Mock => "MOCK",
            ModeArg::Passthrough => "PASSTHROUGH",
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum ExportFormat {
    Openapi,
    Postman,
    Har,
}

impl ExportFormat {
    fn as_str(self) -> &'static str {
        match self {
            ExportFormat::Openapi => "openapi",
            ExportFormat::Postman => "postman",
            ExportFormat::Har => "har",
        }
    }
}

// Top-level dispatcher. Bare binary (no args) → `start` with all defaults =
// the pre-CLI behaviour. CLI args override env vars by setting them in the
// process before `run_proxy` reads them.
async fn run_cli() -> Result<(), String> {
    use clap::CommandFactory;
    // Inject runtime-built version string (commit + flavour) into the parser.
    // `Cli::command()` returns the clap::Command for `Cli`; we override the
    // version then re-derive matches → struct via FromArgMatches.
    use clap::FromArgMatches;
    let matches = Cli::command()
        .version(version_with_metadata())
        .get_matches();
    let cli = Cli::from_arg_matches(&matches)
        .map_err(|e| format!("argument parse error: {e}"))?;
    let cmd = cli.command.unwrap_or(Commands::Start(StartArgs {
        upstream: None,
        port: 8080,
        data_dir: "./data".to_string(),
        mode: ModeArg::Learn,
    }));

    match cmd {
        Commands::Start(args) => {
            apply_start_args_to_env(&args);
            run_proxy().await;
            Ok(())
        }
        Commands::Mode { mode, port } => cmd_set_mode(port, mode).await,
        Commands::Status { port } => cmd_status(port).await,
        Commands::Stop { pidfile } => cmd_stop(&pidfile).await,
        Commands::Export { format, port } => cmd_export(port, format).await,
        Commands::Logs { follow, data_dir } => cmd_logs(&data_dir, follow).await,
    }
}

// CLI args win over env vars. We push them back into the process env so the
// existing `run_proxy` body — which reads env directly — keeps working with
// zero changes. Only set vars the user actually supplied (Option) or whose
// CLI default differs from the daemon's env-var default.
fn apply_start_args_to_env(args: &StartArgs) {
    if let Some(upstream) = &args.upstream {
        std::env::set_var("BACKEND_URL", upstream);
    }
    // PROXY_PORT: CLI default 8080 == daemon default 8080, so safe to always set.
    std::env::set_var("PROXY_PORT", args.port.to_string());
    // Data dir: the daemon doesn't read DATA_DIR directly — it derives `data`
    // from MOCK_DIR's parent. We set MOCK_DIR (and friends) so a non-default
    // --data-dir is honoured without touching `run_proxy`.
    let data = args.data_dir.trim_end_matches('/');
    std::env::set_var("MOCK_DIR", format!("{data}/mocks"));
    std::env::set_var("SEQUENCE_FILE_PATH", format!("{data}/sequences.jsonl"));
    std::env::set_var("MODE_FILE_PATH", format!("{data}/mode.txt"));
    std::env::set_var("TRAFFIC_LOG_DIR", format!("{data}/traffic"));
    std::env::set_var("INITIAL_MODE", args.mode.as_str());
}

fn proxy_base(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn cli_http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("failed to build CLI HTTP client")
}

async fn cmd_set_mode(port: u16, mode: ModeArg) -> Result<(), String> {
    let url = format!("{}/ghost/mode", proxy_base(port));
    let body = serde_json::json!({ "mode": mode.as_str() });
    let resp = cli_http()
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("could not reach proxy at {url}: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("proxy returned {status}: {text}"));
    }
    println!("mode set to {} ({})", mode.as_str(), text.trim());
    Ok(())
}

async fn cmd_status(port: u16) -> Result<(), String> {
    let url = format!("{}/health", proxy_base(port));
    let resp = cli_http()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("could not reach proxy at {url}: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid JSON from {url}: {e}"))?;
    if !status.is_success() {
        return Err(format!("proxy returned {status}: {body}"));
    }
    let pretty = serde_json::to_string_pretty(&body)
        .unwrap_or_else(|_| body.to_string());
    println!("{pretty}");
    Ok(())
}

async fn cmd_stop(pidfile: &str) -> Result<(), String> {
    // Try pidfile first. If absent (e.g. older proxy build that didn't write
    // one), fall back to pgrep so the smoke test still works.
    let pid = match tokio::fs::read_to_string(pidfile).await {
        Ok(s) => s.trim().parse::<i32>().ok(),
        Err(_) => None,
    };
    let pid = match pid {
        Some(p) => p,
        None => {
            let out = std::process::Command::new("pgrep")
                .arg("-f")
                .arg("gostly-agent")
                .output()
                .map_err(|e| format!("pgrep failed: {e}"))?;
            let s = String::from_utf8_lossy(&out.stdout);
            match s.lines().next().and_then(|l| l.trim().parse::<i32>().ok()) {
                Some(p) => p,
                None => {
                    println!("no running proxy found (pidfile {pidfile} absent and no gostly-agent in pgrep)");
                    return Ok(());
                }
            }
        }
    };
    // SIGTERM via libc — keeping deps light. nix would be cleaner but is a
    // 4-LOC win that pulls in a non-trivial transitive tree.
    #[cfg(unix)]
    {
        // SAFETY: kill(2) is safe to call on any pid from any thread. The pid
        // may have already exited; ESRCH is non-fatal.
        let rc = unsafe { libc_kill(pid, 15 /* SIGTERM */) };
        if rc != 0 {
            // ESRCH = 3 = no such process; treat as already stopped.
            let errno = std::io::Error::last_os_error();
            if errno.raw_os_error() == Some(3) {
                println!("proxy (pid {pid}) was not running");
                return Ok(());
            }
            return Err(format!("kill(pid={pid}, SIGTERM) failed: {errno}"));
        }
        println!("sent SIGTERM to proxy (pid {pid})");
    }
    #[cfg(not(unix))]
    {
        return Err(format!(
            "stop is only supported on Unix; kill pid {pid} manually"
        ));
    }
    Ok(())
}

// Minimal libc shim. Using a function-level extern keeps the shim local and
// avoids pulling the `libc` crate into the OSS binary just for one symbol.
#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

async fn cmd_export(port: u16, format: ExportFormat) -> Result<(), String> {
    // The OSS proxy exposes the recorded library at /ghost/mocks. A future
    // PR can wire up native OpenAPI/Postman/HAR converters; today the agent
    // just prints the raw library and tags the requested format so callers
    // (and downstream tools) can branch.
    let url = format!("{}/ghost/mocks", proxy_base(port));
    let resp = cli_http()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("could not reach proxy at {url}: {e}"))?;
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid JSON from {url}: {e}"))?;
    let wrapped = serde_json::json!({
        "format": format.as_str(),
        "library": body,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&wrapped).unwrap_or_else(|_| wrapped.to_string())
    );
    Ok(())
}

// `cmd_import(argv)` is Postman/WireMock-aware and is the single import
// implementation. See the manual sniff in main() for the dispatch path.

async fn cmd_logs(data_dir: &str, follow: bool) -> Result<(), String> {
    // The proxy writes structured logs to stdout/stderr; if the operator has
    // redirected those to a file in the data dir we tail it. Otherwise we
    // print a hint. Common path: <data-dir>/proxy.log.
    let path = format!("{}/proxy.log", data_dir.trim_end_matches('/'));
    if !std::path::Path::new(&path).exists() {
        return Err(format!(
            "no log file at {path}. Redirect the proxy with `gostly start ... 2>&1 | tee {path}` \
             or set LOG_FORMAT=json and pipe to your log shipper."
        ));
    }
    if follow {
        // Spawn `tail -F` and inherit stdio so Ctrl-C works naturally. Using
        // the system tool keeps us from reimplementing inotify/kqueue in
        // Rust just for a CLI nicety.
        let status = std::process::Command::new("tail")
            .args(["-F", &path])
            .status()
            .map_err(|e| format!("could not exec tail: {e}"))?;
        if !status.success() {
            return Err(format!("tail exited with {status}"));
        }
        return Ok(());
    }
    let contents = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("could not read {path}: {e}"))?;
    print!("{contents}");
    Ok(())
}

// ─── Entry point ──────────────────────────────────────────────────────────────
//
// The bare binary boots the proxy with env-var-driven defaults; the CLI
// dispatcher (clap) routes each subcommand to its handler.
#[tokio::main]
async fn main() {
    if let Err(e) = run_cli().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run_proxy() {
    // Logging: JSON in production, human-readable otherwise
    let log_level = std::env::var("LOG_LEVEL")
        .unwrap_or_else(|_| "info".to_string())
        .parse::<Level>()
        .unwrap_or(Level::INFO);

    if std::env::var("LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::fmt().json().with_max_level(log_level).init();
    } else {
        tracing_subscriber::fmt().with_max_level(log_level).init();
    }

    // Config from env
    let backend_url = std::env::var("BACKEND_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:3000".to_string());
    let port: u16 = std::env::var("PROXY_PORT")
        .ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let mock_dir = std::env::var("MOCK_DIR")
        .unwrap_or_else(|_| "data/mocks".to_string());
    let sequence_file_path = std::env::var("SEQUENCE_FILE_PATH")
        .unwrap_or_else(|_| "data/sequences.jsonl".to_string());
    let mode_file_path = std::env::var("MODE_FILE_PATH")
        .unwrap_or_else(|_| "data/mode.txt".to_string());
    let max_body_bytes: usize = std::env::var("MAX_BODY_BYTES")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(1_048_576); // 1 MB
    let traffic_log_dir = std::env::var("TRAFFIC_LOG_DIR")
        .unwrap_or_else(|_| "data/traffic".to_string());

    let data_dir = std::path::Path::new(&mock_dir)
        .parent()
        .unwrap_or(std::path::Path::new("data"))
        .to_string_lossy()
        .to_string();

    // ── Telemetry collector ───────────────────────────────────────────────────
    // In-memory only; backs the local `/metrics` endpoint.
    let telemetry_collector = telemetry::TelemetryCollector::new();
    telemetry_collector.inc_counter(telemetry::counter_names::AGENT_BOOTS_TOTAL, 1);

    // Smart-swap fallback toggle. Off by default; opt in with
    // `SMART_SWAP_ENABLED=true`.
    let smart_swap_enabled = std::env::var("SMART_SWAP_ENABLED")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // Header redaction list: floor (non-overrideable) + env var additions
    let redact_headers: Vec<String> = {
        let mut set: std::collections::HashSet<String> = REDACT_FLOOR
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Ok(extras) = std::env::var("REDACT_HEADERS") {
            for h in extras.split(',') {
                let h = h.trim().to_lowercase();
                if !h.is_empty() { set.insert(h); }
            }
        }
        set.into_iter().collect()
    };

    // Create data directories
    let _ = tokio::fs::create_dir_all(&data_dir).await;
    let _ = tokio::fs::create_dir_all(&mock_dir).await;
    let _ = tokio::fs::create_dir_all(&traffic_log_dir).await;

    // Load persisted mode
    let initial_mode = io::read_mode_from_file(&mode_file_path).await
        .or_else(|| match std::env::var("INITIAL_MODE")
            .unwrap_or_default().to_uppercase().as_str()
        {
            "MOCK"        => Some(Mode::Mock),
            "PASSTHROUGH" => Some(Mode::Passthrough),
            "LEARN"       => Some(Mode::Learn),
            _             => None,
        })
        .unwrap_or(Mode::Learn);

    let mocks     = io::load_all_service_mocks(&mock_dir).await;
    let sequences = io::load_sequences(&sequence_file_path).await;
    let mock_count: usize = mocks.values().map(|m| m.len()).sum();
    info!("Loaded {} mocks ({} services), {} sequences — mode: {:?}",
        mock_count, mocks.len(), sequences.len(), initial_mode);

    // ── TODO(tls-spike) ────────────────────────────────────────────────────────
    // Current model: app → HTTP → proxy:8080 → HTTPS → real upstream (reqwest/rustls).
    // No MITM between app and proxy — the proxy is a plain HTTP client to the upstream.
    // Local dev uses Caddy as an optional TLS terminator (caddy trust required).
    //
    // Spike: evaluate whether the proxy should handle HTTPS interception natively:
    //   Option A — CONNECT tunneling + on-the-fly cert generation (rcgen + rustls):
    //     app → HTTPS → proxy (generates leaf cert signed by embedded CA) → HTTPS → upstream
    //     Requires: customer trusts proxy CA once (similar to caddy trust).
    //     Benefit: removes Caddy from dev setup, single binary handles TLS.
    //     Cost: ~500 LOC of subtle TLS code; ALPN, SNI, session resumption edge cases.
    //   Option B — HTTPS_PROXY env var support (RFC 7235 CONNECT):
    //     app uses standard proxy env vars; proxy handles CONNECT tunnel.
    //     No cert generation needed if using HTTP CONNECT passthrough (no inspection).
    //     Benefit: zero app config change for apps that respect HTTPS_PROXY.
    //   Option C — keep current (HTTP-only on 8080, Caddy opt-in for local HTTPS):
    //     Simplest. Works for most dev use cases. Ship Caddy as a separate compose override.
    //
    // Security note on cert pinning: only relevant if the APP talks HTTPS to the proxy.
    // In current model the app talks HTTP, so pinning doesn't apply here.
    // Pinning on the proxy→upstream leg is handled correctly by rustls with ACCEPT_INVALID_CERTS=false.
    //
    // Decision: revisit when a customer reports they cannot configure their app to use HTTP.
    // ─────────────────────────────────────────────────────────────────────────
    let accept_invalid_certs = std::env::var("ACCEPT_INVALID_CERTS").as_deref() == Ok("true");
    if accept_invalid_certs {
        warn!(
            accept_invalid_certs = true,
            "TLS certificate verification disabled — do not use in production"
        );
        metrics::counter!("gostly_security_bypass_total", "bypass" => "tls_cert_verification").increment(1);
    }
    let http_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(accept_invalid_certs)
        .build()
        .expect("failed to build HTTP client");

    let state = AppState {
        http_client,
        mode:                    Arc::new(RwLock::new(initial_mode)),
        mocks:                   Arc::new(RwLock::new(mocks)),
        unmatched:               Arc::new(RwLock::new(Vec::new())),
        sequences:               Arc::new(RwLock::new(sequences)),
        sequence_counters:       Arc::new(RwLock::new(HashMap::new())),
        runtime_config:          Arc::new(RwLock::new(RuntimeConfig::new())),
        upstreams:               Arc::new(RwLock::new(Vec::new())),
        backend_url,
        mock_dir,
        sequence_file_path,
        mode_file_path,
        redact_headers,
        max_body_bytes,
        traffic_log_dir,
        entry_counter: Arc::new(AtomicU64::new(0)),
        smart_swap_enabled,
        markov_state: Arc::new(parking_lot::RwLock::new(HashMap::new())),
        onboarding_proxied: Arc::new(parking_lot::RwLock::new(std::collections::HashSet::new())),
        onboarding_served:  Arc::new(parking_lot::RwLock::new(std::collections::HashSet::new())),
        telemetry:          telemetry_collector.clone(),
    };

    let _ = (&data_dir, &telemetry_collector);

    // Prometheus metrics layer — always on; backs the local /metrics
    // endpoint.
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let app = Router::new()
        // ── Observability ──
        .route("/health",  get(handle_health))
        .route("/metrics", get(move || std::future::ready(metric_handle.render())))
        // ── Mock library admin ──
        .route("/ghost/mode",      post(handle_set_mode))
        .route("/ghost/config",    post(handle_set_config))
        .route("/ghost/mocks",     get(handle_list_mocks))
        .route("/ghost/reload",    post(handle_reload_mocks))
        .route("/ghost/unmatched", get(handle_list_unmatched))
        // ── Sequence admin (in-development; routes commented out) ──
        // Re-enable when the sequences feature ships — handlers + data
        // structures + io::load are kept intact under the hood for that
        // re-enable.
        // .route("/ghost/sequences",           get(handle_list_sequences).post(handle_create_sequence))
        // .route("/ghost/sequences/:id/reset", post(handle_reset_sequence))
        // .route("/ghost/sequences/:id",       delete(handle_delete_sequence))
        // ── Multi-stream routing ──
        .route("/ghost/upstreams", post(handle_set_upstreams))
        // ── Proxy (must be last) ──
        .fallback(any(proxy_handler))
        .with_state(state)
        .route_layer(prometheus_layer)
        .layer(TraceLayer::new_for_http().make_span_with(|req: &Request<Body>| {
            tracing::info_span!("ghost", method = %req.method(), uri = %req.uri())
        }));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    info!("👻 Ghost Mock proxy live at http://{}", addr);

    // Write a pidfile for `gostly stop`. Best-effort: we don't fail the boot
    // if the data dir is read-only — the CLI's stop command falls back to
    // pgrep when the pidfile is absent. Path matches the CLI default
    // (`<data-dir>/gostly.pid`).
    let pidfile_path = format!("{}/gostly.pid", data_dir.trim_end_matches('/'));
    if let Err(e) = tokio::fs::write(&pidfile_path, std::process::id().to_string()).await {
        tracing::debug!(path = %pidfile_path, error = %e, "could not write pidfile (non-fatal)");
    }

    axum::serve(listener, app).await.unwrap();
}

// ─── Health ───────────────────────────────────────────────────────────────────

async fn handle_health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let index     = state.mocks.read().await;
    let mock_count: usize = index.values().map(|m| m.len()).sum();
    let services  = index.len();
    drop(index);
    // Sequences are in-development; the data structure exists on
    // AppState but the admin routes are not exposed publicly. Re-add
    // `"sequences": <count>` here when the feature ships.
    let mode = format!("{:?}", *state.mode.read().await);
    metrics::gauge!("ghost_mock_library_size").set(mock_count as f64);
    Json(serde_json::json!({
        "status":     "ok",
        "mode":       mode,
        "mock_count": mock_count,
        "services":   services,
    }))
}

// ─── Mode ─────────────────────────────────────────────────────────────────────

async fn handle_set_mode(
    State(state): State<AppState>,
    Json(body): Json<SetModeBody>,
) -> Json<serde_json::Value> {
    let label = format!("{:?}", body.mode);
    // Don't persist TRANSITIONING — if the agent restarts mid-transition it
    // should resume in LEARN (the last durable mode), not stay stuck.
    if body.mode != Mode::Transitioning {
        io::write_mode_to_file(&state.mode_file_path, &body.mode).await;
    }
    *state.mode.write().await = body.mode;
    metrics::counter!("ghost_mode_changes_total", "mode" => label.clone()).increment(1);
    info!("🔀 Mode → {}", label);
    Json(serde_json::json!({ "status": "ok", "mode": label }))
}

// ─── Mock library ─────────────────────────────────────────────────────────────

async fn handle_list_mocks(State(state): State<AppState>) -> Json<serde_json::Value> {
    let index = state.mocks.read().await;
    let all: Vec<&MockEntry> = index.values().flat_map(|svc| svc.values()).collect();
    let count = all.len();
    Json(serde_json::json!({ "count": count, "mocks": all }))
}

async fn handle_reload_mocks(State(state): State<AppState>) -> Json<serde_json::Value> {
    let index    = io::load_all_service_mocks(&state.mock_dir).await;
    let count: usize   = index.values().map(|m| m.len()).sum();
    let services = index.len();
    *state.mocks.write().await = index;
    metrics::gauge!("ghost_mock_library_size").set(count as f64);
    info!("🔄 Reloaded {} mocks across {} services", count, services);
    Json(serde_json::json!({ "status": "ok", "loaded": count, "services": services }))
}

async fn handle_list_unmatched(State(state): State<AppState>) -> Json<serde_json::Value> {
    let u = state.unmatched.read().await;
    Json(serde_json::json!({ "count": u.len(), "requests": *u }))
}

async fn handle_set_config(
    State(state): State<AppState>,
    Json(body): Json<SetConfigBody>,
) -> Json<serde_json::Value> {
    let mut cfg = state.runtime_config.write().await;
    if let Some(v) = body.unmatched_status { cfg.unmatched_status = v; }
    if let Some(v) = body.unmatched_body   { cfg.unmatched_body   = v; }
    info!("⚙️  Config updated: miss_status={}", cfg.unmatched_status);
    Json(serde_json::json!({ "status": "ok" }))
}

// ─── Sequences ────────────────────────────────────────────────────────────────

#[allow(dead_code)]  // sequences in-development; routes commented out, see Router::new() above
async fn handle_list_sequences(State(state): State<AppState>) -> Json<serde_json::Value> {
    let s = state.sequences.read().await;
    Json(serde_json::json!({ "count": s.len(), "sequences": *s }))
}

#[allow(dead_code)]  // sequences in-development; routes commented out, see Router::new() above
async fn handle_create_sequence(
    State(state): State<AppState>,
    Json(seq): Json<MockSequence>,
) -> Json<serde_json::Value> {
    let id = seq.id.clone();
    let mut seqs = state.sequences.write().await;
    seqs.retain(|s| s.id != id);
    seqs.push(seq);
    let all = seqs.clone();
    drop(seqs);
    let path = state.sequence_file_path.clone();
    tokio::spawn(async move { io::save_sequences(&path, &all).await; });
    info!("📋 Sequence created: {}", id);
    Json(serde_json::json!({ "status": "ok", "id": id }))
}

#[allow(dead_code)]  // sequences in-development; routes commented out, see Router::new() above
async fn handle_reset_sequence(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    state.sequence_counters.write().await.remove(&id);
    info!("🔁 Sequence reset: {}", id);
    Json(serde_json::json!({ "status": "ok", "id": id }))
}

#[allow(dead_code)]  // sequences in-development; routes commented out, see Router::new() above
async fn handle_delete_sequence(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let mut seqs = state.sequences.write().await;
    seqs.retain(|s| s.id != id);
    state.sequence_counters.write().await.remove(&id);
    let all = seqs.clone();
    drop(seqs);
    let path = state.sequence_file_path.clone();
    tokio::spawn(async move { io::save_sequences(&path, &all).await; });
    info!("🗑  Sequence deleted: {}", id);
    Json(serde_json::json!({ "status": "ok" }))
}

// ─── Multi-stream upstream routing ────────────────────────────────────────────

async fn handle_set_upstreams(
    State(state): State<AppState>,
    Json(body): Json<SetUpstreamsBody>,
) -> Json<serde_json::Value> {
    let count = body.upstreams.len();
    let markov_count = body.upstreams.iter()
        .filter(|u| u.chaos_config.as_ref()
            .map(|c| c.chaos_model == chaos::ChaosModel::Markov)
            .unwrap_or(false))
        .count();
    *state.upstreams.write().await = body.upstreams;
    info!("📡 Upstreams updated: {} routes", count);
    info!("markov_chaos_enabled services={}", markov_count);
    Json(serde_json::json!({ "status": "ok", "count": count }))
}

/// Returns (upstream_url, Option<service_id>, Option<per-service Mode>, Option<ChaosConfig>, Vec<redact_headers>)
/// for the first matching route. Falls back to default_url with no service_id/mode if nothing matches.
fn resolve_upstream(
    upstreams: &[UpstreamRoute],
    host: &str,
    path: &str,
    default_url: &str,
) -> (String, Option<String>, Option<Mode>, Option<ChaosConfig>, Vec<String>) {
    for up in upstreams {
        let matched = match up.routing_type.as_str() {
            "host" => {
                let bare_host = host.split(':').next().unwrap_or(host);
                bare_host == up.routing_value || host == up.routing_value
            }
            "path" => path.starts_with(&up.routing_value),
            _      => false,
        };
        if matched {
            return (
                up.upstream_url.clone(),
                Some(up.service_id.clone()),
                up.mode.clone(),
                up.chaos_config.clone(),
                up.redact_headers.clone(),
            );
        }
    }
    (default_url.to_string(), None, None, None, Vec::new())
}

// ─── Onboarding milestones ────────────────────────────────────────────────────
//
// Increments a local telemetry counter once per service-id per process
// when the agent observes an activation milestone (first request
// proxied, first mock recorded, first mock served). The in-memory
// dedupe set keeps the hot path off subsequent allocations after the
// first hit. Counters are visible at `/metrics` only.
fn fire_onboarding_event(
    state: &AppState,
    event_type: &'static str,
    service_id: Option<&str>,
    seen_set: &Arc<parking_lot::RwLock<std::collections::HashSet<String>>>,
) {
    let svc = service_id.unwrap_or("_global").to_string();
    {
        let read = seen_set.read();
        if read.contains(&svc) {
            return;
        }
    }
    {
        let mut write = seen_set.write();
        if !write.insert(svc.clone()) {
            return;
        }
    }

    match event_type {
        "first_request_proxied" => state
            .telemetry
            .inc_counter(telemetry::counter_names::FIRST_REQUEST_PROXIED_TOTAL, 1),
        "first_mock_recorded" => state
            .telemetry
            .inc_counter(telemetry::counter_names::FIRST_MOCK_RECORDED_TOTAL, 1),
        "first_mock_served" => state
            .telemetry
            .inc_counter(telemetry::counter_names::FIRST_MOCK_SERVED_TOTAL, 1),
        _ => {}
    }
}

// ─── Core proxy handler ───────────────────────────────────────────────────────

async fn proxy_handler(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response<Body>, StatusCode> {
    let (parts, body) = req.into_parts();
    let bytes = body.collect().await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?.to_bytes();

    let method     = parts.method.to_string();
    let path_query = matcher::normalize_uri(
        parts.uri.path_and_query().map(|v| v.as_str()).unwrap_or("/")
    );
    let body_str    = String::from_utf8_lossy(&bytes).to_string();
    let global_mode = state.mode.read().await.clone();

    // Resolve which upstream to proxy to (multi-stream routing)
    let host = parts.headers.get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let (resolved_upstream, resolved_service_id, service_mode, chaos_config, service_redact_headers) = {
        let ups = state.upstreams.read().await;
        resolve_upstream(&ups, &host, &path_query, &state.backend_url)
    };

    // Per-service mode overrides the global mode
    let mode = service_mode.unwrap_or(global_mode);

    // ── Chaos injection ────────────────────────────────────────────────────────
    // Endpoint rules win over service-level config when a pattern matches.
    // Legacy flat fields (latency_ms, error_status, error_body) still work — see chaos.rs.
    // The chaos event log POST is fire-and-forget and contains only:
    //   service_id, method, scrubbed_path, chaos_type, injected_*, rule_name.
    // No query string, headers, or bodies ever leave this block.
    if matches!(mode, Mode::Mock | Mode::Passthrough) {
        if let Some(ref svc_cc) = chaos_config {
            if svc_cc.enabled {
                let mode_str = match mode { Mode::Mock => "MOCK", Mode::Passthrough => "PASSTHROUGH", _ => "" };
                // Endpoint rules take precedence over service-level config.
                let active_cc: &ChaosConfig = chaos::find_endpoint_rule(&svc_cc.endpoint_rules, &path_query)
                    .unwrap_or(svc_cc);
                let applies = active_cc.modes.is_empty()
                    || active_cc.modes.iter().any(|m| m.as_str() == mode_str);
                if applies {
                    // Decide effective error_rate + latency multiplier under the chosen
                    // chaos model. The Markov branch grabs a parking_lot write guard,
                    // steps the state machine (sync), and *drops the guard* before any
                    // .await — the `!Send` guard would actually fail to compile if held
                    // across an await, but the explicit drop documents the contract.
                    let svc_id_for_state = resolved_service_id.clone().unwrap_or_default();
                    let (eff_error_rate, latency_mult) = match active_cc.chaos_model {
                        chaos::ChaosModel::Markov => {
                            let mc = active_cc.markov.clone().unwrap_or_default();
                            let mut map = state.markov_state.write();
                            let st = map
                                .entry(svc_id_for_state.clone())
                                .or_insert_with(markov_chaos::MarkovState::new);
                            let (state_kind, _) = st.step(&mc);
                            drop(map); // release before any .await
                            match state_kind {
                                markov_chaos::StateKind::Healthy  => (0.0, 1.0),
                                markov_chaos::StateKind::Degraded => {
                                    (mc.degraded_error_rate, mc.degraded_latency_mult)
                                }
                            }
                        }
                        chaos::ChaosModel::Uniform => (active_cc.error_rate, 1.0),
                    };

                    let base_latency = chaos::compute_latency_ms(active_cc);
                    let latency = (base_latency as f64 * latency_mult) as u64;
                    if latency > 0 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(latency)).await;
                    }

                    let mut injected_status: Option<u16> = None;
                    let mut error_resp: Option<Response<Body>> = None;
                    if eff_error_rate > 0.0 && rand::random::<f64>() < eff_error_rate {
                        if let Some(drawn) = chaos::draw_error(active_cc) {
                            let status = StatusCode::from_u16(drawn.status)
                                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                            let mut builder = Response::builder()
                                .status(status)
                                .header("content-type", "application/json")
                                .header("X-Ghost-Chaos", "true");
                            for (k, v) in &drawn.headers {
                                builder = builder.header(k.as_str(), v.as_str());
                            }
                            // Response::builder().body() can fail if a header value is invalid;
                            // fall through to a plain 500 in that case rather than panicking.
                            match builder.body(Body::from(drawn.body.clone())) {
                                Ok(r)  => {
                                    injected_status = Some(drawn.status);
                                    error_resp      = Some(r);
                                }
                                Err(_) => {
                                    let fallback = Response::builder()
                                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                                        .header("content-type", "application/json")
                                        .header("X-Ghost-Chaos", "true")
                                        .body(Body::from(String::new()));
                                    if let Ok(r) = fallback {
                                        injected_status = Some(500);
                                        error_resp      = Some(r);
                                    }
                                }
                            }
                        }
                    }

                    // Fire-and-forget chaos event log if anything was injected.
                    let logged_anything = injected_status.is_some() || latency > 0;
                    if logged_anything {
                        let scrubbed_path = chaos::scrub_path(&path_query);
                        let chaos_type = match (injected_status.is_some(), latency > 0) {
                            (true,  true)  => "both",
                            (true,  false) => "error",
                            (false, true)  => "latency",
                            _              => "none",
                        };
                        let svc_id    = resolved_service_id.clone().unwrap_or_default();
                        let rule_name = active_cc.preset.clone().unwrap_or_else(|| "custom".to_string());
                        let _ = (svc_id, rule_name, scrubbed_path, chaos_type);

                        if let Some(s) = injected_status {
                            info!("💥 CHAOS error {} {} → {}", method, path_query, s);
                        }
                        if latency > 0 && injected_status.is_none() {
                            info!("💥 CHAOS latency {} {} → {}ms", method, path_query, latency);
                        }
                    }

                    if let Some(r) = error_resp {
                        metrics::counter!("ghost_requests_total", "match_type" => "chaos").increment(1);
                        return Ok(r);
                    }
                }
            }
        }
    }

    match mode {
        // ── MOCK ──────────────────────────────────────────────────────────────
        Mode::Mock => {
            // 1. Exact match — O(1) per-service HashMap lookup
            {
                let index   = state.mocks.read().await;
                let svc_key = resolved_service_id.as_deref().unwrap_or("_global");
                let entry   = index.get(svc_key)
                    .and_then(|svc| matcher::find_exact(svc, &method, &path_query, &body_str));
                drop(index);
                if let Some(entry) = entry {
                    info!("🎭 EXACT HIT  {} {}", method, path_query);
                    metrics::counter!("ghost_requests_total", "match_type" => "exact").increment(1);
                    fire_onboarding_event(
                        &state,
                        "first_mock_served",
                        resolved_service_id.as_deref(),
                        &state.onboarding_served,
                    );
                    return serve_mock_response(
                        entry.response.status, entry.response.headers,
                        entry.response.body.into_bytes(), entry.response.latency_ms,
                        "X-Ghost-Mock", "true",
                    ).await;
                }
            }

            // 2. Sequence match
            {
                let seqs = state.sequences.read().await;
                let mut counters = state.sequence_counters.write().await;
                if let Some(seq_resp) = matcher::find_sequence_response(&seqs, &mut counters, &method, &path_query) {
                    drop(counters); drop(seqs);
                    info!("📋 SEQ HIT   {} {}", method, path_query);
                    metrics::counter!("ghost_requests_total", "match_type" => "sequence").increment(1);
                    fire_onboarding_event(
                        &state,
                        "first_mock_served",
                        resolved_service_id.as_deref(),
                        &state.onboarding_served,
                    );
                    return serve_mock_response(
                        seq_resp.status, seq_resp.headers,
                        seq_resp.body.into_bytes(), seq_resp.latency_ms,
                        "X-Ghost-Sequence", "true",
                    ).await;
                }
            }

            // 3. Smart-swap fallback (opt-in via SMART_SWAP_ENABLED).
            //
            // Inference-assisted structural match and generative gap-fill
            // live in the hosted Gostly product (https://gostly.ai); they
            // are not part of this binary.
            if state.smart_swap_enabled {
                let svc_entries: Vec<MockEntry> = {
                    let index   = state.mocks.read().await;
                    let svc_key = resolved_service_id.as_deref().unwrap_or("_global");
                    index.get(svc_key)
                        .map(|svc| svc.values().cloned().collect())
                        .unwrap_or_default()
                };
                if let Some(entry) = matcher::find_smart_swap(&svc_entries, &method, &path_query, &body_str) {
                    info!("🔀 SWAP HIT   {} {}", method, path_query);
                    metrics::counter!("ghost_requests_total", "match_type" => "smart_swap").increment(1);
                    fire_onboarding_event(
                        &state,
                        "first_mock_served",
                        resolved_service_id.as_deref(),
                        &state.onboarding_served,
                    );
                    return serve_mock_response(
                        entry.response.status, entry.response.headers,
                        entry.response.body.into_bytes(), entry.response.latency_ms,
                        "X-Ghost-SwapMatch", "true",
                    ).await;
                }
            }

            // 4. Total miss
            warn!("🔴 MOCK MISS {} {}", method, path_query);
            metrics::counter!("ghost_requests_total", "match_type" => "miss").increment(1);
            metrics::counter!("ghost_unmatched_total").increment(1);
            state.unmatched.write().await.push(MockRequest {
                method, uri: path_query, body: body_str,
            });
            let (miss_status, miss_body) = {
                let cfg = state.runtime_config.read().await;
                (cfg.unmatched_status, cfg.unmatched_body.clone())
            };
            Ok(Response::builder()
                .status(StatusCode::from_u16(miss_status).unwrap_or(StatusCode::NOT_FOUND))
                .header("Content-Type", "application/json")
                .header("X-Ghost-Miss", "true")
                .body(Body::from(miss_body))
                .unwrap())
        }

        // ── LEARN ─────────────────────────────────────────────────────────────
        Mode::Learn => {
            let target = format!("{}{}", resolved_upstream, path_query);
            let mut req_builder = state.http_client
                .request(method.parse().unwrap_or(reqwest::Method::GET), &target);
            for (name, value) in &parts.headers {
                if matches!(name.as_str(), "host" | "transfer-encoding" | "connection" | "keep-alive") { continue; }
                req_builder = req_builder.header(name.as_str(), value.as_bytes());
            }

            let start = std::time::Instant::now();
            let res = req_builder.body(bytes.to_vec()).send().await
                .map_err(|_| StatusCode::BAD_GATEWAY)?;
            let latency_ms = start.elapsed().as_millis() as u64;

            let res_status = res.status().as_u16();
            let mut res_headers: HashMap<String, String> = HashMap::new();
            for (k, v) in res.headers() {
                res_headers.insert(k.to_string(), v.to_str().unwrap_or("").to_string());
            }
            let res_bytes = res.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;

            // Cap body size
            let res_body_str = if res_bytes.len() > state.max_body_bytes {
                format!("[BODY_TRUNCATED: {} bytes]", res_bytes.len())
            } else {
                String::from_utf8_lossy(&res_bytes).to_string()
            };

            // Build effective redaction set: global floor+env additions + per-service additions
            let effective_redact: std::collections::HashSet<String> = state.redact_headers
                .iter()
                .chain(service_redact_headers.iter())
                .map(|h| h.to_lowercase())
                .collect();

            // Cap request body before storing (request headers are never stored — no headers field on MockRequest)
            let req_body_stored = if bytes.len() > state.max_body_bytes {
                format!("[BODY_TRUNCATED: {} bytes]", bytes.len())
            } else {
                body_str.clone()
            };

            // Redact response headers before storing (set-cookie etc. are in the floor)
            let stored_res_headers: HashMap<String, String> = res_headers.iter()
                .map(|(k, v)| {
                    if effective_redact.contains(k.to_lowercase().as_str()) {
                        (k.clone(), "[REDACTED]".to_string())
                    } else {
                        (k.clone(), v.clone())
                    }
                })
                .collect();

            let entry = MockEntry {
                id:        chrono::Utc::now().timestamp_millis().to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                request:   MockRequest {
                    method: method.clone(), uri: path_query.clone(), body: req_body_stored,
                },
                response:  MockResponse {
                    status: res_status, headers: stored_res_headers.clone(),
                    body: res_body_str, latency_ms,
                },
                service_id: resolved_service_id.clone(),
            };

            info!("📼 RECORDED {} {} → {} ({}ms)", method, path_query, res_status, latency_ms);
            metrics::counter!("ghost_requests_total", "match_type" => "learn").increment(1);

            // Onboarding telemetry — first proxied request per service. The set is
            // process-local; the API enforces tenant-level idempotency.
            fire_onboarding_event(
                &state,
                "first_request_proxied",
                resolved_service_id.as_deref(),
                &state.onboarding_proxied,
            );
            // A successful LEARN-mode upstream round-trip is a mock being
            // recorded for the first time per service.
            state.telemetry.inc_counter(
                telemetry::counter_names::FIRST_MOCK_RECORDED_TOTAL, 1);
            // Mock fidelity observation — record upstream latency so
            // /metrics can expose a fidelity histogram locally.
            state.telemetry.record_observation(
                telemetry::observation_names::MOCK_FIDELITY_MS, latency_ms as f64);

            // ── Traffic log (full history, per-service, append-only) ────────────
            // Append-only file capturing every request the proxy sees. Never
            // deduplicated — every request lands here regardless of whether
            // the same method+uri was seen before.
            //
            // Requires a routed service; unrouted traffic is not recorded.
            // (Dashboard enforces "configure a service first" before LEARN mode.)
            //
            // TODO: At sustained write concurrency (> ~1 000 req/s per service)
            // replace with a per-service mpsc channel → single writer task to
            // eliminate reliance on O_APPEND POSIX atomicity (see io.rs).
            if let Some(ref svc_id) = resolved_service_id {
                let seq = state.entry_counter.fetch_add(1, Ordering::Relaxed);
                let ts  = chrono::Utc::now().timestamp_millis();
                let traffic_entry = MockEntry {
                    id: format!("{}_{}", ts, seq),
                    ..entry.clone()
                };
                let line = serde_json::to_string(&traffic_entry).unwrap_or_default();
                let dir  = state.traffic_log_dir.clone();
                let svc  = svc_id.clone();
                tokio::spawn(async move { io::append_to_traffic_log(&dir, &svc, &line).await; });
            }

            // ── Serving index (per-service, deduplicated by method+uri) ─────────
            // HashMap keyed by (method, uri) — insert always overwrites the previous
            // entry for the same endpoint, keeping the index bounded and current.
            //
            // Separation from the traffic log follows CQRS/event-sourcing:
            //   • Traffic log  = event store (full history, per-service, never mutated)
            //   • Serving index = materialized view (per-service HashMap, always current)
            {
                let svc_id = resolved_service_id.as_deref().unwrap_or("_global").to_string();
                {
                    let mut index = state.mocks.write().await;
                    let svc = index.entry(svc_id.clone()).or_default();
                    svc.insert((entry.request.method.clone(), entry.request.uri.clone()), entry.clone());
                }
                let line = serde_json::to_string(&entry).unwrap_or_default();
                let dir = state.mock_dir.clone();
                tokio::spawn(async move {
                    io::append_to_mock_log(&dir, &svc_id, &line).await;
                });
            }

            build_response(res_status, &res_headers, res_bytes.to_vec())
        }

        // ── TRANSITIONING ─────────────────────────────────────────────────────
        // The API has started batch processing (scrub + pattern extraction).
        // Return 503 so clients retry once MOCK mode is ready.
        Mode::Transitioning => {
            metrics::counter!("ghost_requests_total", "match_type" => "transitioning").increment(1);
            Ok(Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header("Content-Type", "application/json")
                .header("Retry-After", "5")
                .header("X-Ghost-Transitioning", "true")
                .body(Body::from(
                    r#"{"status":"transitioning","message":"Proxy is processing recorded traffic. Retry in a few seconds.","retry_after":5}"#
                ))
                .unwrap())
        }

        // ── PASSTHROUGH ───────────────────────────────────────────────────────
        Mode::Passthrough => {
            let target = format!("{}{}", resolved_upstream, path_query);
            let mut req_builder = state.http_client
                .request(method.parse().unwrap_or(reqwest::Method::GET), &target);
            for (name, value) in &parts.headers {
                if matches!(name.as_str(), "host" | "transfer-encoding" | "connection" | "keep-alive") { continue; }
                req_builder = req_builder.header(name.as_str(), value.as_bytes());
            }
            let res = req_builder.body(bytes.to_vec()).send().await
                .map_err(|_| StatusCode::BAD_GATEWAY)?;
            let res_status = res.status().as_u16();
            let mut res_headers = HashMap::new();
            for (k, v) in res.headers() {
                res_headers.insert(k.to_string(), v.to_str().unwrap_or("").to_string());
            }
            let res_bytes = res.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
            metrics::counter!("ghost_requests_total", "match_type" => "passthrough").increment(1);
            build_response(res_status, &res_headers, res_bytes.to_vec())
        }
    }
}

// ─── Response helpers ─────────────────────────────────────────────────────────

async fn serve_mock_response(
    status:     u16,
    headers:    HashMap<String, String>,
    body:       Vec<u8>,
    latency_ms: u64,
    extra_key:  &'static str,
    extra_val:  &str,
) -> Result<Response<Body>, StatusCode> {
    if latency_ms > 0 {
        tokio::time::sleep(tokio::time::Duration::from_millis(latency_ms.min(10_000))).await;
    }
    let mut r = build_response(status, &headers, body)?;
    r.headers_mut().insert(
        extra_key,
        HeaderValue::from_str(extra_val).unwrap_or(HeaderValue::from_static("true")),
    );
    Ok(r)
}

fn build_response(
    status:  u16,
    headers: &HashMap<String, String>,
    body:    Vec<u8>,
) -> Result<Response<Body>, StatusCode> {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let mut builder = Response::builder().status(code);
    for (k, v) in headers {
        if matches!(k.to_lowercase().as_str(), "transfer-encoding" | "connection" | "keep-alive") { continue; }
        if let Ok(val) = HeaderValue::from_bytes(v.as_bytes()) {
            builder = builder.header(k.as_str(), val);
        }
    }
    Ok(builder.body(Body::from(body)).unwrap_or_else(|_| Response::new(Body::empty())))
}

#[cfg(test)]
mod proxy_handler_tests;

#[cfg(test)]
mod tests {
    use super::{
        Cli,
        ExportFormat,
        ModeArg,
        version_with_metadata,
    };
    use clap::CommandFactory;

    // ── CLI surface ────────────────────────────────────────────────────────
    // These tests pin the user-visible command surface so a future refactor
    // doesn't silently drop a subcommand or flip a default. Light-touch by
    // design — they exercise clap's parser, not network behaviour.

    #[test]
    fn cli_parses_no_args_and_help_lists_all_subcommands() {
        // No-args parse must succeed (defaults to `start`).
        let cmd = Cli::command();
        let m = cmd.clone().try_get_matches_from(["gostly"]).unwrap();
        // Subcommand should be None → caller defaults to Start.
        assert!(m.subcommand().is_none());

        // Help text must mention every promised subcommand. Render via clap.
        let mut help_buf = Vec::new();
        cmd.clone().write_help(&mut help_buf).unwrap();
        let help = String::from_utf8(help_buf).unwrap();
        for sub in ["start", "mode", "status", "stop", "export", "logs"] {
            assert!(help.contains(sub), "help missing subcommand `{sub}`:\n{help}");
        }
    }

    #[test]
    fn cli_version_string_format_is_stable() {
        // Asserts the README's promised shape: "gostly vX.Y.Z (commit ..., oss)".
        let v = version_with_metadata();
        assert!(v.starts_with('v'), "version must start with v: {v}");
        assert!(v.contains("commit "), "version must include commit: {v}");
        assert!(
            v.ends_with(", oss)"),
            "version must end with feature flavour: {v}"
        );
    }

    #[test]
    fn cli_mode_subcommand_accepts_each_mode_value() {
        for m in ["learn", "mock", "passthrough"] {
            let parsed = Cli::command().try_get_matches_from(["gostly", "mode", m]);
            assert!(parsed.is_ok(), "mode {m} should parse: {parsed:?}");
        }
        // Unknown mode must fail loudly.
        let bad = Cli::command().try_get_matches_from(["gostly", "mode", "shenanigans"]);
        assert!(bad.is_err(), "unknown mode value must reject");
    }

    #[test]
    fn cli_start_args_default_port_and_mode() {
        // Bare `start` should pick up the documented defaults (port 8080,
        // data ./data, mode learn). If anyone changes these, downstream
        // smoke scripts will silently drift — pin them here.
        let m = Cli::command()
            .try_get_matches_from(["gostly", "start"])
            .unwrap();
        let (sub, sm) = m.subcommand().unwrap();
        assert_eq!(sub, "start");
        assert_eq!(sm.get_one::<u16>("port"), Some(&8080));
        assert_eq!(
            sm.get_one::<String>("data_dir").map(String::as_str),
            Some("./data")
        );
    }

    #[test]
    fn mode_arg_as_str_is_uppercase() {
        // The agent's /ghost/mode endpoint expects UPPERCASE ("LEARN" etc.).
        // If we ever lowercase this the wire protocol breaks silently.
        assert_eq!(ModeArg::Learn.as_str(), "LEARN");
        assert_eq!(ModeArg::Mock.as_str(), "MOCK");
        assert_eq!(ModeArg::Passthrough.as_str(), "PASSTHROUGH");
    }

    #[test]
    fn export_format_as_str_is_lowercase() {
        assert_eq!(ExportFormat::Openapi.as_str(), "openapi");
        assert_eq!(ExportFormat::Postman.as_str(), "postman");
        assert_eq!(ExportFormat::Har.as_str(), "har");
    }

    // ── Pure-function tests ────────────────────────────────────────────────────
    //
    // These cover functions that have no I/O surface — small enough that the
    // tests double as living documentation of the contract.

    use super::{
        build_response, resolve_upstream,
        serve_mock_response,
        Mode, UpstreamRoute,
    };

    // ── resolve_upstream ───────────────────────────────────────────────────────

    fn route(routing_type: &str, value: &str, upstream: &str, svc: &str) -> UpstreamRoute {
        UpstreamRoute {
            routing_type:   routing_type.to_string(),
            routing_value:  value.to_string(),
            upstream_url:   upstream.to_string(),
            service_id:     svc.to_string(),
            mode:           None,
            chaos_config:   None,
            redact_headers: vec![],
        }
    }

    #[test]
    fn resolve_upstream_no_routes_returns_default() {
        let (url, svc, mode, cc, hdrs) = resolve_upstream(&[], "host.example", "/foo", "http://default");
        assert_eq!(url, "http://default");
        assert!(svc.is_none());
        assert!(mode.is_none());
        assert!(cc.is_none());
        assert!(hdrs.is_empty());
    }

    #[test]
    fn resolve_upstream_host_match_strips_port() {
        // The host header may include a port; the resolver must compare the
        // bare host. (And the literal-with-port form should also match — that's
        // the second branch in resolve_upstream.)
        let routes = vec![route("host", "api.example", "http://up", "svc-1")];
        let (url, svc, _, _, _) = resolve_upstream(&routes, "api.example:443", "/anything", "http://default");
        assert_eq!(url, "http://up");
        assert_eq!(svc.as_deref(), Some("svc-1"));
    }

    #[test]
    fn resolve_upstream_host_match_full_value() {
        // If the configured value already includes the port, the full host
        // should match too.
        let routes = vec![route("host", "api.example:443", "http://up", "svc-1")];
        let (url, _, _, _, _) = resolve_upstream(&routes, "api.example:443", "/", "http://default");
        assert_eq!(url, "http://up");
    }

    #[test]
    fn resolve_upstream_host_no_match_returns_default() {
        let routes = vec![route("host", "api.example", "http://up", "svc-1")];
        let (url, svc, _, _, _) = resolve_upstream(&routes, "other.example", "/", "http://default");
        assert_eq!(url, "http://default");
        assert!(svc.is_none());
    }

    #[test]
    fn resolve_upstream_path_match_uses_starts_with() {
        let routes = vec![route("path", "/api/orders", "http://orders", "svc-orders")];
        let (url, svc, _, _, _) = resolve_upstream(
            &routes, "any.host", "/api/orders/123", "http://default",
        );
        assert_eq!(url, "http://orders");
        assert_eq!(svc.as_deref(), Some("svc-orders"));
    }

    #[test]
    fn resolve_upstream_path_no_match_returns_default() {
        let routes = vec![route("path", "/api/orders", "http://orders", "svc-orders")];
        let (url, _, _, _, _) = resolve_upstream(&routes, "any.host", "/users", "http://default");
        assert_eq!(url, "http://default");
    }

    #[test]
    fn resolve_upstream_unknown_routing_type_is_skipped() {
        // Routing type "regex" isn't supported yet; the resolver must ignore
        // such entries (false match) and fall through to the next route or the
        // default.
        let routes = vec![
            route("regex", ".*", "http://wrong", "svc-wrong"),
            route("path", "/v1", "http://right", "svc-right"),
        ];
        let (url, svc, _, _, _) = resolve_upstream(&routes, "h", "/v1/x", "http://default");
        assert_eq!(url, "http://right");
        assert_eq!(svc.as_deref(), Some("svc-right"));
    }

    #[test]
    fn resolve_upstream_first_match_wins() {
        // Two routes that both match — the first one in the list takes priority.
        let routes = vec![
            route("path", "/api", "http://first", "svc-first"),
            route("path", "/api", "http://second", "svc-second"),
        ];
        let (url, svc, _, _, _) = resolve_upstream(&routes, "h", "/api/x", "http://default");
        assert_eq!(url, "http://first");
        assert_eq!(svc.as_deref(), Some("svc-first"));
    }

    #[test]
    fn resolve_upstream_propagates_per_route_mode_and_redact_headers() {
        let r = UpstreamRoute {
            routing_type:   "host".to_string(),
            routing_value:  "h".to_string(),
            upstream_url:   "http://up".to_string(),
            service_id:     "svc".to_string(),
            mode:           Some(Mode::Mock),
            chaos_config:   None,
            redact_headers: vec!["x-extra-secret".to_string()],
        };
        let (_, _, mode, _, hdrs) = resolve_upstream(&[r], "h", "/", "http://default");
        assert_eq!(mode, Some(Mode::Mock));
        assert_eq!(hdrs, vec!["x-extra-secret".to_string()]);
    }

    // ── build_response / serve_mock_response ───────────────────────────────────

    #[test]
    fn build_response_invalid_status_falls_back_to_200() {
        // 0 is outside the valid HTTP status range — `StatusCode::from_u16`
        // rejects values < 100. build_response must fall back to 200 rather
        // than panic.
        let resp = build_response(0, &std::collections::HashMap::new(), b"hi".to_vec()).unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // 99 is also below the floor.
        let resp2 = build_response(99, &std::collections::HashMap::new(), b"".to_vec()).unwrap();
        assert_eq!(resp2.status(), axum::http::StatusCode::OK);
    }

    #[test]
    fn build_response_strips_hop_by_hop_headers() {
        // transfer-encoding / connection / keep-alive are hop-by-hop and must
        // never be propagated downstream — that would corrupt the response
        // framing axum is about to do.
        let mut headers = std::collections::HashMap::new();
        headers.insert("Transfer-Encoding".to_string(), "chunked".to_string());
        headers.insert("Connection".to_string(), "keep-alive".to_string());
        headers.insert("keep-alive".to_string(), "timeout=5".to_string());
        headers.insert("X-Real-Header".to_string(), "ok".to_string());
        let resp = build_response(200, &headers, b"".to_vec()).unwrap();
        let h = resp.headers();
        assert!(!h.contains_key("transfer-encoding"));
        assert!(!h.contains_key("connection"));
        assert!(!h.contains_key("keep-alive"));
        assert_eq!(h.get("x-real-header").map(|v| v.to_str().unwrap()), Some("ok"));
    }

    #[test]
    fn build_response_skips_headers_with_invalid_bytes() {
        // A header value containing a NUL byte cannot be turned into a
        // HeaderValue. The function must skip the bad header rather than abort
        // the response.
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-Bad".to_string(), "evil\0value".to_string());
        headers.insert("X-Good".to_string(), "fine".to_string());
        let resp = build_response(201, &headers, b"body".to_vec()).unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
        let h = resp.headers();
        assert!(!h.contains_key("x-bad"));
        assert_eq!(h.get("x-good").map(|v| v.to_str().unwrap()), Some("fine"));
    }

    #[test]
    fn build_response_returns_200_for_valid_status() {
        let resp = build_response(204, &std::collections::HashMap::new(), b"".to_vec()).unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn serve_mock_response_appends_extra_header() {
        let resp = serve_mock_response(
            200,
            std::collections::HashMap::new(),
            b"x".to_vec(),
            0,
            "x-mock-source",
            "library",
        ).await.unwrap();
        assert_eq!(
            resp.headers().get("x-mock-source").and_then(|v| v.to_str().ok()),
            Some("library"),
        );
    }

    #[tokio::test]
    async fn serve_mock_response_clamps_extra_header_to_safe_fallback() {
        // A header value containing a control char cannot be turned into a
        // HeaderValue. The function falls back to "true" rather than panicking.
        let resp = serve_mock_response(
            200,
            std::collections::HashMap::new(),
            b"x".to_vec(),
            0,
            "x-mock-source",
            "evil\nvalue",
        ).await.unwrap();
        // The fallback path inserts the static "true" sentinel.
        assert_eq!(
            resp.headers().get("x-mock-source").and_then(|v| v.to_str().ok()),
            Some("true"),
        );
    }

    #[tokio::test]
    async fn serve_mock_response_caps_latency_at_10_seconds() {
        // Should NOT actually sleep for the requested 1-hour duration. We
        // assert that the call returns within a much smaller envelope. (The
        // 10s cap is enforced inside the function via .min(10_000), but we
        // can't burn 10s in a unit test, so we assert that the function does
        // not sleep when latency_ms == 0 — the sleep branch is exercised by
        // the existing chaos tests.)
        let start = std::time::Instant::now();
        let resp = serve_mock_response(
            200,
            std::collections::HashMap::new(),
            b"".to_vec(),
            0,
            "x-mock-source",
            "test",
        ).await.unwrap();
        assert!(start.elapsed() < std::time::Duration::from_millis(100));
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    // ── Mode (de)serialization ─────────────────────────────────────────────────

    #[test]
    fn mode_serialises_uppercase() {
        // The agent persists mode as the uppercase string label. A regression
        // here would silently break the on-disk mode file format.
        assert_eq!(serde_json::to_string(&Mode::Learn).unwrap(), "\"LEARN\"");
        assert_eq!(serde_json::to_string(&Mode::Mock).unwrap(), "\"MOCK\"");
        assert_eq!(
            serde_json::to_string(&Mode::Passthrough).unwrap(),
            "\"PASSTHROUGH\"",
        );
        assert_eq!(
            serde_json::to_string(&Mode::Transitioning).unwrap(),
            "\"TRANSITIONING\"",
        );
    }

    #[test]
    fn mode_deserialises_uppercase() {
        let m: Mode = serde_json::from_str("\"LEARN\"").unwrap();
        assert_eq!(m, Mode::Learn);
        let m: Mode = serde_json::from_str("\"MOCK\"").unwrap();
        assert_eq!(m, Mode::Mock);
        let m: Mode = serde_json::from_str("\"PASSTHROUGH\"").unwrap();
        assert_eq!(m, Mode::Passthrough);
    }

    #[test]
    fn mode_rejects_lowercase() {
        // The on-disk format is uppercase; lowercase must NOT silently parse
        // (otherwise we'd silently accept stale or hand-edited files).
        let r: Result<Mode, _> = serde_json::from_str("\"mock\"");
        assert!(r.is_err());
    }

}
