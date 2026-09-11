# Out-of-Process Gears and gRPC SDK Pattern

> **Transport note.** **REST is the default transport for out-of-process (OoP) gears** (see
> [ADR-0003](../adrs/toolkit/0003-toolkit-universal-lazy-layer.md)); gRPC as documented here is
> the **opt-in** transport. For the current REST-based model — independent pods that
> self-register with the DirectoryService and are reached through the api-gateway edge — see the
> how-to *Run a gear out-of-process* (`docs/web-docs/build-with-gears/out-of-process.md`) and the
> runnable examples `examples/toolkit/hello/` and `examples/toolkit/api-contracts/`. Gear-to-gear
> calls use `#[toolkit::consumes]` / `#[toolkit::provides]`, not the manual wiring shown below.
>
> This document remains the reference for the **shared OoP runtime** (`OopRunOptions`, lifecycle,
> shutdown) and the **gRPC SDK pattern** when gRPC is explicitly chosen.

ToolKit supports running gears as separate processes with gRPC-based inter-process communication. This enables process isolation, language flexibility, and independent scaling.

## Core invariants

- **Rule**: For OoP gears, use the SDK pattern with a single `*-sdk` crate containing API trait, types, gRPC client, and wiring helpers.
- **Rule**: For gRPC: server implementations live in the gear itself; the SDK crate provides only the client.
- **Rule**: For gRPC clients: always use `toolkit_transport_grpc::client` utilities (`connect_with_stack`, `connect_with_retry`).
- **Rule**: Use `CancellationToken` for coordinated shutdown across the entire process tree.

## RuntimeKind

Gears can run in two modes:

```rust
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    #[default]
    Local,  // In-process (default)
    Oop,    // Out-of-process
}
```

## Configuring an OoP process

An OoP gear runs as its own process and connects back to the platform host's DirectoryService.
Point it at that endpoint with the `TOOLKIT_DIRECTORY_ENDPOINT` environment variable, and drive
its local HTTP surface (REST + probes) with an `oop_http` block:

```yaml
oop_http:
  listen_addr: "127.0.0.1:9091"
  advertise_uri: "http://127.0.0.1:9091"

gears:
  my_gear:
    config: {}
```

> **Removed:** the legacy `runtime.type: oop` + `execution.executable_path` config — in which
> the host spawned the gear as a child process — is no longer used anywhere in the repo. OoP
> gears are deployed as independent processes/pods (see `deploy/helm/` and `deploy/docker/`).

## OoP Bootstrap Library

### Bootstrap entry point

```rust
use toolkit::bootstrap::oop::{OopRunOptions, run_oop_with_options};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = OopRunOptions {
        gear_name: "my_gear".to_string(),
        instance_id: None,  // Auto-generated UUID
        directory_endpoint: "http://127.0.0.1:50051".to_string(),
        config_path: None,
        verbose: 0,
        print_config: false,
        heartbeat_interval_secs: 5,
        version: None,
    };

    run_oop_with_options(opts).await
}
```

### OopRunOptions fields

| Field | Description |
|-------|-------------|
| `gear_name` | Logical gear name (e.g., "file-parser") |
| `instance_id` | Instance ID (defaults to random UUID) |
| `directory_endpoint` | DirectoryService gRPC endpoint |
| `config_path` | Path to configuration file |
| `verbose` | Log verbosity (0=default, 1=debug, 2=trace) |
| `print_config` | Print effective config and exit |
| `heartbeat_interval_secs` | Heartbeat interval (default: 5) |
| `version` | Gear version used for `DirectoryService` registration and the generated `OpenAPI` spec version; `None` means no explicit version |

## OoP Lifecycle

### Startup sequence

1. **Configuration loading** — loads config from file or `TOOLKIT_MODULE_CONFIG` env var
2. **Logging initialization** — sets up tracing with optional OTEL
3. **DirectoryService connection** — connects to the master host's directory service
4. **Instance registration** — registers with DirectoryService for discovery
5. **Heartbeat loop** — starts background heartbeat task
6. **Gear lifecycle** — runs the normal gear lifecycle (init → migrate → start)
7. **Graceful shutdown** — deregisters from DirectoryService on exit

### Shutdown model

Shutdown is driven by a single root `CancellationToken` per process:

- OS signals (SIGTERM, SIGINT, Ctrl+C) are hooked at bootstrap level
- The root token is passed to `RunOptions::Token` for gear runtime shutdown
- Background tasks (like heartbeat) use child tokens derived from the root
- On shutdown, the gear deregisters itself from DirectoryService before exiting

## SDK Pattern for OoP

### Gear structure with SDK

