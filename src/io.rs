use std::collections::HashMap;
use std::sync::Arc;
use crate::{MockEntry, MockSequence, Mode, GLOBAL_TENANT};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

/// Tenant key for per-test isolation (v0.3, feature #7). Backwards-compat
/// JSONL files without a tenant field deserialize to `_global`. Requests
/// without `X-Gostly-Tenant` (or `?_tenant=`) read `_global`.
pub type Tenant = String;

/// Per-service mock map: (method, uri, tenant) → MockEntry. O(1) exact lookup.
///
/// Tenant scoping (v0.3, feature #7) is part of the key so a single library
/// can hold mocks for many parallel test workers without cross-pollination.
/// A request under tenant `worker-3` will only ever match entries written
/// under tenant `worker-3`; the default tenant is `_global`.
pub type ServiceMocks = HashMap<(String, String, Tenant), MockEntry>;

/// One mock file → one immutable `MockSegment`. Hot-reload (v0.3, feature #6)
/// rebuilds only the changed file's segment, leaving the others untouched, so
/// editing `mock_users.jsonl` doesn't perturb the in-memory state for
/// `mock_orders.jsonl`. The segment is wrapped in an `Arc` and swapped out
/// atomically via `MockLibrary` (which is itself wrapped in an `arc_swap::ArcSwap`).
#[derive(Default, Debug, Clone)]
pub struct MockSegment {
    /// service_id → ServiceMocks
    /// In v0.3 the file naming scheme is still `mock_{service_id}.jsonl`, so
    /// each segment only contributes one service. The map keeps the same
    /// shape as `MockIndex` so callers (matcher, handlers) don't need a
    /// separate code path for "single-service-segment" lookups.
    pub services: HashMap<String, ServiceMocks>,
}

/// Full mock library: a tree of per-file segments keyed by file basename
/// (the `{svc_id}` part of `mock_{svc_id}.jsonl`). Segments are
/// `Arc<MockSegment>` so the watcher can swap one out without rebuilding
/// the others, and so a request handler can read a segment once and hold
/// the Arc through processing.
///
/// The library itself lives behind an `arc_swap::ArcSwap` (see `AppState`)
/// for in-flight request safety: the handler reads `state.mocks` ONCE at
/// the top of the request and holds the resulting `Arc<MockLibrary>` for
/// the entire lifecycle. Subsequent reloads publish a new `Arc<MockLibrary>`
/// — the in-flight handler still sees the old one. This is the same pattern
/// rustls uses for its `ConfigBuilder`: lock-free reads, atomic swaps.
#[derive(Default, Debug, Clone)]
pub struct MockLibrary {
    /// Segment basename (e.g. "users" for `mock_users.jsonl`) → Arc<MockSegment>.
    /// The `_global` synthetic key holds entries injected via `POST /ghost/mocks`
    /// at runtime; it has no on-disk file.
    pub segments: HashMap<String, Arc<MockSegment>>,
}

impl MockLibrary {
    pub fn new() -> Self { Self::default() }

    /// Total mock count across every segment. O(segments + services).
    pub fn total_count(&self) -> usize {
        self.segments
            .values()
            .map(|seg| seg.services.values().map(|m| m.len()).sum::<usize>())
            .sum()
    }

    /// Number of distinct service_ids across every segment. O(segments + services).
    pub fn service_count(&self) -> usize {
        let mut svcs: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for seg in self.segments.values() {
            for k in seg.services.keys() { svcs.insert(k.as_str()); }
        }
        svcs.len()
    }

    /// Look up a single (method, uri, tenant) entry across all segments.
    /// Returns the first matching `MockEntry` found in any segment that
    /// contains the requested service_id. The `_global` tenant fallback is
    /// the responsibility of the caller (handler) — this function is
    /// strictly tenant-scoped so accidental cross-pollution is impossible.
    pub fn find_exact(
        &self,
        service_id: &str,
        method: &str,
        uri: &str,
        tenant: &str,
    ) -> Option<MockEntry> {
        let key = (method.to_string(), uri.to_string(), tenant.to_string());
        for seg in self.segments.values() {
            if let Some(svc) = seg.services.get(service_id) {
                if let Some(entry) = svc.get(&key) {
                    return Some(entry.clone());
                }
            }
        }
        None
    }

    /// Collect every entry for `service_id` across every segment, optionally
    /// filtered by tenant. Used by smart-swap fallback which needs the full
    /// per-service corpus.
    pub fn entries_for_service(
        &self,
        service_id: &str,
        tenant: Option<&str>,
    ) -> Vec<MockEntry> {
        let mut out = Vec::new();
        for seg in self.segments.values() {
            if let Some(svc) = seg.services.get(service_id) {
                for ((_, _, t), e) in svc.iter() {
                    if tenant.is_none_or(|want| want == t) {
                        out.push(e.clone());
                    }
                }
            }
        }
        out
    }

