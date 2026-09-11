# Requirements for a Gear to Run Out-of-Process (OoP) and Scale

This checklist captures every condition a gear must satisfy to run in the
Profile-3 OoP architecture - both to deploy as its own Kubernetes pod and to
scale beyond one replica.

There are **two axes**, and they are independent:

- **OoP readiness** (requirements 1–7) gets a gear into **one** pod.
- **Horizontal scaling** (requirement 8) gets a gear into **N** pods.

---

## Requirements

- [1. No hard-linked dependencies](#1-no-hard-linked-dependencies)
- [2. The gear must have an OoP binary](#2-the-gear-must-have-an-oop-binary)
- [3. Authenticated gears need an embedded tenant-plane authn stack](#3-authenticated-gears-need-an-embedded-tenant-plane-authn-stack)
- [4. Contract providers must be discoverable cross-pod](#4-contract-providers-must-be-discoverable-cross-pod)
- [5. There must be a standalone Helm chart](#5-there-must-be-a-standalone-helm-chart)
- [6. Database-backed gears need isolated storage](#6-database-backed-gears-need-isolated-storage)
- [7. Trust-coupled gears must migrate synthetic identities](#7-trust-coupled-gears-must-migrate-synthetic-identities)
- [8. Scalable gears must externalize state](#8-scalable-gears-must-externalize-state)

---

## 1. No hard-linked dependencies

This is the single most common blocker. Any form of hard coupling forces gears
into the same process.

What to eliminate:

- `#[toolkit::gear(deps = [...])]` at the gear-macro level.
- Direct `Cargo.toml` dependencies on another gear's implementation crate.
- Direct SQL reads/writes against another gear's tables.

What to do instead:

- Declare remote dependencies with `#[toolkit::consumes(contract = ..., from = "...")]`.
- Resolve the dependency lazily from `ClientHub`
  (`PolicyEnforcer::from_hub`, a resolving REST client, etc.).
- Let the provider expose a `#[toolkit::contract]` / `#[toolkit::rest_contract]`
  surface and advertise it in the DirectoryService.
- Route all cross-gear data access through the SDK contract.

**Example:**

```rust
// src/gear.rs - declare the remote dependency with a standalone attribute on
// the gear struct.
#[toolkit::consumes(contract = some_gear_sdk::SomeApi, from = "some-gear")]
pub struct MyGear { ... }

// Resolve it lazily from the ClientHub (works whether the provider is local or
// in another pod). The macro registers a resolving client for the trait.
let client = ctx.client_hub().get::<dyn some_gear_sdk::SomeApi>()?;
```

Until this is true, the gear literally cannot run in a separate process.

## 2. The gear must have an OoP binary

The gear crate needs a feature-gated `[[bin]]` target that enables the bootstrap
runtime. The runtime also stands up the gear's own HTTP server, so serving REST
out-of-process is a property of this binary, not a separate requirement.

- `Cargo.toml`: `oop_module = ["dep:tokio", "toolkit/bootstrap"]`,
  `k8s-auth = ["toolkit/k8s-auth"]` (the platform-plane feature - wired in
  [requirement 5](#5-there-must-be-a-standalone-helm-chart)), and a `[[bin]]`
  with `required-features = ["oop_module"]`.
- `src/main.rs`: call `toolkit::bootstrap::oop::run_oop_with_options(...)`.
- `src/registered_gears.rs`: link the gear crate with `use <crate> as _;`.
- Routes: register them from `register_rest()` and mark any edge-reachable route
  `.exposed()` - required for both `.authenticated()` and `.anonymous()` routes.
- Container image: build with `oop_module` and `k8s-auth` enabled
  (`deploy/docker/oop-gear.Dockerfile` takes these as `GEAR_FEATURES`; use
  `BUILD_PROFILE=release` for production).

**Example:**

```toml
# Cargo.toml
[features]
oop_module = ["dep:tokio", "toolkit/bootstrap"]
k8s-auth = ["toolkit/k8s-auth"]

[[bin]]
name = "my-gear-oop"
required-features = ["oop_module"]
```

```rust
// src/main.rs
use toolkit::bootstrap::oop::{OopRunOptions, run_oop_with_options};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = OopRunOptions {
        gear_name: "my-gear".to_owned(),
        config_path,
        verbose,
        version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        ..Default::default()
    };
    run_oop_with_options(opts).await
}
```

```yaml
# config/oop-my-gear.yaml - the bootstrap runtime serves REST here and
# advertises it to the edge.
oop_http:
  listen_addr: "0.0.0.0:9090"
  advertise_uri: "http://my-gear:9090"
```

## 3. Authenticated gears need an embedded tenant-plane authn stack

The OoP pod has no gateway security-context middleware; it must reconstruct
`SecurityContext` from the bearer token itself.

- Add `authn-resolver` + your production authn plugin (e.g. `oidc-authn-plugin`,
  or `static-authn-plugin` only for non-production acceptance testing) +
  `types-registry` as optional deps behind `oop_module`.
- Wire them in `registered_gears.rs` so the OoP binary links them.
- `types-registry` is required because authn-resolver discovers its plugins
  (static, OIDC, etc.) through the GTS registry. In host mode that registry is
  shared; in OoP mode the gear must embed a local one so the configured authn
  plugin can register itself and authn-resolver can resolve it.
- **Future state:** once `types-registry` runs OoP as its own pod, the tenant
  authn stack will consume it remotely instead of embedding it. Until then,
  every authenticated OoP gear must carry its own copy.
- Middleware re-validates the JWT via `AuthNResolverClient` and builds a
  `SecurityContext` for every tenant-scoped route.
- Generated REST clients forward the original bearer token automatically, so
  tenant context propagates across gear-to-gear calls.
- System/background jobs acting inside a tenant must obtain a real S2S
  client-credentials JWT (`exchange_client_credentials()`), not a synthetic or
  anonymous `SecurityContext`.
- Anonymous gears skip this requirement.

**Context:** `SecurityContext` is the *tenant-plane* identity, carried by
`Authorization: Bearer <jwt>`. Use it for user requests, gear-to-gear tenant
calls, and S2S tenant-scoped jobs. It is never used for platform-level calls
(DirectoryService, GTS registration, heartbeats) - those use the platform-plane
`PlatformSecurityContext` wired in
[requirement 5](#5-there-must-be-a-standalone-helm-chart). `x-secctx-bin` is not
used over HTTP in OoP.

**Example:**

```toml
# Cargo.toml
[features]
oop_module = [
  "dep:tokio",
  "toolkit/bootstrap",
  "dep:authn-resolver",
  "dep:oidc-authn-plugin",      # or static-authn-plugin for testing
  "dep:types-registry",
]
k8s-auth = ["toolkit/k8s-auth"]
```

```rust
// src/registered_gears.rs
use authn_resolver as _;
use oidc_authn_plugin as _;   // or static_authn_plugin for testing
use types_registry as _;
```

## 4. Contract providers must be discoverable cross-pod

Applies only to gears that expose a contract consumed by other gears.

- Declare the contract with `#[toolkit::rest_contract]` and include OpenAPI
  metadata.
- Register `rest_endpoint` + `openapi_spec` in the DirectoryService. OoP
  bootstrap does this automatically once the contract is wired.

**Example:**

```rust
// SDK crate that defines the contract.
#[toolkit::rest_contract(base_path = "/my-gear/v1")]
pub trait MyApiRest: MyApi {
    #[post("/do-thing")]
    async fn do_thing(
        &self,
        ctx: SecurityContext,
        req: DoThingRequest,
    ) -> Result<DoThingResponse, CanonicalError>;
}
```

```rust
// Consumer gear declares the dependency with the standalone `consumes`
// attribute.
#[toolkit::consumes(contract = my_gear_sdk::MyApi, from = "my-gear")]
pub struct ConsumerGear { ... }
```

## 5. There must be a standalone Helm chart

The gear needs a deployable unit. This is also where **platform-plane auth** is
wired - the deployment side of the `k8s-auth` build feature from
[requirement 2](#2-the-gear-must-have-an-oop-binary). Every DirectoryService
caller and receiver must authenticate gRPC traffic with `X-ToolKit-Internal-Token`,
or the platform-host rejects registration.

- Base the chart on `deploy/helm/toolkit-common` for Deployment/Service/ConfigMap
  /SA projection.
- Set `directoryEndpoint` to the platform-host's grpc-hub Service DNS.
- Configure `oop_http` with the gear's listen port and `advertise_uri`.
- **Platform-plane auth:**
  - Point the gear at its projected token
    (`oop_http.internal_auth.provider: kube`, `token_path: ...`).
  - Project a ServiceAccount token with audience `toolkit-internal`.
  - Configure the platform-host `grpc-hub` to enforce internal auth
    (`provider: kube`, `internal_auth_enforcement: required`,
    `audiences: [toolkit-internal]`).
  - `PlatformSecurityContext` (carried by that token) is the platform-plane
    identity: use it for DirectoryService registration, GTS, and heartbeats -
    never for tenant policy, which is the tenant-plane `SecurityContext` of
    [requirement 3](#3-authenticated-gears-need-an-embedded-tenant-plane-authn-stack).
- Add the chart as a dependency of the umbrella `toolkit-platform` chart.

**Example:**

```text
deploy/helm/my-gear/
├── Chart.yaml            # depends on toolkit-common
├── values.yaml           # image, oop_http (+ internal_auth), directoryEndpoint, postgres
└── templates/            # uses toolkit-common templates
```

```yaml
# deploy/helm/my-gear/values.yaml - gear side of the platform plane.
oop_http:
  internal_auth:
    provider: kube
    token_path: /var/run/secrets/tokens/toolkit-internal/token
```

```yaml
# deploy/helm/platform-host/values.yaml - enforcement side of the platform plane.
grpc-hub:
  internal_auth:
    provider: kube
    audiences: [toolkit-internal]
  internal_auth_enforcement: required
```

```yaml
# umbrella values (production example)
my-gear:
  enabled: true
  fullnameOverride: my-gear
  image:
    repository: ghcr.io/constructorfabric/my-gear
    tag: "1.0.0"
    pullPolicy: IfNotPresent
  directoryEndpoint: "http://platform-host:50051"
  postgres:
    enabled: true
    host: shared-postgres
    database: mygear
    user: platform
    # Pull the password from a Kubernetes secret in production.
    password: "${POSTGRES_PASSWORD}"
```

## 6. Database-backed gears need isolated storage

Applies only to gears that persist data.

- In Kubernetes, use the shared Postgres with its own database
  (`postgres.databases` in the umbrella values).
- Locally, use a separate SQLite file per gear.
- The gear crate must be able to run its migrations against that isolated
  database.
- Never read or write another gear's tables directly.

**Example:**

```yaml
# umbrella values
postgres:
  databases:
    - mygear
    - othergear
```

```yaml
# local development config (do not use SQLite in production)
database:
  uri: "sqlite:///app/data/my-gear.db?mode=rwc"
```

## 7. Trust-coupled gears must migrate synthetic identities

Applies only to the trust-coupled core. Requirements 1–6 are necessary but **not
sufficient** for these gears: even with contracts and clean `consumes` wiring,
they cannot cross a network boundary while any hop uses a synthetic or anonymous
identity, because that identity has no meaning off-process.

What to eliminate:

- `SecurityContext::anonymous()` used to carry an *internal* call across the
  authz→tenant-resolver→resource-group chain.
- Synthetic system actors (e.g. `account-management`'s `am.system`) used for
  background flows.
- Any assumption that a downstream gear trusts an upstream simply because they
  share a process.

What to do instead:

- Replace synthetic/anonymous internal calls with real IdP-issued **S2S
  client-credentials** JWTs (`exchange_client_credentials()`), scoped to the
  operation, validated by the callee's tenant-plane authn stack (requirement 3).
- Keep platform-plane calls (DirectoryService, GTS, heartbeats) on
  `PlatformSecurityContext` / `X-ToolKit-Internal-Token` (requirement 5) - do
  not conflate the two planes.

Until this is done, the gear is **structurally** in `platform-host`, regardless
of how many other requirements it meets. This is the only blocker not solvable
by "add a contract".

## 8. Scalable gears must externalize state

Requirements 1–7 get a gear into **one** pod. This one gets it into **N** pods.
A gear can be fully OoP-ready and still be pinned to a single replica because its
only store is in-process memory or a local disk - a second replica would then
diverge.

Pick the weakest strategy that holds:

- **Externalize** - put durable state in the shared backing service (Postgres,
  or an object store like S3 for blobs). The default.
- **Rebuildable** - keep state in memory but repopulate it per replica (from
  re-registration/heartbeat, or compiled-in inventory + config) so each replica
  converges on the same truth.
- **Leader-gate** - scale the request path statelessly on a shared store, and
  wrap any singleton background work (sweeps, outbox drains) in a `LeaderElector`
  so exactly one replica runs it.
- **Declare singleton** - if the gear *is* a coordination primitive or the
  discovery seed, cap it at one / leader-elected and label it as such.

## Notes for special cases

- **Plugins** (`static-authn-plugin`, `static-authz-plugin`, etc.) are always
  embedded in their parent gear's process and inherit that gear's OoP status.
  They are not independently assessed.
- **Anonymous gears** skip the tenant-plane authn stack (requirement 3) and the
  identity migration (requirement 7); they need 1 and 2, plus 4/5/6 as
  applicable.
- **Trust-coupled core** gears are the only ones subject to requirement 7;
  every other category skips it.
- **Requirement 8 (scaling)** cuts across all categories and is *orthogonal* to
  OoP readiness - it applies to any gear meant to run >1 replica.
