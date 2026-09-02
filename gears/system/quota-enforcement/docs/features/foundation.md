<!-- Created: 2026-08-24 by Constructor Tech -->

# Feature: Gear Foundation, Storage & Coordination

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-featstatus-foundation-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-quota-enforcement-feature-foundation`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Gear Bootstrap and Readiness](#gear-bootstrap-and-readiness)
  - [Authorized Operation Admission](#authorized-operation-admission)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Two-Phase PDP Scope Enforcement](#two-phase-pdp-scope-enforcement)
  - [Bounded-Cardinality Telemetry Emission](#bounded-cardinality-telemetry-emission)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [SDK Contract Crate](#sdk-contract-crate)
  - [Reference Storage Plugin on toolkit-db](#reference-storage-plugin-on-toolkit-db)
  - [Gateway Admission and Tenant Isolation](#gateway-admission-and-tenant-isolation)
  - [Workspace and Crate Skeletons](#workspace-and-crate-skeletons)
  - [Cluster Coordination Adapter](#cluster-coordination-adapter)
  - [Telemetry Conventions](#telemetry-conventions)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Stands up the `quota-enforcement` gear and SDK crates, the storage plugin contract with the storage-plugin skeleton
whose trait implementation completes as later features add their primitives, the coordination adapter over the
platform `cluster` gear's leader election, plus gear bootstrap, two-phase PDP authorization, tenant isolation, and the
telemetry conventions every later feature emits through.

### 1.2 Purpose

Every other Quota Enforcement feature calls a storage primitive, runs behind the Gateway's authorization, writes
tenant-scoped rows, or emits telemetry. This feature delivers those seams once, as contracts, so later features add
behavior without renegotiating infrastructure.

**Scope**: the storage SDK plugin contract, the `cluster-sdk` dependency with the typed `quota-enforcement` profile
and the coordination adapter, the storage-plugin crate skeleton and its foundation-table functions, gear bootstrap
with idempotent seeding and startup validation, two-phase PDP admission, tenant isolation at both layers, and the
telemetry conventions.

**Out of scope**: projection-catalogue publication and its consistency checks (projection-contracts feature), every
evaluation/lease/notification behavior and the storage primitives they add (their owning features), and the
`LeaseSweeper`/`RetentionSweeper` singletons that consume the coordination adapter delivered here, and the
notification dispatch that runs under the `toolkit-db` Outbox lease instead.

**Requirements**: `cpt-cf-quota-enforcement-fr-pluggable-storage`, `cpt-cf-quota-enforcement-fr-authorization`,
`cpt-cf-quota-enforcement-fr-tenant-isolation`, `cpt-cf-quota-enforcement-fr-telemetry`,
`cpt-cf-quota-enforcement-nfr-authentication`, `cpt-cf-quota-enforcement-nfr-authorization`,
`cpt-cf-quota-enforcement-nfr-tenant-isolation-integrity`

**Principles**: `cpt-cf-quota-enforcement-principle-fail-closed`, `cpt-cf-quota-enforcement-principle-storage-pluggable`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-quota-enforcement-actor-platform-operator` | Deploys the gear, selects the active storage plugin, configures defaults |
| `cpt-cf-quota-enforcement-actor-storage-backend` | Persists all gear state behind `QuotaEnforcementStoragePluginV1` |
| `cpt-cf-quota-enforcement-actor-authz-resolver` | Answers every admission query in the two-phase PDP integration |
| `cpt-cf-quota-enforcement-actor-monitoring-system` | Scrapes the gear-specific telemetry surface |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md)
- **Dependencies**: None

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-quota-enforcement-usecase-debit` (admission prefix only; the debit body is owned by the
consumption-operations feature)

### Gear Bootstrap and Readiness

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-gear-bootstrap`

**Actor**: `cpt-cf-quota-enforcement-actor-platform-operator`

**Success Scenarios**:
- The gear starts with a compatible schema, seeded defaults, reachable dependencies, and reports ready

**Error Scenarios**:
- Incompatible storage schema fails readiness with `SchemaVersionMismatch`
- No storage plugin resolves, or the `quota-enforcement` cluster profile is unbound or lacks a linearizable
  election: the gear refuses to serve
