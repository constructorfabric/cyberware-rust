# Multi-stage build for the CF/Gears Platform Host image (OoP-10 task 10.3).
#
# The platform-host bundles the trust-coupled core (authz-resolver,
# tenant-resolver, resource-group, account-management) + system gears
# (gear-orchestrator, types-registry, credstore, api-gateway, grpc-hub) +
# embedded authn-resolver. See docs/arch/toolkit-oop/DESIGN.md
# "Platform Host Composition".
#
# Build (dev plugins, default):
#   docker build -f deploy/docker/platform-host.Dockerfile \
#     -t ghcr.io/constructorfabric/platform-host:dev .
#
# Build (production plugins: OIDC authn + tr-authz + rg-tr, k8s auth):
#   docker build -f deploy/docker/platform-host.Dockerfile \
#     --build-arg CARGO_NO_DEFAULT_FEATURES=1 \
#     --build-arg CARGO_FEATURES="prod-plugins k8s" \
#     -t ghcr.io/constructorfabric/platform-host:prod .

# ---------------------------------------------------------------------------
# Stage 1: Builder
# ---------------------------------------------------------------------------
FROM rust:1.95.0-bookworm@sha256:6bb82db0878825e157664188b319c875de4f1fff5d70f5917b3a3f1974b472e4 AS builder

# BUILD_PROFILE: "release" (default, optimized) or "dev" (fast compile).
ARG BUILD_PROFILE=release
# Extra cargo features to enable, space-separated (e.g. "prod-plugins k8s otel").
ARG CARGO_FEATURES=""
# Set to a non-empty value (e.g. "1") to pass --no-default-features (drops the
# default dev-plugins preset - required when building the prod-plugins image).
ARG CARGO_NO_DEFAULT_FEATURES=""

# protobuf-compiler is required by prost-build (gRPC / directory protos).
RUN apt-get update && \
    apt-get install -y --no-install-recommends cmake protobuf-compiler libprotobuf-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the full workspace context (.dockerignore trims target/, .git/, logs/).
COPY . .

# Build the platform-host binary. BuildKit cache mounts persist the cargo
# registry + target dir across builds; the binary is copied out to /tmp because
# the target dir is a cache mount and does not survive the layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    set -eux; \
    RELEASE_FLAG=""; OUTPUT_DIR="debug"; \
    if [ "$BUILD_PROFILE" = "release" ]; then RELEASE_FLAG="--release"; OUTPUT_DIR="release"; fi; \
    NO_DEFAULT_FLAG=""; \
    if [ -n "$CARGO_NO_DEFAULT_FEATURES" ]; then NO_DEFAULT_FLAG="--no-default-features"; fi; \
    FEATURES_FLAG=""; \
    if [ -n "$CARGO_FEATURES" ]; then FEATURES_FLAG="--features $CARGO_FEATURES"; fi; \
    cargo build $RELEASE_FLAG $NO_DEFAULT_FLAG $FEATURES_FLAG \
        --bin platform-host --package cf-gears-platform-host; \
    cp "/build/target/$OUTPUT_DIR/platform-host" /tmp/platform-host

# ---------------------------------------------------------------------------
# Stage 2: Runtime
# ---------------------------------------------------------------------------
FROM debian:13.3-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Binary (copied via /tmp because the builder target dir is a cache mount).
COPY --from=builder /tmp/platform-host /app/platform-host
# Default config; override by mounting a ConfigMap over /app/config in k8s.
COPY --from=builder /build/config/platform-host.yaml /app/config/platform-host.yaml

# HTTP API / probes (/healthz, /readyz, /health) served on 8087.
EXPOSE 8087

# Runtime state (SQLite DBs, logs) lives under a writable, container-local path
# rather than the non-root user's (absent) home dir. Overrides the config's
# `server.home_dir` via the APP__ env layer. Mount a volume here for persistence.
ENV APP__SERVER__HOME_DIR=/app/data

RUN useradd -U -u 1000 appuser && \
    mkdir -p /app/data && \
    chown -R 1000:1000 /app
USER 1000

CMD ["/app/platform-host", "--config", "/app/config/platform-host.yaml", "run"]
