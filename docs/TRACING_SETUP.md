# Telemetry Setup

This guide covers the **OpenTelemetry** setup shared by every gear: distributed
tracing and metrics.

Both signals speak **OTLP only** (gRPC or HTTP/protobuf) and are **pushed** to a
collector — there is no `/metrics` scrape endpoint and no vendor-specific
exporter. Any OTLP-compatible backend works: the OpenTelemetry Collector,
Jaeger, Uptrace, or the Datadog Agent.

**Logs are not exported over OTLP.** They are written to stderr as JSON and
collected from there — see [Logs](#logs) below.

## Overview

- **Automatic trace-context extraction** from incoming HTTP requests (W3C Trace Context)
- **Automatic trace-context injection** into outgoing HTTP requests
- **OTel metrics** through a global `SdkMeterProvider`; gears only declare instruments
- **Centralized configuration** in one `opentelemetry:` YAML block
- **Graceful flush** of both signals on shutdown
- **Log correlation** — `trace_id`/`span_id` in the JSON log records

> **Config key.** Everything lives under the top-level `opentelemetry:` key.
> Config structs use `#[serde(deny_unknown_fields)]`, so a misspelled or
> misplaced key is a hard load error, not a silently ignored setting.

---

## Quick start with Jaeger (traces only)

```bash
docker run -d --name jaeger \
  -p 16686:16686 \
  -p 4317:4317 \
  -p 4318:4318 \
  -e COLLECTOR_OTLP_ENABLED=true \
  jaegertracing/all-in-one:latest
```

```yaml
server:
  home_dir: "~/.cf-gears"
  host: "127.0.0.1"
  port: 8087

opentelemetry:
  resource:
    service_name: "cf-gears-api"
    attributes:
      service.version: "1.0.0"
      deployment.environment: "dev"

  exporter:
    kind: "otlp_grpc"
    endpoint: "http://127.0.0.1:4317"
    timeout_ms: 5000

  tracing:
    enabled: true
    sampler:
      parent_based_ratio:
        ratio: 1.0

logging:
  default:
    console_level: "info"
```

```bash
cargo run --bin cf-gears-server -- --config config/with-tracing.yaml
```

Traces appear at <http://localhost:16686> under service `cf-gears-api`.

---

## Quick start with Datadog

Datadog ingests OTLP through the **Datadog Agent**, so no gear-side change is
needed beyond configuration.

### Locally

```bash
export DD_API_KEY=...
export DD_SITE=datadoghq.com          # or datadoghq.eu
docker compose -f testing/docker/docker-compose.observability.yml up -d
```

That starts an OpenTelemetry Collector on `:4317`/`:4318` which forwards traces
and metrics to Datadog (see `testing/docker/otel-collector-datadog.yaml`).

### In Kubernetes

Enable the OTLP receiver on the Datadog Agent DaemonSet:

```
DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT=0.0.0.0:4317
```

and point each pod at its own node:

```yaml
env:
  - name: HOST_IP
    valueFrom:
      fieldRef:
        fieldPath: status.hostIP
  - name: APP__OPENTELEMETRY__EXPORTER__ENDPOINT
    value: "http://$(HOST_IP):4317"
```

### Unified service tagging

Datadog derives `service`, `env`, and `version` from OTel resource attributes:

| Resource attribute                     | Datadog tag |
|----------------------------------------|-------------|
| `resource.service_name`                | `service`   |
| `attributes.deployment.environment`    | `env`       |
| `attributes.service.version`           | `version`   |

Set all three — without `env` and `version`, APM, metrics, and logs will not
correlate into one service view.

---

## Configuration reference

The full surface, with every key shown:

```yaml
opentelemetry:
  # Resource identity — attached to all traces and metrics.
  resource:
    service_name: "my-service"
    attributes:
      service.version: "1.2.3"
      deployment.environment: "production"
      service.namespace: "cf-gears"
      k8s.cluster.name: "prod-cluster"

  # Default exporter, shared by both signals.
  # A per-signal `exporter` block fully replaces this one.
  exporter:
    kind: "otlp_grpc"                 # otlp_grpc (4317) | otlp_http (4318)
    endpoint: "http://127.0.0.1:4317" # plaintext only for a loopback collector
    timeout_ms: 5000
    # Backend auth. Credential-bearing headers require an https:// endpoint —
    # over plaintext OTLP they travel in the clear to anything on the path.
    headers:
      authorization: "Bearer token"

  tracing:
    enabled: true
    sampler:
      parent_based_ratio:             # parent_based_always_on | parent_based_ratio
        ratio: 0.1                    # always_on | always_off
    exporter:                         # optional per-signal override
      kind: "otlp_grpc"
      endpoint: "http://127.0.0.1:14317"

  metrics:
    enabled: true
    cardinality_limit: 2000           # optional; SDK default when omitted
```

### Samplers

`always_on: {}`, `always_off: {}`, `parent_based_always_on: {}`, or
`parent_based_ratio: { ratio: 0.1 }`. The default when `sampler` is omitted is
`parent_based_always_on`; `parent_based_ratio` with no `ratio` defaults to `0.1`.

### Signals are independent

Each of `tracing` and `metrics` has its own `enabled` flag, defaulting to
**`false`**. A gear with neither enabled emits no telemetry at all and does not
even install the W3C propagator. Every config shipped in `config/` has them off
— enable them deliberately.

### Migrating from the old `tracing:` block

Before 2026-03 the settings lived in a top-level `tracing:` section. That form
is no longer accepted: loading a config that still uses it fails with an error
naming each key's new home. The same applies to `APP__TRACING__*` environment
overrides, which are now `APP__OPENTELEMETRY__*`.

| Old | New |
|---|---|
| `tracing.enabled` | `opentelemetry.tracing.enabled` |
| `tracing.service_name` | `opentelemetry.resource.service_name` |
| `tracing.resource` | `opentelemetry.resource.attributes` |
| `tracing.metrics` | `opentelemetry.metrics` |
| `tracing.exporter` | `opentelemetry.exporter`, or `opentelemetry.tracing.exporter` to override it for traces only |
| `tracing.sampler`, `.propagation`, `.http`, `.logs_correlation` | `opentelemetry.tracing.*` |

### Not yet implemented

`tracing.propagation` and `tracing.http` are accepted by the config parser but
**read by no code**. W3C propagation is always on when tracing is enabled,
regardless of `propagation.w3c_trace_context`.

---

## Logs

Logs do **not** travel over OTLP. Each gear writes to stderr and, when a `file:`
sink is configured, to rotating files — both governed by the `logging:` block,
which is independent of `opentelemetry:`.

For a collector or agent to pick them up, emit JSON:

```yaml
logging:
  default:
    console_format: "json"     # text (default) | json
    console_level: info
```

The Datadog Agent's container log collection, or an OpenTelemetry Collector
`filelog` receiver, then reads the container's log stream — the runtime captures
stderr and stdout together, so the console sink is picked up as it is. Nothing
needs to be enabled inside the process.

### Correlating logs with traces

For a backend to link a log line to the span that produced it, the record has
to carry the ids. Enable:

```yaml
opentelemetry:
  tracing:
    logs_correlation:
      inject_trace_ids_into_logs: true
```

With this on, every JSON record emitted inside a sampled span gains top-level
`trace_id` and `span_id` fields, in the same lowercase-hex form as the
`traceparent` header. Records emitted outside a span are unchanged.

The flag costs a context lookup per event, so it is off by default and the
stock formatter is used unless it is enabled.

## Environment variable overrides

Any key can be overridden through the generic `APP__` figment layer, with `__`
as the nesting separator:

```bash
export APP__OPENTELEMETRY__TRACING__ENABLED=true
export APP__OPENTELEMETRY__METRICS__ENABLED=true
export APP__OPENTELEMETRY__RESOURCE__SERVICE_NAME=cf-gears-prod
export APP__OPENTELEMETRY__EXPORTER__KIND=otlp_grpc
export APP__OPENTELEMETRY__EXPORTER__ENDPOINT=http://collector:4317
```

The only standard OpenTelemetry variable honoured is
**`OTEL_EXPORTER_OTLP_HEADERS`** (`k=v,k2=v2`), merged over any headers from the
config file. `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME`, and
`OTEL_RESOURCE_ATTRIBUTES` are **not** read.

---

## Cargo features

The toolkit enables `otel` by default, so telemetry bootstrap is always present.
Two features are **not** default and silently remove outbound instrumentation
when missing:

```toml
[dependencies]
toolkit-http = { workspace = true, features = ["otel"] }      # .with_otel()
toolkit-contract = { workspace = true, features = ["otel"] }  # generated clients
```

Without `toolkit-http/otel` the `with_otel()` method does not exist and the
build fails; without `toolkit-contract/otel` generated clients compile fine but
drop out of the trace.

---

## Instrumented HTTP clients

```rust,ignore
use toolkit_http::HttpClient;
use std::time::Duration;

let client = HttpClient::builder()
    .with_otel()                       // span + W3C traceparent injection
    .with_metrics("payments")          // http.client.request.duration
    .timeout(Duration::from_secs(30))
    .build()?;

let data = client
    .get("https://api.example.com/data")
    .send()
    .await?
    .checked_bytes()
    .await?;
```

## Metrics in a gear

Instruments come from the global provider installed by the bootstrap. A gear
must **not** build its own exporter or provider, and must not expose a
`/metrics` endpoint:

```rust,ignore
let meter = opentelemetry::global::meter_with_scope(scope);
let counter = meter.u64_counter("my_gear_operation_total").build();
```

The established pattern is a metrics port in `domain/ports/` with an OTel
adapter in `infra/metrics.rs`.

## Manual spans

```rust,ignore
use tracing::{info_span, Instrument, info};

async fn process_user_data(user_id: u64) -> anyhow::Result<()> {
    async {
        info!("Processing user");
        // …
        Ok(())
    }
    .instrument(info_span!("process_user", user.id = user_id))
    .await
}
```

Or declaratively:

```rust,ignore
#[tracing::instrument(skip(self, ctx), fields(user_id = %id))]
pub async fn get_user(&self, ctx: &SecurityContext, id: Uuid) -> Result<User, DomainError> {
    tracing::debug!("Getting user by id");
    // …
}
```

---

## Troubleshooting

**No data at all.** Check that the relevant `enabled` flag is `true` — both
default to `false`. On startup the bootstrap emits a `startup_check` span and
runs a connectivity probe; look for `OpenTelemetry tracing initialized`,
and `OpenTelemetry metrics initialized successfully`.

**Traces stop at a service boundary.** Outbound HTTP needs `.with_otel()` and
the `toolkit-http/otel` feature. Note that **gRPC hops do not propagate trace
context yet**, so an internal gRPC call currently starts a new, unlinked trace.

**Config file fails to load.** `deny_unknown_fields` rejects any key it does not
recognise. The block is `opentelemetry:`, not `tracing:`.

**Data missing right after a restart.** Telemetry is flushed by
`bootstrap::run::tracing_shutdown()` during graceful shutdown. A process killed
with `SIGKILL` loses whatever is still batched.

**Performance.** Use `parent_based_ratio` in production (`0.01` = 1%). Export is
batched on a background task. Consider `metrics.cardinality_limit` for
instruments with unbounded attribute values.

---

## Best practices

Use consistent, low-cardinality attribute names, and keep high-cardinality
values (ids, keys, names) on spans rather than on metric attributes:

```rust,ignore
tracing::info_span!(
    "user_operation",
    user.id = user_id,
    operation.type = "create",
);
```

Record failures on the span so the backend can flag it:

```rust,ignore
match risky_operation().await {
    Ok(result) => Ok(result),
    Err(e) => {
        tracing::error!(error = %e, "risky_operation failed");
        Err(e)
    }
}
```