- PDP unreachable at startup: readiness fails (fail-closed)

**Steps**:
1. [x] - `p1` - Operator starts the gear with a configuration naming exactly one active storage plugin - `inst-boot-start`
2. [x] - `p1` - DB: `bootstrap()` verifies the installed schema matches the plugin contract major version - `inst-boot-schema`
3. [x] - `p1` - **IF** schema version is incompatible - `inst-boot-schema-if`
   1. [x] - `p1` - Abort readiness with `SchemaVersionMismatch`; serve nothing - `inst-boot-schema-abort`
4. [x] - `p1` - DB: seed default config rows (`contention_timeout_config`, `lease_capacity_config`, `idempotency_retention_config`) when missing - `inst-boot-seed-config`
5. [x] - `p1` - API: resolve the cluster leader-election facade for the `quota-enforcement` profile with the linearizable requirement; the cluster resolver validates the operator's backend binding - `inst-boot-cluster-resolve`
6. [x] - `p1` - API: verify `authz-resolver` reachability with one bounded PDP evaluation round trip; any decision proves the PDP answered, a transport error or the deadline fails the probe - `inst-boot-pdp-probe`
7. [x] - `p1` - **IF** any probe or the cluster resolve fails - `inst-boot-probe-if`
   1. [x] - `p1` - Fail readiness and surface the failing dependency in the health endpoint - `inst-boot-probe-abort`
8. [x] - `p1` - Register REST routes into the platform `api-gateway` via ToolKit typed-operation registration - `inst-boot-rest`
9. [x] - `p1` - **RETURN** ready; later features extend this bootstrap hook with their own steps (the resolution-policy-engine feature seeds the `global` Policy here once its Engine is registered) - `inst-boot-ready`

### Authorized Operation Admission

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-authorized-admission`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Success Scenarios**:
- An authenticated caller with PDP permission reaches the operation handler with its `AccessScope` attached

**Error Scenarios**:
- Unauthenticated request: rejected by the platform `api-gateway` before any QE handler runs
- Malformed public request/target shape: canonical `InvalidArgument` before PDP
- PDP denies a structurally valid target: canonical `PermissionDenied`, no partial operation
- PDP unreachable: canonical `ServiceUnavailable`, fail-closed, nothing mutated

**Steps**:
1. [x] - `p1` - Caller sends an operation request with a platform bearer token - `inst-adm-request`
2. [x] - `p1` - Platform `api-gateway` authenticates and populates the service principal in `SecurityContext`; target attribution remains untrusted request data - `inst-adm-authn`
3. [x] - `p1` - Deserialize the request and run the operation's documented public target-shape checks; reject malformed shape with canonical `InvalidArgument` before any PDP call - `inst-adm-shape`
4. [x] - `p1` - API: call `PolicyEnforcer::access_scope(...)` with the requested operation and explicit target — the in-process PEP evaluates against `authz-resolver` and compiles the response itself, returning `AccessScope` or `EnforcerError`; QE never sees the raw decision and keeps no PDP decision cache of its own - `inst-adm-pdp`
5. [x] - `p1` - **IF** the call returns `EnforcerError` (denied, compile-failed, or PDP unreachable) - `inst-adm-deny-if`
   1. [x] - `p1` - **RETURN** the canonical error (`PermissionDenied` / `ServiceUnavailable`); no handler runs - `inst-adm-deny`
6. [x] - `p1` - Carry the returned `AccessScope` unmodified to the operation handler for `SecureConn` consumption - `inst-adm-scope`
7. [x] - `p1` - **RETURN** control to the operation handler with `SecurityContext` and `AccessScope` attached; the in-process SDK client enters at this same admission step, so both transports share one authorization boundary - `inst-adm-forward`

## 3. Processes / Business Logic (CDSL)

### Two-Phase PDP Scope Enforcement

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-pdp-constraint-composition`

**Input**: `SecurityContext`, the PDP-authorized target tenant, the `AccessScope` returned by `PolicyEnforcer`, target storage operation

**Output**: Storage operation executed under the caller's tenant and PDP scope, or a canonical error

