# gostly

> Recording proxy. Record HTTP traffic, replay it as mocks. Apache-2.0 in 2 years.

[![Build](https://github.com/NicRios/gostly-ai-proxy/actions/workflows/build.yml/badge.svg)](https://github.com/NicRios/gostly-ai-proxy/actions)
[![License: FSL-1.1-Apache-2.0](https://img.shields.io/badge/license-FSL--1.1--Apache--2.0-blue.svg)](LICENSE.md)

## What it does

Point gostly at an upstream HTTP service. It forwards real traffic and records every request/response pair to JSONL on disk. Flip a switch and it replays those recordings as mocks — your tests run with no network.

## Install

### macOS / Linux

```
curl -fsSL https://raw.githubusercontent.com/NicRios/gostly-ai-proxy/main/install.sh | bash
```

### Homebrew

```
brew tap nicrios/gostly https://github.com/NicRios/gostly-ai-proxy
brew install gostly
```

### Windows (Scoop)

```
scoop bucket add gostly https://github.com/NicRios/gostly-ai-proxy
scoop install gostly
```

### Docker

```
docker run -p 8080:8080 \
  -e BACKEND_URL=https://api.example.com \
  ghcr.io/nicrios/gostly-proxy:latest
```

To persist the mock library across container restarts, bind-mount a host directory at `/app/data`. The host directory must be writable by uid 65532 (the nonroot user the image runs as):

```
mkdir -p data && sudo chown 65532:65532 data
docker run -p 8080:8080 \
  -e BACKEND_URL=https://api.example.com \
  -v "$(pwd)/data:/app/data" \
  ghcr.io/nicrios/gostly-proxy:latest
```

## Quick start

You always need to tell gostly *what upstream to record*. Pick one of:

```bash
# CLI flag
gostly start --upstream https://api.example.com

# or env var (also how the docker image is configured)
BACKEND_URL=https://api.example.com gostly start
```

Then point your client at `http://localhost:8080` instead of the real upstream:

```
# proxy is on :8080, recording everything to ./data/traffic/

gostly mode mock    # flip to MOCK mode — replays from library
gostly mode learn   # flip back to recording
```

## Modes

- **LEARN** — pass through, record traffic to JSONL
- **MOCK** — replay from recorded library; falls back per config
- **PASSTHROUGH** — pure pass-through (debugging)

## Hot-reload

Edit `data/mocks/*.jsonl` with a text editor; the proxy picks up the change without a restart. Pick a strategy with `MOCK_RELOAD_STRATEGY`:

| Value        | When to use                                                                  |
|--------------|------------------------------------------------------------------------------|
| `fs_watch`   | Default. inotify (Linux) / FSEvents (macOS) / ReadDirectoryChangesW (Windows). 200 ms debounce coalesces editor-save bursts. |
| `signal`     | k8s / Docker with shared volumes where filesystem events on bind-mounts or PVCs are flaky. `kubectl exec ... kill -HUP 1` triggers a reload. |
| `poll`       | NFS / EFS / GCS-FUSE — anywhere fs events and SIGHUP are both unreliable. Interval defaults to 30 s; override with `MOCK_RELOAD_POLL_MS`. |
| `http_admin` | Hosted / managed deployments. The route `POST /ghost/admin/reload` is always live regardless of strategy; this option just disables the background watcher. |

In-flight safety: the proxy snapshots the live mock library at the top of every request and holds that snapshot until the request finishes. A reload mid-request publishes a *new* library; the in-flight handler keeps using the old one. New requests pick up the new library on their next snapshot.

```bash
# Edit the file …
echo '{"id":"u1","timestamp":"2026-05-04T00:00:00Z","request":{"method":"GET","uri":"/users/1","body":""},"response":{"status":200,"headers":{},"body":"{\"v\":2}","latency_ms":0}}' \
  > data/mocks/mock__global.jsonl

# fs_watch picks it up automatically. To force a reload:
curl -X POST http://localhost:8080/ghost/admin/reload
```

## Per-test isolation

A single proxy can serve N parallel test workers without cross-pollution. Tag each worker's traffic with `X-Gostly-Tenant: <id>` (preferred) or `?_tenant=<id>` (fallback for clients that can't set headers). Mocks under tenant `worker-3` are invisible to requests under any other tenant. The default tenant is `_global`.

```bash
# Worker A records under tenant test-a
curl -H "X-Gostly-Tenant: test-a" http://localhost:8080/api/users/1

# Worker B records under tenant test-b — different mock, same endpoint
curl -H "X-Gostly-Tenant: test-b" http://localhost:8080/api/users/1
```

Backwards compatible: pre-v0.3 JSONL files (no `tenant` field) load as `_global`. Existing recordings keep working.

## Sequences

For testing retry logic, polling endpoints, or multi-step flows: define an ordered list of responses for a single endpoint. The cursor advances on each call.

```
POST /ghost/sequences
{
  "id": "checkout-retry",
  "method": "POST",
  "uri": "/api/checkout",
  "responses": [
    {"status": 503, "body": "{\"error\": \"transient\"}"},
    {"status": 503, "body": "{\"error\": \"transient\"}"},
    {"status": 200, "body": "{\"order_id\": \"ord_123\"}"}
  ],
  "loop_responses": false
}
```

`POST /ghost/sequences/{id}/reset` rewinds the cursor.
`DELETE /ghost/sequences/{id}` removes the sequence.
`GET /ghost/sequences` lists everything currently loaded.

## Multi-service

Route by `Host` header or path prefix to any number of upstreams in a single proxy. Each service gets its own mock library, mode, chaos config, and redaction rules — so service-A can be recording while service-B is replaying, in the same instance.

```
POST /ghost/upstreams
{
  "upstreams": [
    {"routing_type":"host","routing_value":"api.stripe.com",  "service_id":"stripe",  "upstream_url":"https://api.stripe.com",  "mode":"LEARN"},
    {"routing_type":"host","routing_value":"api.twilio.com",  "service_id":"twilio",  "upstream_url":"https://api.twilio.com",  "mode":"MOCK"},
    {"routing_type":"path","routing_value":"/api/orders",     "service_id":"orders",  "upstream_url":"http://orders.local"},
    {"routing_type":"path","routing_value":"/api/billing",    "service_id":"billing", "upstream_url":"http://billing.local"}
  ]
}
```

No cap on services. No cap on mocks per service.

## Architecture

```
  your client  ──HTTP──▶  gostly :8080  ──HTTPS──▶  real upstream
                              │
                              ▼
                       data/traffic/*.jsonl
                       (append-only, on your machine)
```

Three modes per service. Mock library is plain JSONL — diffable, version-controllable, no proprietary format.

## Want more?

This repo is the recording proxy itself. AI gap-fill on traffic you've
recorded, a multi-user dashboard, drift detection, and team features
(SAML / RBAC / audit) live in the hosted Gostly product —
<https://gostly.ai>.

The binary in this repo runs entirely on your machine.

## Status

Active development. Scope is frozen for v1 to keep maintenance solo-cadence-friendly.

## License

FSL-1.1-Apache-2.0. See [LICENSE.md](LICENSE.md). After 2 years from each release, that version automatically converts to Apache 2.0.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). v1 scope is frozen — issue reports and bug-fix PRs welcome; larger features go to the hosted product at <https://gostly.ai>.

## Links

- Hosted product: https://gostly.ai
- Issues: https://github.com/NicRios/gostly-ai-proxy/issues
- Architecture deep dive: [ARCHITECTURE.md](ARCHITECTURE.md)