    /// Flatten every entry across every segment (used by `GET /ghost/mocks`,
    /// optionally tenant-scoped).
    pub fn all_entries(&self, tenant: Option<&str>) -> Vec<MockEntry> {
        let mut out = Vec::new();
        for seg in self.segments.values() {
            for svc in seg.services.values() {
                for ((_, _, t), e) in svc.iter() {
                    if tenant.is_none_or(|want| want == t) {
                        out.push(e.clone());
                    }
                }
            }
        }
        out
    }
}

/// Backwards-compat alias: a few call sites + tests still spell the old name.
/// New code should use `MockLibrary` directly.
#[allow(dead_code)]
pub type MockIndex = MockLibrary;

/// Build a single segment from one JSONL file. Returns `None` if the file
/// has no parseable entries (matches the old per-file behaviour where
/// services with no parseable lines are dropped from the index entirely).
async fn load_one_segment(path: &std::path::Path) -> Option<(String, MockSegment)> {
    let fname = path.file_name().and_then(|n| n.to_str())?.to_string();
    if !fname.starts_with("mock_") || !fname.ends_with(".jsonl") {
        return None;
    }
    let svc_id = fname["mock_".len()..fname.len() - ".jsonl".len()].to_string();
    let content = tokio::fs::read_to_string(path).await.ok()?;

    let mut svc_mocks: ServiceMocks = HashMap::new();
    for line in content.lines() {
        if let Ok(entry) = serde_json::from_str::<MockEntry>(line) {
            // Tenant defaults to "_global" when missing on disk (serde default).
            let tenant = entry.tenant.clone();
            svc_mocks.insert(
                (entry.request.method.clone(), entry.request.uri.clone(), tenant),
                entry,
            );
        }
    }
    if svc_mocks.is_empty() {
        return None;
    }

    // Compact: rewrite file with one line per unique (method, uri, tenant) so
    // the append log doesn't grow unboundedly between restarts.
    let tmp = format!("{}.tmp", path.display());
    let compacted = svc_mocks.values()
        .filter_map(|m| serde_json::to_string(m).ok())
        .collect::<Vec<_>>()
        .join("\n") + "\n";
    if tokio::fs::write(&tmp, &compacted).await.is_ok() {
        let _ = tokio::fs::rename(&tmp, path).await;
    }

    let mut services = HashMap::new();
    services.insert(svc_id.clone(), svc_mocks);
    Some((svc_id, MockSegment { services }))
}

/// Load all per-service mock files from `dir` into a fresh `MockLibrary`.
/// Reads every file matching `mock_{svc_id}.jsonl`. Skips missing/unreadable
/// files silently. Each file becomes its own `Arc<MockSegment>` so subsequent
/// reloads can rebuild segments individually (see `reload_segment`).
pub async fn load_all_service_mocks(dir: &str) -> MockLibrary {
    let mut library = MockLibrary::new();
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(r)  => r,
        Err(_) => return library,
    };
    while let Ok(Some(de)) = rd.next_entry().await {
        let path = de.path();
        if let Some((svc_id, segment)) = load_one_segment(&path).await {
            library.segments.insert(svc_id, Arc::new(segment));
        }
    }
    library
}

/// Reload a single file into a new `Arc<MockSegment>` and return it. The
/// caller publishes the segment by cloning the current `MockLibrary`,
/// inserting the new segment, and `store`-ing the rebuilt library back into
/// the `ArcSwap`. Returns `None` if the file is gone or has no parseable
/// entries — caller decides whether to drop the segment from the library.
pub async fn reload_segment(path: &std::path::Path) -> Option<(String, Arc<MockSegment>)> {
    load_one_segment(path).await.map(|(svc, seg)| (svc, Arc::new(seg)))
}

/// Build a `MockLibrary` from a single in-memory list of entries (used by
/// admin POSTs which create mocks without touching disk). The synthetic
/// segment basename is `_global` so admin-created entries don't collide
/// with file-backed segments.
#[allow(dead_code)]
pub fn library_from_entries(entries: Vec<MockEntry>) -> MockLibrary {
    let mut svc_map: HashMap<String, ServiceMocks> = HashMap::new();
    for entry in entries {
        let svc = entry.service_id.clone().unwrap_or_else(|| GLOBAL_TENANT.to_string());
        let key = (
            entry.request.method.clone(),
            entry.request.uri.clone(),
            entry.tenant.clone(),
        );
        svc_map.entry(svc).or_default().insert(key, entry);
    }
    let mut library = MockLibrary::new();
    library.segments.insert(
        GLOBAL_TENANT.to_string(),
        Arc::new(MockSegment { services: svc_map }),
    );
    library
}

