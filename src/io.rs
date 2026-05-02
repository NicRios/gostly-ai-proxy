use std::collections::HashMap;
use crate::{MockEntry, MockSequence, Mode};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

/// Per-service mock map: (method, uri) → MockEntry. O(1) exact lookup.
pub type ServiceMocks = HashMap<(String, String), MockEntry>;
/// Full mock index keyed by service_id → ServiceMocks.
/// The sentinel service_id "_global" holds manually created mocks with no service assignment.
pub type MockIndex = HashMap<String, ServiceMocks>;

/// Load all per-service mock files from `dir`.
/// Reads every file matching `mock_{svc_id}.jsonl`. Skips missing/unreadable files silently.
pub async fn load_all_service_mocks(dir: &str) -> MockIndex {
    let mut index = MockIndex::new();
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(r)  => r,
        Err(_) => return index,
    };
    while let Ok(Some(de)) = rd.next_entry().await {
        let path = de.path();
        let fname = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None    => continue,
        };
        if !fname.starts_with("mock_") || !fname.ends_with(".jsonl") {
            continue;
        }
        let svc_id = fname["mock_".len()..fname.len() - ".jsonl".len()].to_string();
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c)  => c,
            Err(_) => continue,
        };
        // Last line wins per (method, uri) — HashMap collect naturally deduplicates.
        let svc_mocks: ServiceMocks = content
            .lines()
            .filter_map(|l| serde_json::from_str::<MockEntry>(l).ok())
            .map(|e| ((e.request.method.clone(), e.request.uri.clone()), e))
            .collect();
        if svc_mocks.is_empty() {
            continue;
        }
        // Compact: rewrite file with one line per unique endpoint so the
        // append log doesn't grow unboundedly between restarts.
        let tmp = format!("{}.tmp", path.display());
        let compacted = svc_mocks.values()
            .filter_map(|m| serde_json::to_string(m).ok())
            .collect::<Vec<_>>()
            .join("\n") + "\n";
        if tokio::fs::write(&tmp, &compacted).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &path).await;
        }
        index.insert(svc_id, svc_mocks);
    }
    index
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

    // ── load_all_service_mocks ────────────────────────────────────────────────

    #[tokio::test]
    async fn load_all_service_mocks_missing_dir_returns_empty() {
        let idx = load_all_service_mocks("/tmp/gostly-io-test-does-not-exist-123456").await;
        assert!(idx.is_empty(), "missing dir must return empty index, not panic");
    }

    #[tokio::test]
    async fn load_all_service_mocks_empty_dir_returns_empty() {
        let dir = unique_dir("emptydir");
        let idx = load_all_service_mocks(&dir).await;
        assert!(idx.is_empty(), "empty dir must yield empty index");
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

        let idx = load_all_service_mocks(&dir).await;
        assert_eq!(idx.len(), 1, "exactly one service expected, got {:?}", idx.keys().collect::<Vec<_>>());
        let svc = idx.get("svc-a").expect("svc-a missing");
        assert!(svc.contains_key(&("GET".to_string(), "/users".to_string())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn load_all_service_mocks_dedupes_by_method_uri_last_write_wins() {
        let dir = unique_dir("dedupe");
        // Two entries for the same (GET, /users); HashMap keeps one. The exact
        // winner is implementation-defined since serde_json::from_str + collect
        // doesn't guarantee order — the load contract is "exactly one entry
        // per (method, uri)" which is what we assert.
        let body = format!(
            "{}\n{}\n",
            entry_line("first", "GET", "/users"),
            entry_line("second", "GET", "/users"),
        );
        tokio::fs::write(format!("{}/mock_svc.jsonl", dir), body).await.unwrap();

        let idx = load_all_service_mocks(&dir).await;
        let svc = idx.get("svc").expect("svc missing");
        assert_eq!(
            svc.len(), 1,
            "duplicates must collapse to a single entry per (method, uri)"
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

        let idx = load_all_service_mocks(&dir).await;
        let svc = idx.get("svc").expect("svc missing");
        assert_eq!(svc.len(), 2, "malformed lines must be silently skipped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn load_all_service_mocks_skips_files_with_only_invalid_lines() {
        let dir = unique_dir("allinvalid");
        // File parses to zero entries → not inserted into the index at all.
        tokio::fs::write(
            format!("{}/mock_svc.jsonl", dir),
            "garbage\nmore-garbage\n",
        ).await.unwrap();
        let idx = load_all_service_mocks(&dir).await;
        assert!(idx.is_empty(), "service with no parseable entries must be omitted");
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
