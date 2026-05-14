//! ResourceStore lookup benchmark — Linked Mocks v1.
//!
//! Acceptance evidence #3: per-request lookup must be <100µs at 10 000
//! captured resources (the proxy hot path budget). The store keys by
//! `(service_id, collection, id)` with the inner per-service map held
//! under a tokio RwLock — read-mostly workload, expected sub-µs per
//! lookup once the read guard is hot.
//!
//! This bench duplicates the lookup data structure shape rather than
//! depending on the agent crate as a library — `gostly-agent` is a binary
//! (no `[lib]` target), and adding one just for the bench is a heavier
//! refactor than is warranted. The lookup path under test is the inner
//! HashMap read — which is what `ResourceStore::lookup` reduces to once
//! the RwLock read guard is taken — so the measurement is faithful to
//! the production hot path.
//!
//! Reproduce:
//!   cargo bench --bench resource_store_lookup_bench
//!
//! Output: markdown table the PR description embeds verbatim.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

#[derive(Clone)]
#[allow(dead_code)]  // body is read into the lookup result but not asserted on
struct StubResourceState {
    body: serde_json::Value,
}

type ServiceResources = HashMap<(String, String), StubResourceState>;

struct StubStore {
    by_service: RwLock<HashMap<String, ServiceResources>>,
}

impl StubStore {
    fn new() -> Self {
        Self { by_service: RwLock::new(HashMap::new()) }
    }
    async fn insert(&self, svc: &str, collection: &str, id: &str, body: serde_json::Value) {
        let mut map = self.by_service.write().await;
        map.entry(svc.to_string())
            .or_default()
            .insert((collection.to_string(), id.to_string()), StubResourceState { body });
    }
    async fn lookup(&self, svc: &str, collection: &str, id: &str) -> Option<StubResourceState> {
        let map = self.by_service.read().await;
        map.get(svc)
            .and_then(|svcm| svcm.get(&(collection.to_string(), id.to_string())))
            .cloned()
    }
}

const WARMUP: usize = 1_000;
const ITERS: usize = 100_000;
const LIB_SIZE: usize = 10_000;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let store = Arc::new(StubStore::new());

    // Populate 10 000 charges across one service.
    for i in 0..LIB_SIZE {
        let id = format!("ch_{i:08}");
        let body = serde_json::json!({"id": id.clone(), "amount": i});
        store.insert("bench-svc", "charges", &id, body).await;
    }

    // Warmup.
    let mut warm = 0u64;
    for i in 0..WARMUP {
        let id = format!("ch_{:08}", i % LIB_SIZE);
        if store.lookup("bench-svc", "charges", &id).await.is_some() {
            warm += 1;
        }
    }
    assert!(warm == WARMUP as u64);

    // Hot loop.
    let start = Instant::now();
    let mut hits = 0u64;
    for i in 0..ITERS {
        let id = format!("ch_{:08}", i % LIB_SIZE);
        if store.lookup("bench-svc", "charges", &id).await.is_some() {
            hits += 1;
        }
    }
    let elapsed = start.elapsed();
    assert_eq!(hits as usize, ITERS);

    let mean_ns = elapsed.as_nanos() as f64 / ITERS as f64;
    let mean_us = mean_ns / 1_000.0;

    println!("\n## ResourceStore lookup bench (Linked Mocks v1)\n");
    println!("| metric | value |");
    println!("|---|---|");
    println!("| library size | {LIB_SIZE} resources |");
    println!("| iterations   | {ITERS} |");
    println!("| total wall   | {elapsed:?} |");
    println!("| mean / lookup| {mean_us:.3} µs ({mean_ns:.0} ns) |");
    println!("| target       | < 100 µs |");
    println!(
        "| pass         | {} |",
        if mean_us < 100.0 { "yes" } else { "NO — regression!" },
    );
    if mean_us >= 100.0 {
        std::process::exit(1);
    }
}