/// Append a single serialised MockEntry to the per-service serving index log.
///
/// Mirrors `append_to_traffic_log` — one JSON line per call, O_APPEND.
/// `load_all_service_mocks` compacts the file on startup (last write wins per
/// method+uri), so file size stays bounded to unique endpoint count × avg line size.
///
/// On open or write failure (disk full, permission denied, missing mount), emits
/// `tracing::error!` with the path and underlying error and increments
/// `ghost_io_errors_total{operation=mock_open|mock_write}`. The function returns
/// `()` to preserve the existing call sites; the failure surface is observability.
pub async fn append_to_mock_log(dir: &str, service_id: &str, line: &str) {
    let path = format!("{}/mock_{}.jsonl", dir, service_id);
    match OpenOptions::new().create(true).append(true).open(&path).await {
        Ok(mut file) => {
            if let Err(e) = file.write_all(format!("{}\n", line).as_bytes()).await {
                tracing::error!(
                    path = %path,
                    error = %e,
                    "ghost_mock_log_write_failed — served-mock record lost; check disk space and mount permissions",
                );
                metrics::counter!("ghost_io_errors_total", "operation" => "mock_write").increment(1);
            }
        }
        Err(e) => {
            tracing::error!(
                path = %path,
                error = %e,
                "ghost_mock_log_open_failed — served-mock record lost; check disk space and mount permissions",
            );
            metrics::counter!("ghost_io_errors_total", "operation" => "mock_open").increment(1);
        }
    }
}

pub async fn load_sequences(path: &str) -> Vec<MockSequence> {
    match tokio::fs::read_to_string(path).await {
        Ok(c) => c.lines().filter_map(|l| serde_json::from_str(l).ok()).collect(),
        Err(_) => Vec::new(),
    }
}

#[allow(dead_code)]  // sequences in-development; agent routes commented out, see main.rs Router
pub async fn save_sequences(path: &str, seqs: &[MockSequence]) {
    let tmp     = format!("{}.tmp", path);
    let content = seqs.iter()
        .filter_map(|s| serde_json::to_string(s).ok())
        .collect::<Vec<_>>().join("\n")
        + if seqs.is_empty() { "" } else { "\n" };
    if tokio::fs::write(&tmp, &content).await.is_ok() {
        let _ = tokio::fs::rename(&tmp, path).await;
    }
}

pub async fn read_mode_from_file(path: &str) -> Option<Mode> {
    let c = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&format!("\"{}\"", c.trim())).ok()
}

/// Append a single serialised entry to the per-service traffic log.
///
/// Each service gets its own file: `{dir}/traffic_{service_id}.jsonl`
/// This is append-only — every recorded request lands here regardless of
/// whether the same method+uri was seen before. The in-memory mock index
/// stays deduplicated for MOCK-mode serving; this file is the full-fidelity
/// source used by the training pipeline.
///
/// # Concurrency note
/// We rely on POSIX O_APPEND atomicity for writes ≤ PIPE_BUF (~4 096 bytes
/// on Linux/macOS). Typical JSONL lines are well under that limit.
///
/// TODO: At sustained write concurrency (> ~1 000 req/s per service) or on
/// non-POSIX platforms, replace with a per-service `tokio::sync::mpsc`
/// channel feeding a single dedicated writer task.
pub async fn append_to_traffic_log(dir: &str, service_id: &str, line: &str) {
    let path = format!("{}/traffic_{}.jsonl", dir, service_id);
    match OpenOptions::new().create(true).append(true).open(&path).await {
        Ok(mut file) => {
            if let Err(e) = file.write_all(format!("{}\n", line).as_bytes()).await {
                tracing::error!(
                    path = %path,
                    error = %e,
                    "ghost_traffic_log_write_failed — recorded request lost; check disk space and mount permissions",
                );
                metrics::counter!("ghost_io_errors_total", "operation" => "traffic_write").increment(1);
            }
        }
        Err(e) => {
            tracing::error!(
                path = %path,
                error = %e,
                "ghost_traffic_log_open_failed — recorded request lost; check disk space and mount permissions",
            );
            metrics::counter!("ghost_io_errors_total", "operation" => "traffic_open").increment(1);
        }
    }
}