**Steps**:
1. [ ] - `p1` - Accept `tenant_id` only after PDP authorizes the complete explicit target against the authenticated principal - `inst-pdp-derive`
2. [ ] - `p1` - Bind the authorized `tenant_id` into the storage query as a mandatory filter (storage-layer half of defense-in-depth) - `inst-pdp-bind-tenant`
3. [ ] - `p1` - Pass the `AccessScope` to `SecureConn` unmodified; QE never interprets, widens, or re-compiles scope constraints itself - `inst-pdp-scope`
4. [ ] - `p1` - DB: `SecureConn` compiles the scope into query filters; rows outside tenant or scope are unreachable by construction - `inst-pdp-execute`
5. [ ] - `p1` - **RETURN** the filtered result; cross-tenant rows never leave the storage layer - `inst-pdp-return`

### Bounded-Cardinality Telemetry Emission

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-telemetry-emission`

**Input**: A gear-specific counter, histogram, or gauge observation with candidate labels

**Output**: Emitted metric with only catalogue-declared, deployment-bounded labels

**Steps**:
1. [x] - `p1` - Emit through OpenTelemetry instruments declared on the platform `Meter` (ToolKit's global `SdkMeterProvider`), behind a domain metrics port with an infra adapter; no runtime filtering layer - `inst-tel-emit`
2. [x] - `p1` - Emission sites use only the fixed PRD §5.16 instrument catalogue and the deployment-bounded labels declared there; canonical registered `metric` is permitted only on instruments that declare it - `inst-tel-closed`
3. [x] - `p1` - `tenant_id`, `subject_id`, `quota_id`, `policy_id`, `idempotency_key`, `lease_token`, projection type, caller attribution, and raw/unregistered metric input never appear as label values; a declared `metric` label is populated only after registry/catalogue validation with the canonical registered identity; conformance is enforced by tests and code review at each emission site - `inst-tel-highcard`
4. [x] - `p1` - **RETURN** the observation to the platform OTLP export when the `otel` feature is enabled - `inst-tel-export`

## 4. States (CDSL)

Not applicable: the leadership states (`Leader`, `Follower`, `Lost`) belong to the cluster gear's leader election.
The sweeper features consume them through the coordination adapter's run-while-leader semantics.

## 5. Definitions of Done

### SDK Contract Crate

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-sdk-contracts`

The system **MUST** ship a `quota-enforcement-sdk` crate defining `QuotaEnforcementStoragePluginV1` with its closed
`StorageError` enum and the domain types and closed enums that contract references, so plugin authors implement
against a single dependency. The SDK defines no coordination contract; coordination is consumed from the platform
`cluster` gear (see the Cluster Coordination Adapter DoD).

**Implements**:
- `cpt-cf-quota-enforcement-flow-gear-bootstrap`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`,
`cpt-cf-quota-enforcement-constraint-single-storage-plugin`

**Touches**:
- API: `QuotaEnforcementStoragePluginV1` (SDK trait)
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: `MutationResult`

### Reference Storage Plugin on toolkit-db

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-storage-plugin`

The system **MUST** provide the storage-plugin crate on `toolkit-db` using `SecureConn` exclusively. Foundation
delivers the complete `QuotaEnforcementStoragePluginV1` SDK contract, the plugin crate skeleton with `bootstrap()`,
schema migrations for the foundation tables, and internal storage functions upholding transactional atomicity (I1),
read-only reads (I3), tenant and `AccessScope` binding on every query, strong in-tenant consistency (I10), durable
RPO = 0 acknowledgement, and `SchemaVersionMismatch` rejection at bootstrap (I12). Later features add their internal
storage functions incrementally; the final `impl QuotaEnforcementStoragePluginV1` is wired only when every primitive
the trait names exists — no placeholder or `unimplemented!` method ever ships.

**Implements**:
- `cpt-cf-quota-enforcement-flow-gear-bootstrap`
- `cpt-cf-quota-enforcement-algo-pdp-constraint-composition`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`,
`cpt-cf-quota-enforcement-constraint-single-storage-plugin`

**Touches**:
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: configuration rows (`contention_timeout_config`, `lease_capacity_config`, `idempotency_retention_config`)

### Gateway Admission and Tenant Isolation

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-gateway-admission`

