//! ResourceStore mixed-workload stress bench.
//!
//! The lookup-only bench (resource_store_lookup_bench) showed sub-µs reads
//! on a quiescent store. This one measures per-op latency under contention:
//! many concurrent lookups while writers insert + transitions mutate in
//! place. The lookup-bench's RwLock-only model is preserved (writes go
//! through `write().await`, reads through `read().await`) so the contention
//! shape matches the production hot path.
//!
//! Workload:
//!   - 8 reader tasks → tight loop of random lookups
//!   - 2 writer tasks → capture_create at sustained rate
//!   - 1 transition task → mutate state on a random captured resource
//!   - 30 sec wall-clock
//!
//! Output: req/sec + p50/p95/p99 per workload class.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

#[derive(Clone)]
struct StubResourceState {
    // `body` is written when seeding the bench fixtures; the read side
    // only inspects `current_state` for the stress-loop's transition path.
    // Kept for parity with the production `ResourceStore` shape so any
    // future bench that simulates body-aware reads doesn't need a refactor.
    #[allow(dead_code)]
    body: serde_json::Value,
    current_state: Option<String>,
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
            .insert((collection.to_string(), id.to_string()), StubResourceState { body, current_state: Some("created".to_string()) });
    }
    async fn lookup(&self, svc: &str, collection: &str, id: &str) -> Option<StubResourceState> {
        let map = self.by_service.read().await;
        map.get(svc)
            .and_then(|svcm| svcm.get(&(collection.to_string(), id.to_string())))
            .cloned()
    }
    async fn transition(&self, svc: &str, collection: &str, id: &str, new_state: &str) -> bool {
        let mut map = self.by_service.write().await;
        if let Some(svcm) = map.get_mut(svc) {
            if let Some(state) = svcm.get_mut(&(collection.to_string(), id.to_string())) {
                state.current_state = Some(new_state.to_string());
                return true;
            }
        }
        false
    }
}

fn fast_rand_id(seed: u64, max: usize) -> usize {
    // xorshift64 - good enough for sampling
    let mut x = seed;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    (x as usize) % max
}

