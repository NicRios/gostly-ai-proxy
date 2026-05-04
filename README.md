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