```
gears/my_gear/
  ├── my_gear-sdk/                # SDK for consumers (everything in one place)
  │   ├── Cargo.toml
  │   ├── build.rs                # Proto compilation
  │   ├── proto/
  │   │   └── my_gear.proto       # gRPC service definition
  │   └── src/
  │       ├── lib.rs              # Re-exports everything
  │       ├── api.rs              # API trait + types + errors
  │       ├── client.rs           # gRPC client impl (using toolkit-transport-grpc)
  │       └── wiring.rs           # wire_client() helper function
  └── my_gear/                    # Gear implementation + SERVER
      ├── Cargo.toml
      └── src/
          ├── lib.rs              # Gear definition, re-exports SDK
          ├── gear.rs             # Gear struct + traits
          ├── grpc_server.rs      # gRPC server implementation
          └── main.rs             # OoP binary entry point
```

### Key points

- The `-sdk` crate contains everything consumers need: API trait, types, gRPC client, and wiring helpers
- Server implementations are owned by the gear itself, not the SDK
- Consumers only need one dependency: `my_gear-sdk`

## SDK Crate Structure

### SDK `src/lib.rs`

```rust
#![forbid(unsafe_code)]

// API trait and types
mod api;
pub use api::{MyGearApi, MyGearError, Input, Output};

// gRPC proto stubs
pub mod proto {
    tonic::include_proto!("my_gear.v1");
}
pub use proto::my_gear_service_client::MyGearServiceClient;
pub use proto::my_gear_service_server::{MyGearService, MyGearServiceServer};

// gRPC client
mod client;
pub use client::MyGearGrpcClient;

// Wiring helpers
mod wiring;
pub use wiring::{wire_client, build_client};

/// Service name for discovery
pub const SERVICE_NAME: &str = "my_gear.v1.MyGearService";
```

### API Trait (in SDK)

```rust
// my_gear-sdk/src/api.rs
use async_trait::async_trait;
use uuid::Uuid;

/// API trait for MyGear
#[async_trait]
pub trait MyGearApi: Send + Sync {
    async fn do_something(&self, input: Input) -> Result<Output, MyGearError>;
}

/// Input type
#[derive(Debug, Clone)]
pub struct Input {
    pub id: Uuid,
    pub message: String,
}

/// Output type
#[derive(Debug, Clone)]
pub struct Output {
    pub result: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Error type
#[derive(thiserror::Error, Debug)]
pub enum MyGearError {
    #[error("Not found: {id}")]
    NotFound { id: Uuid },
    #[error("Validation error: {message}")]
    Validation { message: String },
    #[error("Internal error")]
    Internal,
}
```

### gRPC Client (in SDK)

```rust
// my_gear-sdk/src/client.rs
use crate::{api::MyGearApi, proto};
use async_trait::async_trait;
use toolkit_transport_grpc::client::{GrpcClient, GrpcClientExt};
use tonic::transport::Channel;

pub struct MyGearGrpcClient {
    inner: GrpcClient<proto::my_gear_service_client::MyGearServiceClient<Channel>>,
}

impl MyGearGrpcClient {
    pub fn new(channel: Channel) -> Self {
        Self {
            inner: GrpcClient::new(proto::my_gear_service_client::MyGearServiceClient::new(channel)),
        }
    }
}

#[async_trait]
impl MyGearApi for MyGearGrpcClient {
    async fn do_something(&self, input: crate::api::Input) -> Result<crate::api::Output, crate::api::MyGearError> {
        let request = proto::DoSomethingRequest {
            id: Some(input.id.to_string()),
            message: input.message,
        };

        let response = self
            .inner
            .call(|client| async move { client.do_something(request).await })
            .await
            .map_err(|e| crate::api::MyGearError::Internal)?;

        Ok(crate::api::Output {
            result: response.result,
            timestamp: chrono::DateTime::parse_from_rfc3339(&response.timestamp)
                .map_err(|_| crate::api::MyGearError::Internal)?
                .with_timezone(&chrono::Utc),
        })
    }
}
```

### Wiring helpers (in SDK)

```rust
// my_gear-sdk/src/wiring.rs
use crate::{MyGearApi, MyGearGrpcClient, SERVICE_NAME};
use toolkit_transport_grpc::client::{connect_with_stack, connect_with_retry};
use tonic::transport::Channel;

/// Wire a gRPC client with default stack
pub async fn wire_client(endpoint: &str) -> Result<Box<dyn MyGearApi>, Box<dyn std::error::Error>> {
    let channel = connect_with_stack(endpoint).await?;
    let client = MyGearGrpcClient::new(channel);
    Ok(Box::new(client))
}

/// Wire a gRPC client with retry logic
pub async fn build_client(
    endpoint: &str,
    max_retries: u32,
    retry_delay: std::time::Duration,
) -> Result<Box<dyn MyGearApi>, Box<dyn std::error::Error>> {
    let channel = connect_with_retry(endpoint, max_retries, retry_delay).await?;
    let client = MyGearGrpcClient::new(channel);
    Ok(Box::new(client))
}

/// Get service name for discovery
pub fn service_name() -> &'static str {
    SERVICE_NAME
}
```