const WARMUP_RESOURCES: usize = 1_000;
const SVC: &str = "stress-svc";
const COLLECTION: &str = "charges";
const DURATION_SECS: u64 = 20;
const N_READERS: usize = 8;
const N_WRITERS: usize = 2;
const N_TRANSITIONS: usize = 1;

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    let store = Arc::new(StubStore::new());

    // Pre-seed
    for i in 0..WARMUP_RESOURCES {
        store.insert(
            SVC,
            COLLECTION,
            &format!("ch_{i}"),
            serde_json::json!({ "id": format!("ch_{i}"), "amount": i * 10 }),
        ).await;
    }
    let resource_count = Arc::new(AtomicU64::new(WARMUP_RESOURCES as u64));

    let stop = Arc::new(AtomicBool::new(false));

    // Latency samples per workload (micros)
    let read_lat = Arc::new(parking_lot::Mutex::new(Vec::<u32>::with_capacity(2_000_000)));
    let write_lat = Arc::new(parking_lot::Mutex::new(Vec::<u32>::with_capacity(500_000)));
    let trans_lat = Arc::new(parking_lot::Mutex::new(Vec::<u32>::with_capacity(500_000)));

    let mut handles = Vec::new();

    // Readers
    for r in 0..N_READERS {
        let store = store.clone();
        let stop = stop.clone();
        let lat = read_lat.clone();
        let count = resource_count.clone();
        handles.push(tokio::spawn(async move {
            let mut local = Vec::with_capacity(250_000);
            let mut seed: u64 = 0xdead_beef + r as u64;
            while !stop.load(Ordering::Relaxed) {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let n = count.load(Ordering::Relaxed) as usize;
                let idx = fast_rand_id(seed, n.max(1));
                let id = format!("ch_{idx}");
                let t0 = Instant::now();
                let _ = store.lookup(SVC, COLLECTION, &id).await;
                let dt = t0.elapsed().as_micros() as u32;
                local.push(dt);
            }
            lat.lock().extend_from_slice(&local);
        }));
    }

    // Writers
    for w in 0..N_WRITERS {
        let store = store.clone();
        let stop = stop.clone();
        let lat = write_lat.clone();
        let count = resource_count.clone();
        handles.push(tokio::spawn(async move {
            let mut local = Vec::with_capacity(125_000);
            let mut counter: u64 = 1_000_000 + (w as u64) * 1_000_000;
            while !stop.load(Ordering::Relaxed) {
                counter += 1;
                let id = format!("ch_{counter}");
                let body = serde_json::json!({ "id": id, "amount": counter });
                let t0 = Instant::now();
                store.insert(SVC, COLLECTION, &id, body).await;
                let dt = t0.elapsed().as_micros() as u32;
                local.push(dt);
                count.fetch_add(1, Ordering::Relaxed);
            }
            lat.lock().extend_from_slice(&local);
        }));
    }

    // Transitions
    for t in 0..N_TRANSITIONS {
        let store = store.clone();
        let stop = stop.clone();
        let lat = trans_lat.clone();
        let count = resource_count.clone();
        handles.push(tokio::spawn(async move {
            let mut local = Vec::with_capacity(125_000);
            let mut seed: u64 = 0xabad_cafe + t as u64;
            let states = ["captured", "refunded", "created", "voided"];
            let mut s = 0;
            while !stop.load(Ordering::Relaxed) {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let n = count.load(Ordering::Relaxed) as usize;
                let idx = fast_rand_id(seed, n.max(1));
                let id = format!("ch_{idx}");
                let new_state = states[s % states.len()];
                s += 1;
                let t0 = Instant::now();
                let _ = store.transition(SVC, COLLECTION, &id, new_state).await;
                let dt = t0.elapsed().as_micros() as u32;
                local.push(dt);
            }
            lat.lock().extend_from_slice(&local);
        }));
    }

    let start = Instant::now();
    tokio::time::sleep(Duration::from_secs(DURATION_SECS)).await;
    stop.store(true, Ordering::Relaxed);
    for h in handles { let _ = h.await; }
    let elapsed = start.elapsed().as_secs_f64();

    let mut reads = read_lat.lock().clone();
    let mut writes = write_lat.lock().clone();
    let mut transitions = trans_lat.lock().clone();

    fn percentile(v: &mut [u32], p: f64) -> u32 {
        if v.is_empty() { return 0; }
        v.sort_unstable();
        let idx = ((v.len() as f64 - 1.0) * p) as usize;
        v[idx]
    }
    fn mean(v: &[u32]) -> f64 {
        if v.is_empty() { return 0.0; }
        v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64
    }

    let n_read = reads.len();
    let n_write = writes.len();
    let n_trans = transitions.len();
    let total_resources = resource_count.load(Ordering::Relaxed);

    println!("\n=== ResourceStore mixed-workload stress ===");
    println!("Duration: {:.2}s, workers: {}r/{}w/{}t, final resource count: {}",
        elapsed, N_READERS, N_WRITERS, N_TRANSITIONS, total_resources);
    println!();
    println!("| Workload    | Ops      | Throughput     | mean   | p50  | p95  | p99   |");
    println!("|-------------|----------|----------------|--------|------|------|-------|");
    println!("| Lookup      | {:>8} | {:>10.0}/s | {:>4.1}µs | {:>3}µs | {:>3}µs | {:>4}µs |",
        n_read, n_read as f64 / elapsed, mean(&reads),
        percentile(&mut reads, 0.50), percentile(&mut reads, 0.95), percentile(&mut reads, 0.99));
    println!("| Capture     | {:>8} | {:>10.0}/s | {:>4.1}µs | {:>3}µs | {:>3}µs | {:>4}µs |",
        n_write, n_write as f64 / elapsed, mean(&writes),
        percentile(&mut writes, 0.50), percentile(&mut writes, 0.95), percentile(&mut writes, 0.99));
    println!("| Transition  | {:>8} | {:>10.0}/s | {:>4.1}µs | {:>3}µs | {:>3}µs | {:>4}µs |",
        n_trans, n_trans as f64 / elapsed, mean(&transitions),
        percentile(&mut transitions, 0.50), percentile(&mut transitions, 0.95), percentile(&mut transitions, 0.99));
    println!();
    println!("Note: stub store mirrors the agent's RwLock<HashMap<...>> shape; persistence (JSONL append) is NOT in this measurement.");
}
