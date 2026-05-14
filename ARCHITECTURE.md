# Architecture

Gostly is a recording proxy: it sits between an application and an
upstream HTTP service, records the real traffic it sees, and replays
those recordings back as mocks.

For a five-minute setup, see [README.md](./README.md).

---

## Overview

The binary in this repo is a single-process HTTP proxy. It exposes
three modes per service:

| Mode           | Behavior                                                  |
|----------------|-----------------------------------------------------------|
| `LEARN`        | Forward every request upstream and record the pair        |
| `MOCK`         | Serve recorded responses; never reach the upstream        |
| `PASSTHROUGH`  | Forward without recording                                 |

Mode is per-service: a single proxy install can run different services
in different modes simultaneously. Mode is persisted in `data/mode.txt`
and the proxy exposes `/ghost/mode` to read or update it.

---

## Component map

```
   ┌──────────────────┐    HTTP    ┌────────────┐    HTTPS   ┌──────────────┐
   │  Your client     │ ─────────► │  gostly    │ ─────────► │  upstream    │
   │  (app, tests)    │ ◄───────── │  :8080     │ ◄───────── │  (real API)  │
   └──────────────────┘            └─────┬──────┘            └──────────────┘
                                         │
                                         ▼
                                ┌──────────────────┐
                                │  data/           │
                                │   traffic/*.jsonl│   ← raw recordings
                                │   mode.txt       │
                                │   sequences.json │
                                └──────────────────┘
```

The proxy is a single binary. The `data/` directory is the only
persistent state and lives on the user's machine.

---

## Match pipeline

For each incoming request in `MOCK` mode, the proxy walks two layers in
order. The first layer that produces a confident answer serves the
response.

### 1. Exact match

Method, path, query string, and body shape match a recorded entry. The
proxy reads `data/traffic/{service}.jsonl`, locates the entry, and
returns the recorded response verbatim. Sub-millisecond.

### 2. Smart swap (opt-in)

Set `SMART_SWAP_ENABLED=true` to enable. When enabled, requests that
miss the exact-match library try a structural / Markov-chaos swap
against the same-service recordings. The proxy serves the closest
recorded response with variable fields rewritten to match the new
request.

### 3. Total miss

If neither layer hits, the proxy returns the configured `unmatched`
status (default 404) with an explanatory body.

---

## Storage model

`data/traffic/{service}.jsonl` — append-only JSONL files containing the
exact response bodies, headers, and timing of every recorded request.
Diffable, version-controllable, no proprietary format. These files
never leave the host.

`data/mode.txt`, `data/sequences.json` — plain-text mode + sequence
state, also local-only.

There is no database, no remote sync, no telemetry shipping.

---

## Deployment patterns

### Local development

```bash
docker run --rm -p 8080:8080 ghcr.io/nicrios/gostly-proxy:latest \
  --upstream https://api.example.com
```

Point your client at `localhost:8080`.

### CI

Commit `data/` (or restore from CI cache). Start the proxy in `MOCK`
mode at the beginning of the test job. Tests run with no network
dependency on the upstream.

### Shared staging

One proxy instance per staging environment; multiple apps pointing at
it. Per-service modes mean different teams can record and replay
independently.

---

## Hot-reload model (v0.3)

The mock library is a tree of per-file `Arc<MockSegment>` segments. Each
file `data/mocks/mock_<svc>.jsonl` becomes one segment; segments are
keyed by file basename so a reload can rebuild the changed file's
segment alone, leaving the rest untouched.

The whole library lives behind an `arc_swap::ArcSwap<MockLibrary>` —
readers grab the current `Arc<MockLibrary>` in O(1) without locking,
writers publish a new library by calling `store(...)`. The proxy handler
contract is:

> Call `state.mocks.load_full()` ONCE at the top of the request. Hold
> the resulting `Arc<MockLibrary>` for the entire request lifecycle.

In-flight requests already holding the previous Arc keep using it until
they finish. New requests see the new snapshot on their next
`load_full()`. This is the same pattern rustls uses for its
`ConfigBuilder`: lock-free reads, atomic publishes, no torn views, no
panics from a writer that shrinks the index out from under a reader.

`MOCK_RELOAD_STRATEGY` selects the trigger source:

| Strategy     | Mechanism                                                       |
|--------------|-----------------------------------------------------------------|
| `fs_watch`   | `notify` crate (inotify / FSEvents / ReadDirectoryChangesW), 200 ms debounce, per-file segment rebuild |
| `signal`     | `signal-hook-tokio` listens for SIGHUP, full library reload     |
| `poll`       | `tokio::time::interval` rescans the dir at `MOCK_RELOAD_POLL_MS` (default 30 s), full library reload |
| `http_admin` | No background task — `POST /ghost/admin/reload` is the only trigger |

`POST /ghost/admin/reload` is always live regardless of strategy.

## Going further

This binary is the recording-and-replay core. AI gap-fill on traffic
you've already recorded (per-service LoRA adapters, RAG-grounded
generation), a multi-user dashboard, drift detection, training
pipelines, scrubbed managed storage, and team features (SAML / RBAC /
audit) live in the hosted Gostly product at <https://gostly.ai>.