The system **MUST** mount the Gateway into the platform `api-gateway`, reject unauthenticated requests before any
handler, run phase-1 PDP admission via `authz-resolver-sdk::PolicyEnforcer` with fail-closed posture and no QE-side
decision cache, pass the returned `AccessScope` unmodified to `SecureConn`, apply the same admission boundary to the
in-process SDK entry, and stamp the PDP-authorized target `tenant_id` on every persisted tenant row.

**Implements**:
- `cpt-cf-quota-enforcement-flow-authorized-admission`
- `cpt-cf-quota-enforcement-algo-pdp-constraint-composition`

**Constraints**: `cpt-cf-quota-enforcement-constraint-security-context`,
`cpt-cf-quota-enforcement-constraint-no-business-logic`

**Touches**:
- API: REST registration under `/v1/quota-enforcement/...`
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: `SecurityContext` (platform-provided)

### Workspace and Crate Skeletons

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-workspace-crates`

The system **MUST** register the `quota-enforcement`, `quota-enforcement-sdk`, and storage plugin crates in the
workspace so every crate compiles with only foundation behavior present. The gear **MUST** depend on `cluster-sdk`
only and **MUST NOT** declare `deps = [cluster]` (cluster DESIGN §3.17.7): the embedded binary links the `cluster`
gear, a provider plugin, and the mandatory `grpc-hub`; the remote image enables QE's forwarding Cargo feature and
links none of them. Start ordering comes from the cluster `system` tier and readiness gating from the SDK-submitted
consumer registration. Bootstrap and readiness are exercised in tests against a complete
`QuotaEnforcementStoragePluginV1` test double; production plugin registration happens only once every trait primitive
exists (per the storage-plugin DoD), so no partial trait implementation is ever wired. The
gear **MUST** declare the stateful lifecycle capability and host its background tasks under ToolKit's `WithLifecycle`
model (docs/toolkit_unified_system/08_lifecycle_stateful_tasks.md): the lifecycle entry owns the background tasks and
hands each a child `CancellationToken`, and graceful shutdown cancels them within the configured stop timeout.

**Implements**:
- `cpt-cf-quota-enforcement-flow-gear-bootstrap`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`

**Touches**:
- API: workspace `Cargo.toml` membership
- Entities: crate skeletons

### Cluster Coordination Adapter

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-coordination-adapter`

The system **MUST** provide the `CoordinationAdapter` (`cpt-cf-quota-enforcement-component-coordination-plugin`) over
the platform `cluster` gear's leader election per `cpt-cf-quota-enforcement-adr-coordination-plugin`. The adapter
**MUST** define the typed cluster profile `QuotaEnforcementProfile` (name `quota-enforcement`), resolve the
leader-election facade in the gear's lifecycle `start` with the `Linearizable` requirement, scope every election name
under the `qe` prefix, and map the closed `SingletonScope` enum (`LeaseSweeper`, `RetentionSweeper`) to the election
names `lease-sweeper` and `retention-sweeper`. It **MUST** implement the domain port `SingletonCoordinator` with
run-while-leader semantics: the work starts on election with a child `CancellationToken`, is cancelled on leadership
loss, is aborted after the configured stop timeout, and restarts on re-election; the resolved cluster backend renews
the claim.
The adapter **MUST** drive the election watch itself and keep ownership of it, because the SDK's `run_while_leader`
consumes the watch and a dropped watch performs no resign. On graceful shutdown the adapter **MUST** cancel the work
and then resign every held election. A `CapabilityNotMet` or `ProfileNotBound` outcome **MUST** fail startup
(embedded profile) or readiness (deployed profile) and name `cluster` in the health endpoint. No QE-owned
coordination contract, plugin crate, or bootstrap probe ships.

**Implements**:
- `cpt-cf-quota-enforcement-flow-gear-bootstrap`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`

**Touches**:
- API: `cluster-sdk` leader-election facade (`LeaderElectionV1`), `SingletonCoordinator` port
- Entities: `SingletonScope`, `QuotaEnforcementProfile`