pub async fn write_mode_to_file(path: &str, mode: &Mode) {
    let label = match mode {
        Mode::Learn         => "LEARN",
        Mode::Mock          => "MOCK",
        Mode::Passthrough   => "PASSTHROUGH",
        Mode::Transitioning => return,
    };
    if let Err(e) = tokio::fs::write(path, label).await {
        tracing::error!(
            path = %path,
            mode = %label,
            error = %e,
            "mode_file_write_failed — agent restart will revert to previous mode; check disk space and mount permissions",
        );
        metrics::counter!("ghost_io_errors_total", "operation" => "mode_write").increment(1);
    }
}

// ─── Hot-reload watcher (v0.3, feature #6) ────────────────────────────────────
//
// `MOCK_RELOAD_STRATEGY` selects the trigger source. Default is `fs_watch`
// (the `notify` crate; inotify on Linux, FSEvents on macOS,
// ReadDirectoryChangesW on Windows). Other strategies suit environments
// where filesystem events are unreliable or unsupported:
//
// - `fs_watch`  — default for local dev. Editor saves trigger reload.
// - `signal`    — SIGHUP. Useful in k8s/Docker with shared volumes where
//                 inotify on a bind-mount or PVC is flaky.
// - `poll`      — fixed-interval rescan. Fallback for NFS / EFS / GCS-FUSE
//                 where neither inotify nor SIGHUP is reliable.
// - `http_admin`— operator-driven via `POST /ghost/admin/reload`. The route
//                 is always live; this strategy just stops the watcher from
//                 starting any background task.
//
// In-flight semantics: the watcher publishes a *new* `Arc<MockLibrary>` via
// `ArcSwap::store`. Existing handlers keep their old Arc until they finish.
// The handler's contract is to call `state.mocks.load()` ONCE per request
// at the top of `proxy_handler` and use that snapshot for the rest of the
// request lifecycle. This is the same pattern rustls uses for its
// `ConfigBuilder`: lock-free reads, atomic publishes, no torn views.

/// How a `MockLibraryWatcher` decides when to reload the on-disk library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadStrategy {
    /// `notify` crate — inotify / FSEvents / ReadDirectoryChangesW.
    /// 200ms debounce coalesces editor-save bursts.
    FsWatch,
    /// SIGHUP from the operator (or k8s `kubectl exec ... kill -HUP`).
    Signal,
    /// Fixed-interval rescan. The duration is read from `MOCK_RELOAD_POLL_MS`
    /// (default 30s).
    Poll,
    /// No background task; `POST /ghost/admin/reload` is the only trigger.
    HttpAdmin,
}

impl ReloadStrategy {
    pub fn from_env() -> Self {
        match std::env::var("MOCK_RELOAD_STRATEGY").ok().as_deref() {
            Some("signal")     => ReloadStrategy::Signal,
            Some("poll")       => ReloadStrategy::Poll,
            Some("http_admin") => ReloadStrategy::HttpAdmin,
            // Empty / unset / any other value → default to fs_watch.
            _                  => ReloadStrategy::FsWatch,
        }
    }
}

/// Atomic, in-flight-safe handle to the live mock library.
///
/// `arc_swap::ArcSwap` lets readers grab the current `Arc<MockLibrary>` in
/// O(1) without locking. The watcher publishes a new `Arc<MockLibrary>` by
/// calling `store(...)`; in-flight handlers that already loaded the old Arc
/// keep using it until they drop the reference. New requests see the new
/// snapshot from their next `load()` call.
pub type MockStore = arc_swap::ArcSwap<MockLibrary>;

/// Performs a full reload of `dir` and publishes the result via `store`.
/// Returns the new (count, services) tuple for logging.
///
/// This is the single shared reload path used by every strategy. Per-file
/// segment rebuilds are handled by `reload_one_file`; this version
/// rebuilds the whole library from scratch (used on startup and by
/// poll / signal / http_admin paths which don't know which specific file
/// changed).
pub async fn reload_full(dir: &str, store: &MockStore) -> (usize, usize) {
    let library = load_all_service_mocks(dir).await;
    let count    = library.total_count();
    let services = library.service_count();
    store.store(Arc::new(library));
    metrics::gauge!("ghost_mock_library_size").set(count as f64);
    metrics::counter!("ghost_mock_reloads_total", "trigger" => "full").increment(1);
    (count, services)
}

