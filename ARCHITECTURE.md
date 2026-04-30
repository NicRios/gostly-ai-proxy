# Architecture

Gostly is an HTTP proxy that records real API traffic and replays it back to the calling application. This document describes the components, the matching pipeline, the storage model, and how the system is deployed.

For a five-minute setup, see [README.md](./README.md).

---

## Overview

The proxy sits between an application and an upstream HTTP service. In `LEARN` mode it forwards requests upstream and records every request/response pair. In `MOCK` mode it serves recorded responses without reaching the upstream. One further mode (`PASSTHROUGH`) forwards without recording.

When a request in `MOCK` mode does not match a recorded pair exactly, two further matching layers run before returning 502.

---

## Component map

```
┌─────────────────────────────────────────────────────────────────┐
│                       Calling application                       │
└─────────────────────────────┬───────────────────────────────────┘
                              │ HTTP
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  Caddy (TLS termination, ports 80/443)                          │
└──────────┬────────────────────────────┬─────────────────────────┘
           │                            │
           ▼                            ▼
┌──────────────────┐          ┌──────────────────────┐
│  Proxy           │          │  Dashboard           │
│  port 8080       │ ◄──────► │  port 3000           │
└────────┬─────────┘          └──────────┬───────────┘
         │                               │
         │ control plane                 │ REST
         ▼                               ▼
┌─────────────────────────────────────────────────────────────────┐
│  API  — port 8000                                               │
└──────────┬─────────────────────────────┬────────────────────────┘
           │                             │
           ▼                             ▼
┌──────────────────┐          ┌──────────────────────┐
│  Postgres 16     │          │  Inference           │
│                  │          │  port 5000           │
└──────────────────┘          └──────────────────────┘

Shared volume: ./data/
  mock_library.jsonl      sequences.jsonl       mode.txt
  machine_id              license_cache.json    active_adapters.json
  traffic/{svc}.jsonl     models/adapters/{svc}/{session}/
```

| Container   | Responsibility                                       |
|-------------|------------------------------------------------------|
| `proxy`     | HTTP capture and replay                              |
| `api`       | Control plane, mock library, training pipeline       |
| `dashboard` | Operator UI                                          |
| `inference` | AI fallback for unmatched requests                   |
| `postgres`  | Persistent state (scrubbed mocks, services, sessions)|

All five containers run on the same host. The only outbound network call from the stack is license validation against the configured license server.

---

## Match pipeline

For each incoming request in `MOCK` mode, the proxy walks three layers in order. The first layer that produces a confident answer serves the response.

### 1. Exact match

Method, path, query string, and body shape match a recorded entry. The proxy reads `data/mock_{service}.jsonl`, locates the entry, and returns the recorded response verbatim. Sub-millisecond, no AI involved.

### 2. Smart swap

The request is structurally identical to a recorded one but differs in specific fields (IDs, timestamps, cursors). The proxy serves the recorded response with variable fields rewritten to match the new request.

### 3. AI generation

When neither layer matches, the proxy posts to the inference service with the request and a set of recorded examples for that endpoint. Inference returns a synthesized response.

Two AI paths:

| Source label        | Path                                      | Confidence | Requirement                  |
|---------------------|-------------------------------------------|-----------:|------------------------------|
| `generative`        | Per-service LoRA adapter + RAG examples   | ~0.70      | ≥50 recorded examples, training run |
| `generative_base`   | Base model + RAG examples in prompt       | ~0.60      | None — works from request 1  |

Every AI response includes the `matched_example_id` field so the recorded interaction that grounded the generation can be inspected.

---

## Storage model

Two separate stores back the system.

### `data/mock_{service}.jsonl` — verbatim local serving

Append-only JSONL files on the Docker volume containing the exact response bodies, headers, and timing of every recorded request. Unscrubbed, full fidelity. The proxy reads from these files directly.

These files never leave the host. There is no remote sync, backup, or telemetry.

### Postgres `MockEntry` — scrubbed controlled store

The API runs a transition pipeline that takes recorded mocks, scrubs them (Authorization headers removed, known PII fields redacted, IDs normalized), and writes the result to Postgres. Each row carries a `scrubbed_at` timestamp.

Postgres is used for:

1. **AI training inputs.** Credentials and PII degrade model output and create exfiltration risk.
2. **Managed-DB data sovereignty.** In Team and enterprise deployments where Postgres may be hosted externally (RDS, Cloud SQL), every row that lands in Postgres has been scrubbed. The `scrubbed_at` column is the safety boundary for any operation that moves data off the host (exports, training streams).

### Invariants

- The proxy reads from JSONL only.
- The training pipeline reads from Postgres only.
- Postgres rows without `scrubbed_at` set are not eligible for any off-host operation.
- The two stores are not kept in sync. They serve different consumers with different requirements.

---

## Modes

Mode is per-service. Multiple services in a single proxy install can run different modes simultaneously. Mode is persisted in `data/mode.txt`; the API exposes `/ghost/mode` to read and update.

| Mode           | Behavior                                                  |
|----------------|-----------------------------------------------------------|
| `LEARN`        | Forward all requests upstream, record every pair          |
| `MOCK`         | Serve recorded responses, never touch the upstream        |
| `PASSTHROUGH`  | Forward without recording                                 |

---

## AI pipeline

The training and inference path is retrieval-first. Fine-tuning is optional.

1. **Recording** writes to `data/mock_{service}.jsonl`.
2. **Transition** scrubs each pair and writes to Postgres `MockEntry`, deduplicating and tagging by service / endpoint / method.
3. **Patterns endpoint** (`/patterns/{service_id}`) groups scrubbed mocks by endpoint shape and returns up to 10 examples per pattern. The proxy fetches this on every cache miss.
4. **Inference call** (`POST /generate`) sends the request, the matching examples (RAG context), and optionally a service-specific LoRA adapter.
5. **Response** is returned tagged with the example IDs that grounded it.

The inference model is Qwen 2.5 0.5B. It runs on CPU, fits in approximately 1.5 GB of memory, and serves a request in tens of milliseconds. A per-service LoRA adapter is an optional second step taken when an endpoint has high cardinality (>200 distinct shapes) or strict response schemas; until an adapter exists, `generative_base` covers the same surface with lower confidence.

`ENABLE_RAG=true` activates retrieval. `ENABLE_GENERATION=true` activates the model. Both are required for the AI path; either can be disabled without affecting the exact and smart-swap layers.

---

## Deployment patterns

### Local development

```bash
docker compose up
```

Application points at `localhost:8080`. Default service config is in `docker-compose.yml`.

### CI

Recorded `data/` directory is committed (or restored from a CI cache). Proxy starts in `MOCK` mode at the beginning of the test job. Test suite runs with no network dependency on the upstream.

### Shared staging

One proxy instance per staging environment, multiple applications pointing at it. Mode is per-service, so different teams can run different modes without stepping on each other.

