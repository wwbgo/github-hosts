# syntax=docker/dockerfile:1

# ---- build ----
FROM rust:1-slim-bookworm AS builder
WORKDIR /src

# Cache the dependency graph separately from our sources: touching src/ then
# does not re-download the world.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir -p src \
 && printf 'fn main() {}\n' > src/main.rs \
 && cargo build --release --locked 2>/dev/null || cargo build --release \
 && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

# ---- runtime ----
FROM debian:bookworm-slim
ARG VERSION=0.0.0
LABEL org.opencontainers.image.title="github-hosts" \
      org.opencontainers.image.description="Probe GitHub edge IPs and serve a verified hosts block" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.source="https://github.com/wwbgo/github-hosts" \
      org.opencontainers.image.licenses="MIT"

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tzdata curl \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/github-hosts /usr/local/bin/github-hosts

# The probe owns this directory: hosts.txt and snapshot.json live here and the
# HTTP API reads them back. Mount a volume on /data or every restart starts cold.
ENV PORT=8080 \
    INTERVAL_SECS=900 \
    PROBE_TIMEOUT_SECS=6 \
    CONCURRENCY=12 \
    MAX_SCAN=40 \
    TOP_K=12 \
    REPEATS=2 \
    HOSTS_MODE=pin \
    STATE_DIR=/data \
    TZ=Asia/Shanghai \
    RUST_LOG=info

RUN mkdir -p /data && chown 65534:65534 /data
VOLUME ["/data"]

EXPOSE 8080

# Unprivileged: the service only needs to write under /data.
USER 65534:65534

# Liveness, not data quality: /healthz answers 503 during a blockade even though
# the service is working exactly as designed. See src/api.rs.
HEALTHCHECK --interval=60s --timeout=5s --start-period=90s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:${PORT}/livez" || exit 1

ENTRYPOINT ["/usr/local/bin/github-hosts"]