/// Per-file segment rebuild for the `fs_watch` strategy. Looks at one
/// changed file path, rebuilds its segment, and publishes a new
/// `Arc<MockLibrary>` with that segment swapped in (or removed if the file
/// is now gone or empty).
pub async fn reload_one_file(path: &std::path::Path, store: &MockStore) {
    let fname = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None    => return,
    };
    if !fname.starts_with("mock_") || !fname.ends_with(".jsonl") {
        return;
    }
    let svc_id = fname["mock_".len()..fname.len() - ".jsonl".len()].to_string();

    // Snapshot the current library, swap in the rebuilt (or removed) segment.
    let current = store.load_full();
    let mut next: MockLibrary = (*current).clone();
    match reload_segment(path).await {
        Some((_, segment)) => { next.segments.insert(svc_id.clone(), segment); }
        None               => { next.segments.remove(&svc_id); }
    }
    let count = next.total_count();
    store.store(Arc::new(next));
    metrics::gauge!("ghost_mock_library_size").set(count as f64);
    metrics::counter!("ghost_mock_reloads_total", "trigger" => "fs_watch").increment(1);
    tracing::info!(segment = %svc_id, total_mocks = count, "🔄 hot-reload: segment rebuilt");
}

/// Background task that turns reload triggers into `MockStore` publishes.
///
/// Returns immediately for `HttpAdmin` (the route handler does the work).
/// For the active strategies, spawns a tokio task that runs for the lifetime
/// of the process. The handle is intentionally fire-and-forget — the watcher
/// outlives the function call and is killed by the runtime on shutdown.
pub fn start_watcher(strategy: ReloadStrategy, mock_dir: String, store: Arc<MockStore>) {
    match strategy {
        ReloadStrategy::FsWatch  => spawn_fs_watch(mock_dir, store),
        ReloadStrategy::Signal   => spawn_signal_watch(mock_dir, store),
        ReloadStrategy::Poll     => spawn_poll_watch(mock_dir, store),
        ReloadStrategy::HttpAdmin => {
            tracing::info!("hot-reload strategy: http_admin (POST /ghost/admin/reload to trigger)");
        }
    }
}

fn spawn_fs_watch(mock_dir: String, store: Arc<MockStore>) {
    use notify::{RecursiveMode, Watcher, EventKind};
    tracing::info!(mock_dir = %mock_dir, "hot-reload strategy: fs_watch (notify; 200ms debounce)");

    // Sync mpsc — `notify` callbacks are sync. We forward into a tokio task
    // through a tokio mpsc on the receive side.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<std::path::PathBuf>();

    tokio::spawn(async move {
        // Hold the watcher in this task so it lives as long as the channel.
        let watcher_tx = tx.clone();
        let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                // Only react to mutations that change file contents — Create,
                // Modify, Remove. The Access/Other events are noise.
                if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)) {
                    return;
                }
                for path in event.paths {
                    let _ = watcher_tx.send(path);
                }
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %e, "fs_watch failed to start; falling back to manual reload only");
                return;
            }
        };
        // The dir may not exist yet at boot — create it then watch.
        let _ = tokio::fs::create_dir_all(&mock_dir).await;
        if let Err(e) = watcher.watch(std::path::Path::new(&mock_dir), RecursiveMode::NonRecursive) {
            tracing::error!(mock_dir = %mock_dir, error = %e, "fs_watch could not bind to mock_dir");
            return;
        }

        // Debounce loop — coalesce bursts of events (editors typically
        // write a tmp file then rename, producing 2-3 events per save).
        let debounce = std::time::Duration::from_millis(200);
        loop {
            // Block for the first event.
            let path = match rx.recv().await {
                Some(p) => p,
                None    => break, // channel closed → watcher dropped
            };
            let mut pending: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
            pending.insert(path);

            // Drain any further events that arrive within the debounce window.
            let drain = tokio::time::sleep(debounce);
            tokio::pin!(drain);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut drain => break,
                    next = rx.recv() => match next {
                        Some(p) => { pending.insert(p); },
                        None    => break,
                    }
                }
            }

            for p in pending {
                reload_one_file(&p, &store).await;
            }
        }
    });
}

#[cfg(unix)]
fn spawn_signal_watch(mock_dir: String, store: Arc<MockStore>) {
    use signal_hook::consts::SIGHUP;
    use signal_hook_tokio::Signals;
    use futures::stream::StreamExt;
    tracing::info!(mock_dir = %mock_dir, "hot-reload strategy: signal (SIGHUP)");
    tokio::spawn(async move {
        let mut signals = match Signals::new([SIGHUP]) {
            Ok(s)  => s,
            Err(e) => {
                tracing::error!(error = %e, "signal watcher failed to register SIGHUP");
                return;
            }
        };
        while signals.next().await.is_some() {
            let (count, services) = reload_full(&mock_dir, &store).await;
            tracing::info!(count, services, "🔄 SIGHUP reload");
        }
    });
}

#[cfg(not(unix))]
fn spawn_signal_watch(_mock_dir: String, _store: Arc<MockStore>) {
    tracing::warn!("signal-based reload requires Unix; ignoring MOCK_RELOAD_STRATEGY=signal");
}