### Telemetry Conventions

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-telemetry-conventions`

The system **MUST** emit gear-specific instruments via `tracing` with OTLP export under the `otel` feature, enforcing
the bounded-cardinality label discipline as a compile-time/code-review convention with no high-cardinality identifier
ever used as a label value.

**Implements**:
- `cpt-cf-quota-enforcement-algo-telemetry-emission`

**Constraints**: `cpt-cf-quota-enforcement-constraint-bounded-cardinality`

**Touches**:
- API: platform observability stack (`tracing` + `toolkit` `otel` feature)
- Entities: gear-specific counters, histograms, gauges per PRD §5.16

## 6. Acceptance Criteria

- [ ] An anonymous request to any QE route is rejected by the platform `api-gateway` with `401` before a QE handler runs
- [ ] With the PDP unreachable, every write is denied with canonical `ServiceUnavailable` and no row is mutated
  (fail-closed verified by chaos test)
- [ ] A service supplying a tenant or subject outside its authorized scope receives canonical `PermissionDenied`; no
  storage operation runs (adversarial integration test)
- [ ] Bootstrap fails readiness on: incompatible schema version, missing storage plugin, unbound `quota-enforcement`
  cluster profile or unmet linearizable-election requirement, or unreachable PDP — and serves no operation in that
  state
- [ ] Binding the `quota-enforcement` profile to an eventually consistent cache double fails startup with
  `CapabilityNotMet` naming the primitive, the capability, and the provider
- [ ] The three config-table default rows exist after first bootstrap and are not duplicated by repeated bootstraps
  (idempotent seeding; the `global` Policy is seeded by the resolution-policy-engine bootstrap extension)
- [ ] Killing the elected sweeper replica makes a survivor leader within one election TTL plus observation lag; a
  graceful stop resigns and hands over within one round trip; both hold on the standalone and the Postgres cluster
  backends (recovery input consumed by the lease-operations feature, which owns the end-to-end RTO drill)
- [ ] A committed write to the foundation-owned tables (config rows, schema metadata) survives a storage-backend
  restart with zero data loss — the durable-commit contract input consumed by the consumption-operations end-to-end
  RPO drill
- [ ] Metrics scrape shows no `tenant_id`, `subject_id`, `quota_id`, `policy_id`, `idempotency_key`, `lease_token`,
  projection-type, caller, or raw/unregistered metric label on any gear-specific instrument; a canonical registered
  `metric` label appears only on instruments that declare it in PRD §5.16
- [ ] Chaos test kills gateway pods during sustained readiness/health traffic: requests fail over to surviving
  replicas with no readiness flap — the gateway-failover input consumed by the consumption-operations feature, which
  owns the end-to-end 99.95% evaluation-endpoint availability criterion; the subject-scale and quota-density
  benchmarks are likewise owned by consumption-operations, and the 15-minute recovery drill by lease-operations

## 7. Additional Context (optional)

- **Rollout / rollback**: the Gateway is stateless and multi-replica; ordinary rollout is a rolling update. Rollback is
  redeploying the previous binary against the same schema major version — schema migrations within a major are
  additive, and an incompatible schema is refused at bootstrap (`SchemaVersionMismatch`) rather than partially served.
- **Test layering**: algorithm units (`AccessScope` pass-through, scope-to-election-name mapping, label-catalogue conformance)
  get unit tests; PDP fail-closed, anonymous rejection, and forged-tenant behavior get adversarial integration tests;
  durability and failover claims are verified by the drills named in §6, not by unit tests.
- **Compile-time gates**: the domain-layer dependency rule is enforced by the repository Dylint lints; bounded label
  cardinality is enforced by typed closed enums, tests, and code review — no cardinality lint exists today and no
  runtime check exists by design.
- **Deviations from platform baselines**: none — this feature is the local instantiation of those baselines.
- **Non-applicable review domains**: UX/accessibility is not applicable — this is a backend infrastructure slice with
  no user-facing surface. Data protection and compliance inherit the Platform Operational Data rules from PRD §6.2,
  with no additional feature-specific requirements.
