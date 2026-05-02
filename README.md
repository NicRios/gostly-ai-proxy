# gostly

> Recording proxy. Record HTTP traffic, replay it as mocks. Apache-2.0 in 2 years.

[![Build](https://github.com/NicRios/gostly-ai-proxy/actions/workflows/ci.yml/badge.svg)](https://github.com/NicRios/gostly-ai-proxy/actions)
[![License: FSL-1.1-Apache-2.0](https://img.shields.io/badge/license-FSL--1.1--Apache--2.0-blue.svg)](LICENSE.md)

## What it does

Point gostly at an upstream HTTP service. It forwards real traffic and records every request/response pair to JSONL on disk. Flip a switch and it replays those recordings as mocks — your tests run with no network.

## Install

### macOS / Linux

```
curl -fsSL https://gostly.ai/install.sh | sh
```

### Homebrew

```
brew install nicrios/gostly/gostly
```

### Windows (Scoop)

```
scoop bucket add gostly https://github.com/NicRios/spectre-ai
scoop install gostly
```

### Docker

```
docker run -p 8080:8080 ghcr.io/nicrios/gostly-proxy:latest
```

## Quick start

```
gostly start --upstream https://api.example.com
# proxy listens on :8080, records traffic to ./data/traffic/

# point your client at http://localhost:8080
# requests pass through to upstream, responses get recorded

gostly mode mock    # flip to MOCK mode — replays from library
gostly mode learn   # flip back to recording
```

## Modes

- **LEARN** — pass through, record traffic to JSONL
- **MOCK** — replay from recorded library; falls back per config
- **PASSTHROUGH** — pure pass-through (debugging)

## Architecture

```
  your client  ──HTTP──▶  gostly :8080  ──HTTPS──▶  real upstream
                              │
                              ▼
                       data/traffic/*.jsonl
                       (append-only, on your machine)
```

Three modes per service. Mock library is plain JSONL — diffable, version-controllable, no proprietary format.

## What's OSS vs cloud

| Feature                                  | OSS (this repo) | Cloud (gostly.ai) |
|------------------------------------------|:---------------:|:-----------------:|
| Recording proxy                          |        ✓        |         ✓         |
| Replay engine                            |        ✓        |         ✓         |
| OpenAPI / Postman / HAR import           |        ✓        |         ✓         |
| Basic chaos primitives (latency, errors) |        ✓        |         ✓         |
| AI gap-fill (LoRA on your traffic)       |        —        |         ✓         |
| Multi-user dashboard                     |        —        |         ✓         |
| Drift detection (v1 narrow)              |        —        |         ✓         |
| Team features (SAML / RBAC / audit)      |        —        |     Q3 2026       |

The OSS binary never calls `gostly.ai`. Verify with `strings $(which gostly) | grep gostly.ai` — should be empty.

## Status

Active development. v0.1 release targeted Sat May 23 2026. Scope is frozen for v1 to keep maintenance solo-cadence-friendly.

## License

FSL-1.1-Apache-2.0. See [LICENSE.md](LICENSE.md). After 2 years from each release, that version automatically converts to Apache 2.0.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). v1 scope is frozen — issue reports and bug-fix PRs welcome; new features go to the closed cloud product unless they fit the OSS scope.

## Links

- Cloud product: https://gostly.ai
- Docs: https://gostly.ai/docs
- Issues: https://github.com/NicRios/gostly-ai-proxy/issues
- Architecture deep dive: [ARCHITECTURE.md](ARCHITECTURE.md)