## Gear Implementation

### gRPC Server (in gear)

```rust
// my_gear/src/grpc_server.rs
use crate::api::{MyGearApi, Input, Output, MyGearError};
use crate::proto::{my_gear_service_server::MyGearService, DoSomethingRequest, DoSomethingResponse};
use async_trait::async_trait;
use std::sync::Arc;
use tonic::{Request, Response, Status};

pub struct MyGearGrpcServer {
    service: Arc<dyn MyGearApi>,
}

impl MyGearGrpcServer {
    pub fn new(service: Arc<dyn MyGearApi>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl MyGearService for MyGearGrpcServer {
    async fn do_something(
        &self,
        request: Request<DoSomethingRequest>,
    ) -> Result<Response<DoSomethingResponse>, Status> {
        let req = request.into_inner();

        let input = Input {
            id: req.id.ok_or_else(|| Status::invalid_argument("id required"))?
                .parse()
                .map_err(|_| Status::invalid_argument("invalid id"))?,
            message: req.message,
        };

        match self.service.do_something(input).await {
            Ok(output) => {
                let response = DoSomethingResponse {
                    result: output.result,
                    timestamp: output.timestamp.to_rfc3339(),
                };
                Ok(Response::new(response))
            }
            Err(err) => Err(match err {
                MyGearError::NotFound { .. } => Status::not_found(err.to_string()),
                MyGearError::Validation { .. } => Status::invalid_argument(err.to_string()),
                MyGearError::Internal => Status::internal(err.to_string()),
            }),
        }
    }
}
```

### Gear registration with gRPC

```rust
// my_gear/src/gear.rs
#[toolkit::gear(
    name = "my_gear",
    capabilities = [stateful],
    client = my_gear_sdk::MyGearApi,
    lifecycle(entry = "serve", stop_timeout = "30s")
)]
pub struct MyGear {
    service: Arc<crate::domain::service::MyService>,
}

impl MyGear {
    pub fn new() -> Self {
        Self {
            service: Arc::new(crate::domain::service::MyService::new()),
        }
    }

    async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        // Create gRPC server
        let addr = "0.0.0.0:50051".parse()?;
        let grpc_server = MyGearGrpcServer::new(self.service.clone());

        // Start server
        let server_future = tonic::transport::Server::builder()
            .add_service(my_gear_sdk::proto::my_gear_service_server::MyGearServiceServer::new(grpc_server))
            .serve_with_shutdown(addr, cancel.cancelled());

        ready.notify();

        server_future.await?;
        Ok(())
    }
}
```

> The `client = ...` attribute validates the trait at compile time and exposes MODULE_NAME, but does not auto-register the client into ClientHub. You must still register it explicitly in your `init()` method using `ctx.client_hub().register::<dyn my_gear_sdk::MyGearApi>(client)`.

## Client Registration (in gear)

### Register both local and remote clients

```rust
// In gear's init()
async fn register_clients(&self, ctx: &GearCtx) -> anyhow::Result<()> {
    // Try local client first
    if let Ok(local_client) = ctx.client_hub().try_get::<dyn my_gear_sdk::MyGearApi>() {
        ctx.client_hub().register::<dyn my_gear_sdk::MyGearApi>(local_client);
        return Ok(());
    }

    // Fall back to remote client
    let endpoint = "http://127.0.0.1:50051";
    let remote_client = my_gear_sdk::wire_client(endpoint).await?;
    ctx.client_hub().register::<dyn my_gear_sdk::MyGearApi>(remote_client);

    Ok(())
}
```

## Testing OoP gears

### Test with mock server

```rust
#[tokio::test]
async fn test_grpc_client() {
    // Start mock server
    let mock_server = MockMyGearServer::new();
    let server_addr = mock_server.start().await;

    // Create client
    let client = my_gear_sdk::wire_client(&format!("http://{}", server_addr)).await.unwrap();

    // Test API
    let input = Input {
        id: Uuid::new_v4(),
        message: "test".to_string(),
    };

    let result = client.do_something(input).await.unwrap();
    assert!(!result.result.is_empty());
}
```

## Quick checklist

- [ ] Create `*-sdk` crate with API trait, types, gRPC client, and wiring helpers.
- [ ] Define `.proto` file and generate gRPC stubs in SDK.
- [ ] Implement gRPC server in gear crate.
- [ ] Use `toolkit_transport_grpc::client` utilities for connections.
- [ ] Register both local and remote clients in gear.
- [ ] Use `CancellationToken` for coordinated shutdown.
- [ ] Test with mock gRPC servers.
