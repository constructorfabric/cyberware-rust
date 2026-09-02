---
title: Run a gear out-of-process
description: Run a gear as its own process that self-registers with the DirectoryService and is reached through the api-gateway edge.
sidebar:
  label: Run a gear out-of-process
  order: 10
---

A gear can run **in the host process** (linked into the platform host and resolved through
`ClientHub` as a direct call) or **out-of-process (OoP)** as its own pod. In the OoP model the
gear runs as an independent process, serves its REST surface locally, and self-registers with
the platform host's **DirectoryService**. The built-in **api-gateway** edge discovers the
gear's routes and reverse-proxies external traffic to it. Consumers of the gear's contract are
unaffected: they call the same SDK trait, resolved either in-process or over REST across pods.

This guide follows the minimal `hello` example (`examples/toolkit/hello/`) and the
`api-contracts` gear-to-gear example (`examples/toolkit/api-contracts/`).

## The gear is an ordinary REST gear

An OoP gear is just a normal gear that declares the `rest` capability. Routes it wants reachable
from outside the cluster are marked `.exposed()` so the edge proxies them; `.anonymous()` opts a
route out of bearer-token auth.

```rust title="examples/toolkit/hello/hello/src/gear.rs"
#[toolkit::gear(name = "hello", capabilities = [rest])]
#[derive(Default)]
pub struct Hello;

impl RestApiCapability for Hello {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: Router,
        openapi: &dyn OpenApiRegistry,
    ) -> Result<Router> {
        routes::register_routes(router, openapi)
    }
}
```

```rust title="examples/toolkit/hello/hello/src/api/rest/routes.rs"
let router = OperationBuilder::get("/hello/v1/ping")
    .operation_id("hello.ping")
    .exposed()      // publish at the api-gateway edge (reverse-proxied to this pod)
    .anonymous()    // no bearer token required
    .handler(handlers::handle_ping)
    .json_response_with_schema::<PingResponse>(openapi, StatusCode::OK, "Pong response")
    .register(router, openapi);
```

Nothing in the gear or its handlers knows it is running out-of-process — the same crate links
into the platform host unchanged for a Profile 1 (monolith) deployment.

## Give it a standalone binary

An OoP gear ships a small binary (gated behind an `oop_module` feature) that boots via
`run_oop_with_options`. That call loads config, initializes the gear, starts the OoP HTTP
server + probes, self-registers with the DirectoryService, and runs the normal gear lifecycle
until shutdown (deregistering on the way out).

```rust title="examples/toolkit/hello/hello/src/main.rs"
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = OopRunOptions {
        gear_name: "hello".to_owned(),
        config_path: cli.config,
        ..Default::default()
    };
    run_oop_with_options(opts).await
}
```

A sibling `registered_gears.rs` links the gear (and, for authenticated gears, the tenant-plane
authn stack) so its `#[toolkit::gear]` inventory registration is present in the binary.

## Configure the HTTP surface

The OoP HTTP lifecycle is driven by an `oop_http` block: the address the gear binds locally and
the URI it advertises to the DirectoryService (what the edge proxies to).

```yaml title="config/oop-hello.yaml"
oop_http:
  listen_addr: "127.0.0.1:9091"
  advertise_uri: "http://127.0.0.1:9091"
  allow_loopback_advertise: true

gears:
  hello:
    config: {}
```

The gear finds the platform host's DirectoryService via the `TOOLKIT_DIRECTORY_ENDPOINT`
environment variable (in Kubernetes this is the platform-host Service DNS).

## Discovery and the edge

The platform host runs the DirectoryService (via the grpc-hub) and the api-gateway edge with
its reverse proxy enabled:

```yaml title="config/oop-host.yaml (excerpt)"
gears:
  api-gateway:
    config:
      gateway_proxy:
        enabled: true
        directory_endpoint: "http://127.0.0.1:50051"
        sync_interval_secs: 5
  grpc-hub:
    config:
      listen_addr: "127.0.0.1:50051"
```

When the `hello` pod registers its REST endpoint, the edge picks up its `.exposed()` routes on
the next sync and forwards external `/hello/v1/ping` requests to the pod. Stop the pod and the
edge prunes the route within a poll.

## Gear-to-gear calls across pods

A consumer resolves another gear's contract with `#[toolkit::consumes]`. In-process this binds
to the local provider; across pods it becomes a directory-resolving REST client — no code
change. The `api-contracts` example splits a `PaymentApi` **provider** pod from a **consumer**
pod that calls it over REST:

```rust title="examples/toolkit/api-contracts/api-contracts-consumer (shape)"
#[toolkit::gear(name = "api-contracts-consumer", capabilities = [rest])]
#[toolkit::consumes(contract = api_contracts_sdk::PaymentApi, from = "api-contracts")]
pub struct ApiContractsConsumer;
```

The consumer resolves `PaymentApi` from the `ClientHub` and forwards the call to the provider
pod discovered through the DirectoryService.

## Run it locally

```bash
# 1. Platform host (edge :8087 + DirectoryService :50051)
cargo run -p cf-gears-platform-host -- --config config/oop-host.yaml run

# 2. The hello gear as its own process
TOOLKIT_DIRECTORY_ENDPOINT=http://127.0.0.1:50051 \
  cargo run -p hello --features oop_module --bin hello-oop -- --config config/oop-hello.yaml

# 3. Reach it through the edge (proxied) or directly (same served_by proves the proxy)
curl http://127.0.0.1:8087/hello/v1/ping
curl http://127.0.0.1:9091/hello/v1/ping
```

For Kubernetes (Profile 3), the same images are deployed via the Helm charts under
`deploy/helm/` (per-gear charts + the `toolkit-platform` umbrella).

## See also

- [Gears & composition](../../concepts/gears-and-composition/) — in-process vs out-of-process.
- Full code: `examples/toolkit/hello/` and `examples/toolkit/api-contracts/`.
- Kubernetes deployment: `deploy/helm/` and `deploy/docker/`.
