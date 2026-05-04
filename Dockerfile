# syntax=docker/dockerfile:1
#
# Dockerfile for the gostly recording proxy.
#
# Published to GHCR public as ghcr.io/nicrios/gostly-proxy by
# .github/workflows/release.yml on each `v*` tag.

ARG CARGO_FEATURES=oss
ARG GOSTLY_BUILD_COMMIT=dev

FROM rust:1.88-slim AS builder
ARG CARGO_FEATURES
ARG GOSTLY_BUILD_COMMIT
ENV GOSTLY_BUILD_COMMIT=${GOSTLY_BUILD_COMMIT}
WORKDIR /app

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

# Cache dependency layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release --no-default-features --features "${CARGO_FEATURES}"
RUN rm src/main.rs

# Build the real binary.
COPY src ./src
RUN touch src/main.rs && cargo build --release --no-default-features --features "${CARGO_FEATURES}"

# Pre-create the data dir owned by the runtime nonroot user (uid 65532).
# Without this the proxy can't persist its mock library because /app
# is root-owned in the runtime image and the process runs as nonroot.
RUN mkdir -p /pkg/app/data && chown -R 65532:65532 /pkg

# ── Runtime: distroless cc-debian12:nonroot ────────────────────────────────
# Minimal glibc + ca-certificates, pre-baked nonroot user (uid 65532). No
# shell, no package manager — significantly smaller and more locked-down
# than debian:bookworm-slim. Anything that needs to run inside the image
# (eg. a healthcheck) must use the proxy's HTTP surface, not exec'd shell
# commands.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /app/target/release/gostly-agent /usr/local/bin/gostly-proxy
COPY --from=builder --chown=nonroot:nonroot /pkg/app /app

WORKDIR /app
USER nonroot
EXPOSE 8080

# Runtime envelope:
#   BACKEND_URL         — upstream you want to record/replay (required)
#   PROXY_PORT          — listen port (default 8080)
#   INITIAL_MODE        — LEARN | MOCK | PASSTHROUGH (default LEARN)
#   SMART_SWAP_ENABLED  — opt-in MOCK-mode structural fallback (default false)
#   REDACT_HEADERS      — comma-separated extra headers to redact when recording
#   ACCEPT_INVALID_CERTS — disable upstream TLS verification (dev only)
#
# Example:
#   docker run --rm -p 8080:8080 \
#     -e BACKEND_URL=https://api.example.com \
#     -v $(pwd)/data:/app/data \
#     ghcr.io/nicrios/gostly-proxy:latest
#
# To persist the mock library across container restarts, bind-mount a
# host directory at /app/data. The host directory must be writable by
# uid 65532 (the nonroot user this image runs as).

ENTRYPOINT ["/usr/local/bin/gostly-proxy"]
