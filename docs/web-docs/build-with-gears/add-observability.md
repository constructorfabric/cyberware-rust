---
title: Add observability
description: OpenTelemetry traces and metrics, JSON logs, request IDs, health endpoints, and instrumented HTTP clients.
sidebar:
  label: Add observability
  order: 9
---

Gears ship a common operational surface: distributed tracing, metrics, structured
logs, request-id propagation, health endpoints, and instrumented outbound HTTP —
configured centrally, so every gear behaves the same. This guide covers the knobs (see
`docs/TRACING_SETUP.md` in the framework repo for the full reference).

## Configure telemetry

Traces and metrics are configured in one `opentelemetry:` block and pushed over
OTLP. Point it at any OTLP-compatible backend (the OpenTelemetry Collector,
Jaeger, Uptrace, the Datadog Agent, …). Logs are not sent over OTLP — see
[Logs](#logs) below:

```yaml
opentelemetry:
  resource:
    service_name: "cf-gears-api"
    attributes:
      service.version: "1.0.0"
      deployment.environment: "dev"
  exporter:
    kind: "otlp_grpc"            # or "otlp_http"
    endpoint: "http://127.0.0.1:4317"
    timeout_ms: 5000
  tracing:
    enabled: true
    sampler:
      parent_based_ratio:
        ratio: 0.1               # 10% — use always_on in dev, a ratio in prod
  metrics:
    enabled: true
```

Samplers: `always_on`, `always_off`, `parent_based_always_on`, or
`parent_based_ratio`. Exporters: `otlp_grpc` (port 4317) or `otlp_http`
(port 4318).

:::caution
Each signal defaults to **disabled**, and every config shipped in `config/` has
both off — enable them deliberately. The config structs also use
`deny_unknown_fields`, so an unrecognised key is a hard load error.
:::

Settings can be overridden by environment variables using the `APP__` prefix
with `__` between levels, e.g. `APP__OPENTELEMETRY__EXPORTER__ENDPOINT=...`.

Spin up a local collector to view traces:

```sh
docker run -d --name jaeger -p 16686:16686 -p 4317:4317 -p 4318:4318 \
  -e COLLECTOR_OTLP_ENABLED=true jaegertracing/all-in-one:latest
# UI at http://localhost:16686
```

For a Datadog-backed local setup, see
`testing/docker/docker-compose.observability.yml` in the framework repository.

## Logs

Logs go to stderr, and to rotating files when a `file:` sink is configured —
both governed by the `logging:` block, which is separate from `opentelemetry:`.
Emit JSON so a collector or agent can read them off the container's log stream:

```yaml
logging:
  default:
    console_format: "json"     # text (default) | json
    console_level: info
```

To let a backend link a log line to the span that produced it, add the trace ids
to the records:

```yaml
opentelemetry:
  tracing:
    logs_correlation:
      inject_trace_ids_into_logs: true
```

Every JSON record emitted inside a sampled span then carries top-level
`trace_id` and `span_id`.

## Spans, request IDs, and health — for free

With tracing enabled the gateway and toolkit give you, without per-gear wiring:

- a **span per request** (method, route, `request_id`, `trace_id`);
- **W3C trace-context** propagation in and out (`traceparent`);
- a **request id** correlated across logs and injected as a response header
  (`inject_request_id_header`);
- health endpoints **`/healthz`** (liveness, shallow "ok"), **`/readyz`** (readiness,
  200/503), and **`/health`** (detailed per-component JSON) exposed by the API Gateway. All
  three are public (no auth) and run on the gateway's middleware surface.

### Health serving modes

`api-gateway` config controls where the probes are served:

- `health.serve: main` (default) — probes ride the main listener, sharing `prefix_path`
  (e.g. `/cf/healthz`).
- `health.serve: separate` — probes are served only on a dedicated listener; set
  `health.bind_addr` (e.g. `0.0.0.0:8081`) and expect the paths **unprefixed** on that port.
- `health.serve: both` — served on both. `bind_addr` is required for `separate`/`both`.

Register one composite `Healthcheck` per gear (see `RestApiCapability::healthcheck`); report
external SaaS/LLM dependencies as `degraded` rather than gating readiness on them.

## Instrument your own code

Add spans with the `tracing` macros. Handlers and service methods in the examples use
`#[tracing::instrument]` with structured fields:

```rust
#[tracing::instrument(skip(self, ctx), fields(user_id = %id))]
pub async fn get_user(&self, ctx: &SecurityContext, id: Uuid) -> Result<User, DomainError> {
    tracing::debug!("Getting user by id");
    // …
}
```

## Trace outbound HTTP

Build outbound clients through `toolkit-http` with `.with_otel()` (enable the `otel` feature)
so external calls join the trace and carry the `traceparent` header. Note that `otel` is
**not** a default feature of `toolkit-http` or `toolkit-contract` — without it outbound calls
silently drop out of the trace:

```rust
let client = HttpClient::builder()
    .with_otel()
    .timeout(Duration::from_secs(30))
    .build()?;

let bytes = client.get("https://api.example.com/data").send().await?.checked_bytes().await?;
```

:::note[Initialization]
Telemetry is initialized by the server bootstrap from the `opentelemetry:` config — a
standalone server does this in `run_server(config)`. The bootstrap also flushes both
signals on graceful shutdown. The exact init entry point is part of the bootstrap, not
something gears call directly.
:::

## See also

- [Runtime & lifecycle](../../concepts/runtime-and-lifecycle/) — where background work and
  cancellation fit.
- Full reference: `docs/TRACING_SETUP.md` in the framework repository — covers both
  signals, logs, the Datadog setup, and unified service tagging.