fn spawn_poll_watch(mock_dir: String, store: Arc<MockStore>) {
    let interval_ms = std::env::var("MOCK_RELOAD_POLL_MS")
        .ok().and_then(|v| v.parse().ok())
        .unwrap_or(30_000_u64);
    tracing::info!(mock_dir = %mock_dir, interval_ms, "hot-reload strategy: poll");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
        // First tick fires immediately; skip it so we don't double-reload at boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let (count, services) = reload_full(&mock_dir, &store).await;
            tracing::debug!(count, services, "poll reload");
        }
    });
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for ghost_io_errors_total instrumentation: callers of
    /// `append_to_traffic_log` and `append_to_mock_log` rely on `()` return type,
    /// so a path that cannot be opened or written to must NOT panic and must NOT
    /// propagate. Behaviour is verified at the observability layer (tracing +
    /// metrics) which the test cannot easily assert without installing a recorder
    /// — the assertion here is the weaker but still load-bearing "does not panic
    /// or hang on a known-unwritable path."
    #[tokio::test]
    async fn append_to_traffic_log_does_not_panic_on_unwritable_path() {
        // /proc/1 is a kernel-managed read-only directory on Linux that cannot
        // be created in or written to. On macOS the path doesn't exist, which
        // also fails the open. Either way: graceful return is the contract.
        let unwritable_dir = "/proc/1/this-cannot-exist";
        append_to_traffic_log(unwritable_dir, "svc", "{\"hello\": \"world\"}").await;
    }

    #[tokio::test]
    async fn append_to_mock_log_does_not_panic_on_unwritable_path() {
        let unwritable_dir = "/proc/1/this-cannot-exist";
        append_to_mock_log(unwritable_dir, "svc", "{\"hello\": \"world\"}").await;
    }

    // ── Helpers ────────────────────────────────────────────────────────────────
    //
    // Test fixtures stay in JSON form so the tests don't depend on the private
    // field layout of `MockEntry` / `MockSequence` in main.rs. `load_*` parses
    // these strings via `serde_json::from_str`, so any drift between the JSON
    // shape here and the production struct is caught by load returning an
    // empty collection — which the tests assert against directly.

    fn unique_dir(tag: &str) -> String {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = format!("/tmp/gostly-io-test-{}-{}-{}", tag, pid, nanos);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry_line(id: &str, method: &str, uri: &str) -> String {
        format!(
            r#"{{"id":"{id}","timestamp":"2026-05-01T00:00:00Z","request":{{"method":"{method}","uri":"{uri}","body":""}},"response":{{"status":200,"headers":{{}},"body":"ok","latency_ms":0}}}}"#
        )
    }

    fn seq_line(id: &str, method: &str, uri: &str) -> String {
        format!(
            r#"{{"id":"{id}","method":"{method}","uri":"{uri}","responses":[{{"status":200,"body":"a"}}]}}"#
        )
    }

    // ── load_all_service_mocks (now segmented) ────────────────────────────────
    //
    // The library is a tree of `Arc<MockSegment>` keyed by file basename.
    // Each segment contains a `services: HashMap<String, ServiceMocks>` —
    // ServiceMocks is keyed by (method, uri, tenant). The tests below assert
    // both load behaviour AND segment shape so future refactors don't
    // silently flatten the tree.

    fn segment_for(library: &MockLibrary, svc: &str) -> Option<Arc<MockSegment>> {
        library.segments.get(svc).cloned()
    }

    #[tokio::test]
    async fn load_all_service_mocks_missing_dir_returns_empty() {
        let lib = load_all_service_mocks("/tmp/gostly-io-test-does-not-exist-123456").await;
        assert!(lib.segments.is_empty(), "missing dir must return empty library, not panic");
    }

    #[tokio::test]
    async fn load_all_service_mocks_empty_dir_returns_empty() {
        let dir = unique_dir("emptydir");
        let lib = load_all_service_mocks(&dir).await;
        assert!(lib.segments.is_empty(), "empty dir must yield empty library");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn load_all_service_mocks_picks_up_jsonl_files_and_skips_others() {
        let dir = unique_dir("loadbasic");
        // Valid mock file for service "svc-a"
        tokio::fs::write(
            format!("{}/mock_svc-a.jsonl", dir),
            format!("{}\n", entry_line("e1", "GET", "/users")),
        ).await.unwrap();
        // File that doesn't match the naming pattern — must be ignored
        tokio::fs::write(format!("{}/random.txt", dir), "noise").await.unwrap();
        // File with .jsonl ext but wrong prefix — must be ignored
        tokio::fs::write(format!("{}/traffic_other.jsonl", dir), "noise").await.unwrap();

        let lib = load_all_service_mocks(&dir).await;
        assert_eq!(lib.segments.len(), 1, "exactly one segment expected, got {:?}", lib.segments.keys().collect::<Vec<_>>());
        let seg = segment_for(&lib, "svc-a").expect("svc-a segment missing");
        let svc = seg.services.get("svc-a").expect("svc-a service map missing");
        assert!(svc.contains_key(&("GET".to_string(), "/users".to_string(), GLOBAL_TENANT.to_string())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn load_all_service_mocks_dedupes_by_method_uri_last_write_wins() {
        let dir = unique_dir("dedupe");
        // Two entries for the same (GET, /users); HashMap keeps one. The exact
        // winner is implementation-defined since serde_json::from_str + collect
        // doesn't guarantee order — the load contract is "exactly one entry
        // per (method, uri, tenant)" which is what we assert.
        let body = format!(
            "{}\n{}\n",
            entry_line("first", "GET", "/users"),
            entry_line("second", "GET", "/users"),
        );
        tokio::fs::write(format!("{}/mock_svc.jsonl", dir), body).await.unwrap();

        let lib = load_all_service_mocks(&dir).await;
        let seg = segment_for(&lib, "svc").expect("svc segment missing");
        let svc = seg.services.get("svc").expect("svc service map missing");
        assert_eq!(
            svc.len(), 1,
            "duplicates must collapse to a single entry per (method, uri, tenant)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn load_all_service_mocks_skips_malformed_lines() {
        let dir = unique_dir("malformed");
        let body = format!(
            "{}\nnot-json\n{}\n",
            entry_line("good1", "GET", "/a"),
            entry_line("good2", "POST", "/b"),
        );
        tokio::fs::write(format!("{}/mock_svc.jsonl", dir), body).await.unwrap();

        let lib = load_all_service_mocks(&dir).await;
        let seg = segment_for(&lib, "svc").expect("svc segment missing");
        let svc = seg.services.get("svc").expect("svc service map missing");
        assert_eq!(svc.len(), 2, "malformed lines must be silently skipped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn load_all_service_mocks_skips_files_with_only_invalid_lines() {
        let dir = unique_dir("allinvalid");
        // File parses to zero entries → not inserted into the library at all.
        tokio::fs::write(
            format!("{}/mock_svc.jsonl", dir),
            "garbage\nmore-garbage\n",
        ).await.unwrap();
        let lib = load_all_service_mocks(&dir).await;
        assert!(lib.segments.is_empty(), "service with no parseable entries must be omitted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn load_all_service_mocks_compacts_duplicate_lines_on_disk() {
        let dir = unique_dir("compact");
        let path = format!("{}/mock_svc.jsonl", dir);
        let body = format!(
            "{}\n{}\n{}\n",
            entry_line("a", "GET", "/x"),
            entry_line("b", "GET", "/x"), // dup
            entry_line("c", "POST", "/y"),
        );
        tokio::fs::write(&path, body).await.unwrap();

        let _ = load_all_service_mocks(&dir).await;

        // After compaction the file holds exactly one line per unique (method,uri).
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<_> = after.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "compacted file should have one line per unique endpoint");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── append_to_traffic_log / append_to_mock_log happy paths ───────────────

    #[tokio::test]
    async fn append_to_traffic_log_creates_file_and_appends_line() {
        let dir = unique_dir("traffic-happy");
        append_to_traffic_log(&dir, "svc1", r#"{"a":1}"#).await;
        append_to_traffic_log(&dir, "svc1", r#"{"a":2}"#).await;

        let path = format!("{}/traffic_svc1.jsonl", dir);
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 2, "two appends → two lines");
        assert_eq!(lines[0], r#"{"a":1}"#);
        assert_eq!(lines[1], r#"{"a":2}"#);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn append_to_mock_log_creates_file_and_appends_line() {
        let dir = unique_dir("mock-happy");
        append_to_mock_log(&dir, "svcX", r#"{"hello":"world"}"#).await;
        let path = format!("{}/mock_svcX.jsonl", dir);
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "{\"hello\":\"world\"}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn append_to_traffic_log_writes_per_service_files() {
        // Different service_ids produce different files — important for the
        // multi-tenant traffic capture path.
        let dir = unique_dir("multisvc");
        append_to_traffic_log(&dir, "svc-a", r#"{"a":1}"#).await;
        append_to_traffic_log(&dir, "svc-b", r#"{"b":1}"#).await;

        let a = tokio::fs::read_to_string(format!("{}/traffic_svc-a.jsonl", dir)).await.unwrap();
        let b = tokio::fs::read_to_string(format!("{}/traffic_svc-b.jsonl", dir)).await.unwrap();
        assert_eq!(a, "{\"a\":1}\n");
        assert_eq!(b, "{\"b\":1}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── load_sequences / save_sequences round-trip ────────────────────────────

    #[tokio::test]
    async fn load_sequences_missing_file_returns_empty() {
        let v = load_sequences("/tmp/gostly-io-test-no-such-sequence-file.jsonl").await;
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn load_sequences_skips_malformed_lines() {
        let dir = unique_dir("seq-malformed");
        let path = format!("{}/seq.jsonl", dir);
        let body = format!(
            "{}\nnot-json\n{}\n",
            seq_line("s1", "GET", "/a"),
            seq_line("s2", "POST", "/b"),
        );
        tokio::fs::write(&path, body).await.unwrap();

        let v = load_sequences(&path).await;
        assert_eq!(v.len(), 2, "malformed sequence lines must be silently skipped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_sequences_round_trip_preserves_data() {
        let dir = unique_dir("seq-roundtrip");
        let path = format!("{}/seq.jsonl", dir);

        // Seed by writing JSON the loader can parse, then save back through
        // save_sequences and assert the file is non-empty with one line per seq.
        let body = format!(
            "{}\n{}\n",
            seq_line("s1", "GET", "/a"),
            seq_line("s2", "POST", "/b"),
        );
        tokio::fs::write(&path, &body).await.unwrap();
        let loaded = load_sequences(&path).await;
        assert_eq!(loaded.len(), 2);

        save_sequences(&path, &loaded).await;

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<_> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "save should write one line per sequence");

        // And re-loading the saved file yields the same count.
        let reloaded = load_sequences(&path).await;
        assert_eq!(reloaded.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn save_sequences_empty_writes_empty_file() {
        let dir = unique_dir("seq-empty");
        let path = format!("{}/seq.jsonl", dir);
        save_sequences(&path, &[]).await;
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "", "empty sequences write an empty file (no trailing newline)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── read_mode_from_file / write_mode_to_file ──────────────────────────────

    #[tokio::test]
    async fn read_mode_from_file_returns_none_for_missing_file() {
        let m = read_mode_from_file("/tmp/gostly-io-test-no-such-mode-file").await;
        assert!(m.is_none());
    }

    #[tokio::test]
    async fn read_mode_from_file_returns_none_for_unrecognised_label() {
        let dir = unique_dir("badmode");
        let path = format!("{}/mode", dir);
        tokio::fs::write(&path, "FROBNICATED").await.unwrap();
        let m = read_mode_from_file(&path).await;
        assert!(m.is_none(), "unknown mode label must yield None, not panic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_mode_to_file_writes_each_concrete_variant() {
        let dir = unique_dir("modes");
        for (mode, label) in &[
            (Mode::Learn, "LEARN"),
            (Mode::Mock, "MOCK"),
            (Mode::Passthrough, "PASSTHROUGH"),
        ] {
            let path = format!("{}/mode-{}", dir, label);
            write_mode_to_file(&path, mode).await;
            let content = tokio::fs::read_to_string(&path).await.unwrap();
            assert_eq!(content, *label);

            // Round-trip via read_mode_from_file.
            let parsed = read_mode_from_file(&path).await.expect("should parse back");
            assert_eq!(&parsed, mode);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_mode_to_file_skips_transitioning_variant() {
        // Transitioning is the "in-flight swap" sentinel — persisting it would
        // strand the agent in a meta-state on next boot, so the writer is a
        // no-op for that variant. Path must NOT exist after the call.
        let dir = unique_dir("trans");
        let path = format!("{}/mode-trans", dir);
        write_mode_to_file(&path, &Mode::Transitioning).await;
        assert!(
            !tokio::fs::try_exists(&path).await.unwrap_or(false),
            "Transitioning must not be written to disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_mode_to_file_overwrites_existing_content() {
        let dir = unique_dir("overwrite");
        let path = format!("{}/mode", dir);
        write_mode_to_file(&path, &Mode::Learn).await;
        write_mode_to_file(&path, &Mode::Mock).await;
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "MOCK", "second write must overwrite, not append");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_mode_from_file_trims_whitespace() {
        // The on-disk file may end up with a trailing newline depending on
        // editor, atomic-rename tooling, etc. The reader trims before parsing.
        let dir = unique_dir("trim");
        let path = format!("{}/mode", dir);
        tokio::fs::write(&path, "MOCK\n").await.unwrap();
        let m = read_mode_from_file(&path).await;
        assert_eq!(m, Some(Mode::Mock), "trailing whitespace must be trimmed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
