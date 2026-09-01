# Feature: Foundation — Bootstrap, Schema & SPI Wiring

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
  - [1.5 Out of Scope](#15-out-of-scope)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Backend Bind at Startup](#backend-bind-at-startup)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Idempotent Schema Provisioning](#idempotent-schema-provisioning)
  - [Backend Error Classification](#backend-error-classification)
  - [ClickHouse Client Construction (ParsedEndpoint)](#clickhouse-client-construction-parsedendpoint)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Implement the Plugin Module lifecycle](#implement-the-plugin-module-lifecycle)
  - [Implement the Coordination Lock Manager](#implement-the-coordination-lock-manager)
  - [Implement the SPI Storage Adapter shell and error classification](#implement-the-spi-storage-adapter-shell-and-error-classification)
  - [Implement idempotent Schema Migrations](#implement-idempotent-schema-migrations)
  - [Implement GTS-scoped registration](#implement-gts-scoped-registration)
  - [Enforce TLS and secret-wrapped DSN](#enforce-tls-and-secret-wrapped-dsn)
  - [Publish the consistency profile](#publish-the-consistency-profile)
  - [Preserve vendor isolation](#preserve-vendor-isolation)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Non-Applicable Concerns](#7-non-applicable-concerns)

<!-- /toc -->

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-featstatus-foundation-implemented`

<!-- reference to DECOMPOSITION entry -->

- [x] `p1` - `cpt-cf-uc-ch-plugin-feature-foundation`

## 1. Feature Context

### 1.1 Overview

Establish the plugin's runtime substrate and its single public surface — the storage SPI — so every capability plugs into one identical execution shape: a `#[toolkit::gear]` `init` that loads typed config, builds a TLS-enforced ClickHouse client, provisions the schema idempotently, initialises the Coordination Lock Manager, and performs the GTS + ClientHub registration handshake.

### 1.2 Purpose

Foundation owns the cross-cutting plumbing every other feature builds on: the Plugin Module lifecycle, the Coordination Lock Manager (`LockManager` + `CatalogLockPort`/`LockGuardPort` testability-seam traits), the SPI Storage Adapter (the host's only entry point and the owner of backend-error classification and keyset cursor encoding), the idempotent Schema Migrations (fixed 1-year TTL default in DDL plus `ensure_retention_ttl` reconcile), and the pure-persistence / SPI-conformance / TLS-credential-non-disclosure / vendor-isolation guarantees. It performs no record, query, catalog, retention, or metric behavior itself — it exposes the shape those features realize.

**Requirements**: `cpt-cf-uc-ch-plugin-fr-schema-provisioning`, `cpt-cf-uc-ch-plugin-fr-error-classification`, `cpt-cf-uc-ch-plugin-nfr-spi-stability`, `cpt-cf-uc-ch-plugin-nfr-transport-security`, `cpt-cf-uc-ch-plugin-nfr-consistency-profile`

**Principles**: `cpt-cf-uc-ch-plugin-principle-pure-persistence`, `cpt-cf-uc-ch-plugin-principle-spi-conformance`, `cpt-cf-uc-ch-plugin-principle-honest-degradation`, `cpt-cf-uc-ch-plugin-principle-one-mechanism-two-problems`

**Constraints**: `cpt-cf-uc-ch-plugin-constraint-no-transactions`, `cpt-cf-uc-ch-plugin-constraint-vendor-isolation`

### 1.3 Actors

| Actor | Role in Feature |
| --- | --- |
| `cpt-cf-uc-ch-plugin-actor-plugin-host` | The Usage Collector core; triggers gear `init`, then discovers and binds the registered backend by vendor/priority and is the sole caller of the SPI. |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Decomposition**: `cpt-cf-uc-ch-plugin-feature-foundation`
- **Design elements**: `cpt-cf-uc-ch-plugin-component-module`, `cpt-cf-uc-ch-plugin-component-adapter`, `cpt-cf-uc-ch-plugin-component-migrations`, `cpt-cf-uc-ch-plugin-component-lock-manager`, `cpt-cf-uc-ch-plugin-component-catalog-lock-port`, `cpt-cf-uc-ch-plugin-db-schema`
- **Interfaces**: `cpt-cf-uc-ch-plugin-interface-storage-spi`
- **Contracts**: `cpt-cf-uc-ch-plugin-contract-clickhouse`, `cpt-cf-uc-ch-plugin-contract-coordination-lock`, `cpt-cf-uc-ch-plugin-contract-gts-registration`
- **Dependencies**: None

### 1.5 Out of Scope

- Record insert, dedup, compensation, and deactivation semantics — Feature 2 (`cpt-cf-uc-ch-plugin-feature-record-persistence`).
- Aggregation and keyset raw-list execution and injection-safe translation — Feature 3 (`cpt-cf-uc-ch-plugin-feature-query-aggregation`).
- Usage-type CRUD and the lock-protected verify-then-delete protocol — Feature 4 (`cpt-cf-uc-ch-plugin-feature-usage-type-catalog`).
- `TTL` clause ownership, config, and retention semantics — Feature 5 (`cpt-cf-uc-ch-plugin-feature-retention`); Foundation provides the `apply_migrations` / `ensure_retention_ttl` call sites.
- The `uc_clickhouse_*` metric inventory — Feature 6 (`cpt-cf-uc-ch-plugin-feature-observability`).
- ClickHouse cluster topology, sizing, HA — operator deployment guide.

## 2. Actor Flows (CDSL)

### Backend Bind at Startup

- [ ] `p1` - **ID**: `cpt-cf-uc-ch-plugin-flow-foundation-bind-startup`

**Actor**: `cpt-cf-uc-ch-plugin-actor-plugin-host`

**Success Scenarios**:

- Config loads, ClickHouse client builds over TLS, schema provisions idempotently, Coordination Lock Manager initialises, backend registers under its GTS instance scope, and the host binds it by vendor/priority.

**Error Scenarios**:

- Invalid config (e.g. plaintext `http://` URL without `allow_insecure_http = true`), unreachable ClickHouse, or schema DDL failure — `init` fails fast; the plugin does not register and the host does not bind it.
- Unbound or unavailable cluster `usage-collector` profile is **not** an `init` failure: `LockManager` resolves `DistributedLockV1` lazily on first acquire (cluster backends register in `start`, after this plugin's `init`); an unbound profile fails closed with retryable `Transient` on that acquire (create/delete), not at startup.

**Steps**:

1. [ ] - `p1` - Host process starts with the ClickHouse plugin enabled; ToolKit invokes the gear `init` - `inst-ch-boot-1`
2. [ ] - `p1` - Load and validate `ClickHousePluginConfig` (database URL, `allow_insecure_http`, `request_timeout_secs`, `lock_ttl_secs`, `lock_timeout_secs`, `retention_period_secs`, vendor, priority) - `inst-ch-boot-2`
   1. [ ] - `p1` - **IF** the parsed `database_url` scheme is neither `http` nor `https` — **RETURN** gear initialization failure naming the unsupported scheme (the client speaks ClickHouse's HTTP interface only); independent of `allow_insecure_http` - `inst-ch-boot-2a`
   2. [ ] - `p1` - **IF** `lock_ttl_secs <= request_timeout_secs + 5` (the client deadline) — **RETURN** gear initialization failure stating both values: one ClickHouse round-trip inside the coordination lock may consume the whole deadline and must not outlive the lease renewed immediately before it - `inst-ch-boot-2b`
3. [ ] - `p1` - **IF** the parsed (lowercase-normalized) `database_url` scheme is plaintext `http` AND `allow_insecure_http == false` - `inst-ch-boot-3`
   1. [ ] - `p1` - **RETURN** gear initialization failure: TLS enforcement violation - `inst-ch-boot-3a`
4. [ ] - `p1` - Build the `clickhouse::Client` via `build_client` / `ParsedEndpoint` — parse URL into bare base URL + user/password/database, emit `tracing::warn!` if plaintext - `inst-ch-boot-4`
5. [ ] - `p1` - Run `apply_migrations` (idempotent `CREATE TABLE IF NOT EXISTS` with fixed 1-year TTL default), then `ensure_retention_ttl` to reconcile `retention_period_secs` (schema migrations first — fail fast on schema errors) - `inst-ch-boot-5`
6. [ ] - `p1` - Construct the `LockManager` with the cluster hub handle (lazy `OnceLock` `DistributedLockV1` resolve deferred to first acquire; unbound `usage-collector` profile → `Transient` at acquire-time, not at `init`) - `inst-ch-boot-6`
7. [ ] - `p1` - **IF** provisioning fails - `inst-ch-boot-7`
   1. [ ] - `p1` - **RETURN** gear initialization failure without registering - `inst-ch-boot-7a`
8. [ ] - `p1` - Build the `PluginV1<UsageCollectorPluginSpecV1>` registration and publish to `types-registry` - `inst-ch-boot-8`
9. [ ] - `p1` - Register the SPI `StorageAdapter` as a scoped `UsageCollectorPluginV1` client via ClientHub under the GTS instance scope, carrying the configured vendor and priority - `inst-ch-boot-9`
10. [ ] - `p1` - **RETURN** backend registered and ready; the host performs vendor/priority selection - `inst-ch-boot-10`

## 3. Processes / Business Logic (CDSL)

### Idempotent Schema Provisioning

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-foundation-schema-provisioning`

**Input**: A `clickhouse::Client` handle; configured `retention_period_secs` (for post-DDL TTL reconcile).

**Output**: A provisioned schema (`cpt-cf-uc-ch-plugin-db-schema`) with TTL reconciled to config, re-runnable on restart.

**Steps**:

1. [ ] - `p1` - Strip `--` comment lines from the embedded `migrations/0001_init.sql` text to prevent false statement splits on semicolons inside comments - `inst-ch-prov-1`
2. [ ] - `p1` - Split the remaining text into individual statements, respecting single-quoted string literals - `inst-ch-prov-2`
3. [ ] - `p1` - For each statement: execute via the ClickHouse client; a `CREATE TABLE IF NOT EXISTS` re-runs as a no-op when the table already exists (DDL bakes a fixed 1-year TTL default) - `inst-ch-prov-3`
4. [ ] - `p1` - Call `ensure_retention_ttl` to reconcile the live `usage_records` TTL with `retention_period_secs` - `inst-ch-prov-4`
5. [ ] - `p1` - **IF** any statement or TTL reconcile returns an error - `inst-ch-prov-5`
   1. [ ] - `p1` - **RETURN** provisioning failure; caller fails `init` - `inst-ch-prov-5a`
6. [ ] - `p1` - **RETURN** provisioning complete - `inst-ch-prov-6`

### Backend Error Classification

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-foundation-error-classification`

**Input**: A ClickHouse client or cluster lock error surfaced from a store operation.

**Output**: A `UsageCollectorPluginError` classified as `Transient` vs `Internal` plus the typed domain variants.

**Steps**:

1. [ ] - `p1` - Inspect the error source - `inst-ch-err-1`
2. [ ] - `p1` - **IF** the error encodes a typed domain condition (idempotency conflict, record not found, already inactive, usage-type not found / already exists / referenced) - `inst-ch-err-2`
   1. [ ] - `p1` - Map to the corresponding typed `UsageCollectorPluginError` variant - `inst-ch-err-2a`
3. [ ] - `p1` - **IF** the error is retryable (ClickHouse connection loss, request timeout, client-side deadline expiry, cluster lock timeout / `ClusterError::LockExpired`) - `inst-ch-err-3`
   1. [ ] - `p1` - Classify as `Transient` (retryable) - `inst-ch-err-3a`
   2. [ ] - `p1` - Treat a server-reported error code on the fixed overload/backpressure/replication allowlist as retryable too (`159`, `202`, `203`, `209`, `210`, `252`, `279`, `285`, `999`, plus HTTP `502`/`503`/`504` when the body is unreadable), read from the *start* of the response text so a nested exception from another node cannot reclassify a permanent outer error; `241` `MEMORY_LIMIT_EXCEEDED` and `319` `UNKNOWN_STATUS_OF_INSERT` are deliberately excluded - `inst-ch-err-3b`
   3. [ ] - `p1` - Clear the backend-readiness gauge only for the unreachable-backend cases, never for a server-reported retryable code — a server that answered with backpressure is degraded, not down - `inst-ch-err-3c`
4. [ ] - `p1` - **ELSE** - `inst-ch-err-4`
   1. [ ] - `p1` - Classify as `Internal` (non-retryable) - `inst-ch-err-4a`
5. [ ] - `p1` - **RETURN** the classified error so the host applies retry/fail-closed behavior uniformly - `inst-ch-err-5`

### ClickHouse Client Construction (ParsedEndpoint)

- [ ] `p2` - **ID**: `cpt-cf-uc-ch-plugin-algo-foundation-build-client`

**Input**: The validated `database_url` from config (unwrapped from `SecretFromEnv` only at this boundary).

**Output**: A configured `clickhouse::Client`.

**Steps**:

1. [ ] - `p2` - Parse `database_url` into a `ParsedEndpoint`: extract scheme, host, port as the base URL; extract user, password, and database separately (not via `url()` path/userinfo, which `clickhouse::Client` silently ignores) - `inst-ch-client-1`
2. [ ] - `p2` - Build `clickhouse::Client::default().with_url(base_url).with_user(user).with_password(password).with_database(database)` - `inst-ch-client-2`
3. [ ] - `p2` - Apply `request_timeout_secs` as the ClickHouse server settings `send_timeout` / `receive_timeout`, which bound the server's own socket handling - `inst-ch-client-3`
   1. [ ] - `p2` - Derive a client-side per-request deadline of `request_timeout_secs + 5s` and bound every individual ClickHouse await with it, so a connection that is accepted and then never answered — which the server settings cannot bound because they never reach a server — fails as `Transient` instead of hanging; the 5s margin keeps the server's own descriptive timeout first when the server is responsive - `inst-ch-client-3a`
4. [ ] - `p2` - **IF** `allow_insecure_http == true` AND scheme is `http://` — emit `tracing::warn!` so operators have a per-startup signal that TLS is disabled - `inst-ch-client-4`
5. [ ] - `p2` - **RETURN** the configured client handle - `inst-ch-client-5`

## 4. States (CDSL)

Not applicable — Foundation establishes the schema, adapter, and registration handshake but defines no entity lifecycle state machine. The `active → inactive` record lifecycle belongs to Feature 2 (`cpt-cf-uc-ch-plugin-feature-record-persistence`), and the backend-readiness gauge lifecycle to Feature 6 (`cpt-cf-uc-ch-plugin-feature-observability`).

## 5. Definitions of Done

### Implement the Plugin Module lifecycle

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-foundation-module`

The system **MUST** implement the `#[toolkit::gear]` `init` that loads and validates `ClickHousePluginConfig`, builds the ClickHouse client via `build_client` / `ParsedEndpoint`, invokes `apply_migrations` then `ensure_retention_ttl`, constructs `LockManager` (after schema provisioning — it performs no I/O and resolves its backend lazily on first acquire), and performs GTS publication plus ClientHub scoped registration. The module **MUST NOT** decide whether it is the active backend and **MUST NOT** implement SPI methods directly.

**Implements**: `cpt-cf-uc-ch-plugin-flow-foundation-bind-startup`, `cpt-cf-uc-ch-plugin-algo-foundation-schema-provisioning`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-module`
- Entities: `UsageCollectorPluginV1`, `ClickHousePluginConfig`

### Implement the Coordination Lock Manager

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-foundation-lock-manager`

The system **MUST** implement `LockManager` with `acquire_exclusive_for_create(gts_id)` and `acquire_exclusive_for_delete(gts_id)` both acquiring the **same exclusive mutex name** per `gts_id` (under a `usage-collector/` scope via `DistributedLockV1::scoped`), returning `ClusterLockGuard` which implements `LockGuardPort` (`ensure_still_held`, `release`). `LockManager` **MUST** implement `CatalogLockPort` (the testability-seam trait consumed by both `ChRecordStore` and `ChCatalogStore`). Lock resolution **MUST** be lazy via `OnceLock`; lock-manager unavailability **MUST** return `Transient`.

**Implements**: `cpt-cf-uc-ch-plugin-flow-foundation-bind-startup`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-lock-manager`, `cpt-cf-uc-ch-plugin-component-catalog-lock-port`
- Contract: `cpt-cf-uc-ch-plugin-contract-coordination-lock`

### Implement the SPI Storage Adapter shell and error classification

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-foundation-adapter`

The system **MUST** implement the single `UsageCollectorPluginV1` `StorageAdapter` that routes record operations to `ChRecordStore` and catalog operations to `ChCatalogStore`, holds no business logic, owns backend-error classification (`cpt-cf-uc-ch-plugin-algo-foundation-error-classification`) and keyset cursor encoding, and runs inside the host's ambient tracing span.

**Implements**: `cpt-cf-uc-ch-plugin-algo-foundation-error-classification`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-adapter`
- Interface: `cpt-cf-uc-ch-plugin-interface-storage-spi`
- Entities: `UsageCollectorPluginError`

### Implement idempotent Schema Migrations

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-foundation-migrations`

The system **MUST** run `apply_migrations` at `init`, executing each `CREATE TABLE IF NOT EXISTS` statement idempotently (with a fixed 1-year TTL default on `usage_records`), then run `ensure_retention_ttl` to reconcile `retention_period_secs`. The DDL runner **MUST** strip `--` comment lines before splitting statements, split while respecting single-quoted string literals, and execute each statement as a no-op when the object already exists. No external migration-tracking table or framework is used.

**Implements**: `cpt-cf-uc-ch-plugin-algo-foundation-schema-provisioning`

**Touches**:

- Component: `cpt-cf-uc-ch-plugin-component-migrations`
- DB: `cpt-cf-uc-ch-plugin-db-schema`
- DB Tables: `cpt-cf-uc-ch-plugin-dbtable-usage-records`, `cpt-cf-uc-ch-plugin-dbtable-usage-type-catalog`

### Implement GTS-scoped registration

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-foundation-registration`

The system **MUST** build the `PluginV1<UsageCollectorPluginSpecV1>` registration, publish it to `types-registry`, and register the `StorageAdapter` as a scoped `UsageCollectorPluginV1` client via ClientHub under the GTS instance scope carrying the configured vendor and priority, so the host's plugin selection can discover and bind it.

**Implements**: `cpt-cf-uc-ch-plugin-flow-foundation-bind-startup`

**Touches**: Contract: `cpt-cf-uc-ch-plugin-contract-gts-registration`

### Enforce TLS and secret-wrapped DSN

- [x] `p1` - **ID**: `cpt-cf-uc-ch-plugin-dod-foundation-tls-dsn`

The system **MUST** reject a plaintext `http://` `database_url` at config validation time unless `allow_insecure_http = true` is explicitly set. The scheme **MUST** be read from the parsed URL (normalized to lowercase) rather than matched as a raw prefix, so that `HTTP://` is gated identically. A `database_url` whose scheme is neither `http` nor `https` **MUST** be rejected at the same point regardless of `allow_insecure_http`, and the rejection **MUST** name only the scheme, never the DSN. The DSN **MUST** be wrapped in `SecretFromEnv` with no `Display`, `Serialize`, or `PartialEq`, and a `Debug` that emits `<redacted>`; the raw URL is unwrapped only at the `build_client` boundary. When `allow_insecure_http == true` and the URL is `http://`, `build_client` **MUST** emit `tracing::warn!` on every startup.

**Implements**: `cpt-cf-uc-ch-plugin-algo-foundation-build-client`

**Constraints**: `cpt-cf-uc-ch-plugin-nfr-transport-security`

**Touches**: Contract: `cpt-cf-uc-ch-plugin-contract-clickhouse`

### Publish the consistency profile

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-dod-foundation-consistency-profile`

The system **MUST** document, per deployment topology, the plugin's consistency profile: single-node deployments provide effectively immediate read-after-write; replicated deployments are bounded by ClickHouse replication lag (typically sub-second) and require operator-configured `insert_quorum` for strict cross-replica read-your-writes. The profile **MUST** state that every read path uses `FINAL` and the aggregation-latency NFR budget is measured with `FINAL` included.

**Constraints**: `cpt-cf-uc-ch-plugin-nfr-consistency-profile`

**Touches**: Design: DESIGN.md §3.8

### Preserve vendor isolation

- [x] `p2` - **ID**: `cpt-cf-uc-ch-plugin-dod-foundation-vendor-isolation`

The system **MUST** keep all ClickHouse-specific SQL, schema, and client dependencies inside this crate, depending only on `usage-collector-sdk`, `types-registry-sdk`, `cluster-sdk`, and `toolkit*` — never on the host `usage-collector` crate.

**Constraints**: `cpt-cf-uc-ch-plugin-constraint-vendor-isolation`

## 6. Acceptance Criteria

- [x] The crate implements the full `UsageCollectorPluginV1` SPI at build time (compile-time SPI conformance), with no dependency on the host `usage-collector` crate.
- [x] `init` loads config, builds the ClickHouse client via `ParsedEndpoint`, provisions the schema, reconciles `retention_period_secs` via `ensure_retention_ttl`, initialises `LockManager`, and registers under a GTS instance identifier carrying the configured vendor and priority; the plugin does not self-select as the active backend.
- [x] Re-running `init` on restart re-provisions the schema as a no-op (idempotent), with no error and no duplicate objects.
- [x] A plaintext `http://` URL is refused at config validation time unless `allow_insecure_http = true` is set; the DSN and its embedded credentials never appear in logs, error messages, or `Debug` output.
- [x] The TLS gate matches the parsed, lowercase-normalized scheme, so a mixed-case `HTTP://` URL is refused too; a `database_url` whose scheme is neither `http` nor `https` is refused at the same point even when `allow_insecure_http = true`.
- [x] `build_client` emits `tracing::warn!` on every startup when `allow_insecure_http == true` and the URL is `http://`.
- [x] Backend/ClickHouse errors and cluster lock errors are surfaced as `UsageCollectorPluginError` classified `Transient` vs `Internal` plus the typed domain variants; lock-manager unavailability returns `Transient`.
- [x] Invalid config or an unreachable ClickHouse fails `init` fast; the plugin does not register.
- [x] Both `acquire_exclusive_for_create` and `acquire_exclusive_for_delete` on `LockManager` acquire the same exclusive mutex name per `gts_id`; `ClusterLockGuard` implements `LockGuardPort` with `ensure_still_held` and `release`.
- [x] `LockManager` implements `CatalogLockPort`; `ChRecordStore` and `ChCatalogStore` both depend on `Arc<dyn CatalogLockPort>` and can be unit-tested with stub implementations.
- [x] The single-node consistency profile is documented as effectively immediate read-after-write; the replicated profile is documented with its lag bound and `insert_quorum` guidance.

## 7. Non-Applicable Concerns

- **Security — Authentication & Authorization**: Not applicable — authentication and PDP authorization are enforced upstream by the gear core (`cpt-cf-uc-ch-plugin-principle-pure-persistence`); Foundation's security obligations are transport security and credential non-disclosure.
- **Security — Audit Trail**: Not applicable — the plugin produces no auditable user actions.
- **Data Privacy / Compliance**: Not applicable — the plugin holds only opaque identifiers and metadata; classification is gear-owned.
- **Usability (UX)**: Not applicable — no user interface; all interaction is programmatic via the in-process SPI.
- **Observability (OPS-FDESIGN-001)**: Addressed cross-cuttingly by Feature 6 (`cpt-cf-uc-ch-plugin-feature-observability`); Foundation provides only the meter handle and the ClickHouse client the metric inventory instruments.
- **Performance (PERF)**: Not applicable — Foundation is startup wiring with no runtime hot path; ingestion-throughput and query-latency budgets are allocated to Features 2 and 3.
