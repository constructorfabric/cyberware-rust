# Generalized multi-stage build for a single OoP gear image (OoP-10 task 10.4).
#
# Parameterized by build args so one Dockerfile produces a minimal image per
# gear. Because OoP dependency resolution goes through the DirectoryService +
# REST clients (not in-process linking), a gear image only compiles the target
# gear crate + its SDK/client deps - not the full gear set.
#
# Build the hello OoP gear:
#   docker build -f deploy/docker/oop-gear.Dockerfile \
#     --build-arg GEAR_PACKAGE=hello \
#     --build-arg GEAR_BIN=hello-oop \
#     --build-arg GEAR_FEATURES=oop_module \
#     --build-arg GEAR_CONFIG=config/oop-hello.yaml \
#     -t ghcr.io/constructorfabric/hello:1.0.0 .

# ---------------------------------------------------------------------------
# Stage 1: Builder
# ---------------------------------------------------------------------------
FROM rust:1.95.0-bookworm@sha256:6bb82db0878825e157664188b319c875de4f1fff5d70f5917b3a3f1974b472e4 AS builder

# Cargo package (crate) name, e.g. "hello".
ARG GEAR_PACKAGE
# Binary target name within the package, e.g. "hello-oop".
ARG GEAR_BIN
# Space-separated cargo features required by the OoP binary, e.g. "oop_module".
ARG GEAR_FEATURES=""
# BUILD_PROFILE: "release" (default) or "dev" (fast compile).
ARG BUILD_PROFILE=release

# protobuf-compiler is required by prost-build (gRPC / directory protos).
RUN apt-get update && \
    apt-get install -y --no-install-recommends cmake protobuf-compiler libprotobuf-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the full workspace context (.dockerignore trims target/, .git/, logs/).
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    set -eux; \
    RELEASE_FLAG=""; OUTPUT_DIR="debug"; \
    if [ "$BUILD_PROFILE" = "release" ]; then RELEASE_FLAG="--release"; OUTPUT_DIR="release"; fi; \
    if [ -n "$GEAR_FEATURES" ]; then \
      cargo build $RELEASE_FLAG --features "$GEAR_FEATURES" \
        --bin "$GEAR_BIN" --package "$GEAR_PACKAGE"; \
    else \
      cargo build $RELEASE_FLAG \
        --bin "$GEAR_BIN" --package "$GEAR_PACKAGE"; \
    fi; \
    cp "/build/target/$OUTPUT_DIR/$GEAR_BIN" /tmp/gear

# ---------------------------------------------------------------------------
# Stage 2: Runtime
# ---------------------------------------------------------------------------
FROM debian:13.3-slim

# Default gear config baked into the image; override by mounting a ConfigMap
# over /app/config/gear.yaml in Kubernetes.
ARG GEAR_CONFIG=config/oop-hello.yaml
# OoP REST + probes port; must match the gear's oop_http.listen_addr /
# chart service.port
ARG GEAR_PORT=9091

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /tmp/gear /app/gear
COPY --from=builder /build/${GEAR_CONFIG} /app/config/gear.yaml

# OoP REST + probes port (matches oop_http.listen_addr / chart service.port).
EXPOSE ${GEAR_PORT}

# Writable runtime state dir (non-root user has no home).
ENV APP__SERVER__HOME_DIR=/app/data

RUN useradd -U -u 1000 appuser && \
    mkdir -p /app/data && \
    chown -R 1000:1000 /app
USER 1000

CMD ["/app/gear", "--config", "/app/config/gear.yaml"]
