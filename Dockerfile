# Dockerfile for the gostly recording proxy.
#
# Published to GHCR public as ghcr.io/nicrios/gostly-proxy by
# .github/workflows/release.yml on each `v*` tag.

ARG CARGO_FEATURES=oss

FROM rust:1.88-slim AS builder
ARG CARGO_FEATURES
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

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

# Non-root user.
RUN groupadd --gid 10001 ghost && useradd --uid 10001 --gid ghost --no-create-home ghost
WORKDIR /app
COPY --from=builder /app/target/release/gostly-agent /usr/local/bin/gostly-proxy

# Runtime envelope:
#   BACKEND_URL         — upstream you want to record/replay (required)
#   PROXY_PORT          — listen port (default 8080)
#   INITIAL_MODE        — learn | mock (default learn)
#
# Example:
#   docker run --rm -p 8080:8080 \
#     -e BACKEND_URL=http://api.example.com \
#     -v $(pwd)/data:/app/data \
#     ghcr.io/nicrios/gostly-proxy:latest

USER ghost
EXPOSE 8080
CMD ["gostly-proxy"]
