Created:  2026-04-21 by Virtuozzo International GmbH

# PRD — Tenant Resolver Plugin (AM-backed)

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Non-goals](#14-non-goals)
  - [1.5 Glossary](#15-glossary)
- [2. Actors](#2-actors)
  - [2.1 Human Actors](#21-human-actors)
  - [2.2 System Actors](#22-system-actors)
- [3. Operational Concept & Environment](#3-operational-concept--environment)
  - [3.1 Core Boundary](#31-core-boundary)
  - [3.2 Deployment Context](#32-deployment-context)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 SDK Contract Implementation](#51-sdk-contract-implementation)
  - [5.2 Barrier and Status Semantics](#52-barrier-and-status-semantics)
  - [5.3 Observability](#53-observability)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Query Latency](#61-query-latency)
  - [6.2 Subtree Query Latency](#62-subtree-query-latency)
  - [6.3 Closure Consistency](#63-closure-consistency)
  - [6.4 Tenant Isolation](#64-tenant-isolation)
  - [6.5 Audit Trail](#65-audit-trail)
  - [6.6 Observability Coverage](#66-observability-coverage)
  - [NFR Exclusions](#nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
  - [8.1 Get Root Tenant](#81-get-root-tenant)
  - [8.2 Get Tenant](#82-get-tenant)
  - [8.3 Ancestor Query](#83-ancestor-query)
  - [8.4 Descendant Query](#84-descendant-query)
  - [8.5 Is Ancestor](#85-is-ancestor)
  - [8.6 Barrier-Respecting Query](#86-barrier-respecting-query)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

> **Abbreviations**: Account Management = **AM**; Tenant Resolver Plugin = **TRP**; Global Type System = **GTS**; Policy Enforcement Point = **PEP**. Used throughout this document.

## 1. Overview

### 1.1 Purpose

The Tenant Resolver Plugin (TRP) is the default implementation of the Gears [Tenant Resolver](../../../tenant-resolver/) plugin contract. It answers hierarchy and barrier questions on the authorization hot path — `get_tenant`, `get_root_tenant`, `get_tenants`, `get_ancestors`, `get_descendants`, `is_ancestor` — as a **query facade** over the tenant hierarchy owned by [Account Management (AM)](../PRD.md).

TRP exists so that every authorization decision that needs subtree scoping or barrier enforcement can be answered in single-digit milliseconds against AM's transactionally consistent `(tenants, tenant_closure)` pair, without introducing a second copy of hierarchy state or a synchronization window between AM writes and policy enforcement. The plugin owns the SDK surface; AM owns the data.

**Packaging.** TRP is not a standalone, swappable plugin — it ships as a module inside the `account-management` crate at `gears/system/account-management/src/tr_plugin/`. Co-location is deliberate: the plugin's correctness relies on AM-writer invariants (transactional `(tenants, tenant_closure)` maintenance, self-row existence, barrier materialization over `(ancestor, descendant]`, provisioning-lifecycle semantics) that the two-table schema alone does not express. A standalone crate would implicitly advertise reusability against any schema-compatible storage the plugin cannot validate at runtime; keeping TRP inside the AM crate binds it to the one writer whose invariants it trusts.

### 1.2 Background / Problem Statement

[Account Management](../DESIGN.md) is the platform's source of truth for tenant hierarchy, tenant mode (`self_managed`), and tenant status. Its administrative public APIs (`get_tenant`, `get_children`) are designed for correctness, not for per-request authorization latency. On the hot path, policy evaluation needs ancestor chains, subtree membership, and barrier enforcement answered in a single indexed read rather than by traversing the `parent_id` chain on every call.

AM maintains a canonical `tenant_closure` table with the platform-canonical schema `(ancestor_id, descendant_id, barrier, descendant_status)` from [TENANT_MODEL.md](../../../../../docs/arch/authorization/TENANT_MODEL.md), updated transactionally with every tenant write under [`cpt-cf-account-management-fr-tenant-closure`](../PRD.md). This PRD specifies the default plugin that uses that closure to serve the Tenant Resolver SDK contract.

Representative consumers this plugin serves:

| Consumer | Call pattern | Latency sensitivity |
|----------|--------------|---------------------|
| AuthZ Resolver — policy evaluation | 1–N `get_ancestors` / `is_ancestor` calls per decision | p95 ≤ 5 ms per call on the indexed closure path |
| PEP `in_tenant_subtree` predicate compilation | subtree-membership reads against `tenant_closure` | sub-millisecond indexed read on the AM-owned closure |
| Billing and operator tooling | `get_descendants` with `BarrierMode::Ignore` across whole tenant subtree | seconds-scale batch; correctness over latency |
| Operator dashboards | `get_tenant` + `get_descendants` | interactive, direct-read |

Barrier enforcement is a **security** property: a parent must not see into a subtree that has been flipped to self-managed. Consolidating hierarchy ownership in AM — tree and closure in the same transaction — removes the freshness window that a separate projection would impose. The plugin's correctness reduces to applying SDK-documented predicates and ordering over AM's canonical closure.

### 1.3 Goals (Business Outcomes)

- Answer hot-path reads within budget on the approved deployment profile: `get_tenant`, `get_root_tenant`, `get_ancestors`, and `is_ancestor` at p95 ≤ 5 ms; `get_descendants` at p99 ≤ 20 ms, so that subtree-aware authorization can be evaluated on the request path without widening the platform latency budget.
- Guarantee that every SDK call observes a transactionally consistent pair of `tenants` + `tenant_closure` — no projection lag, no freshness window, no drift — so that barrier enforcement is correct by construction rather than by a polling contract.
- Keep plugin steady-state memory flat with tenant count: no projections, no in-memory hierarchy state, and no plugin-local cache of any kind. Repeated `tenant_type_uuid → tenant_type` lookups are absorbed by `TypesRegistryClient`'s own bounded TTL-aware cache, so the plugin does not need (and does not maintain) a parallel cache.
- Keep the plugin's privilege surface minimal: a read-only database role scoped to `tenants` + `tenant_closure`, with no ability to mutate AM-owned data, so that a plugin compromise cannot corrupt canonical hierarchy state.
- Export the minimum telemetry set needed for Performance, Reliability, Security, and Versatility dashboards, so that query latency, connection-pool health, and barrier-enforcement rate are observable without bespoke instrumentation.

**Success criteria:**

| Metric | Baseline | Target | Timeframe |
|--------|----------|--------|-----------|
| Hot-path query latency (p95) | No AM-backed plugin exists against the target today | p95 ≤ 5 ms on the approved deployment profile for `get_tenant`, `get_root_tenant`, `get_ancestors`, `is_ancestor` | Gear GA |
| Subtree query latency (p99) | — | p99 ≤ 20 ms on the approved deployment profile for `get_descendants` | Gear GA |
| Closure/tree consistency | — | Zero partial-state windows observed under concurrent AM writes + plugin reads across 24 h of load | Pre-GA soak test |
| Cross-tenant leakage via stale barrier | — | Zero leaks observed across the barrier-matrix test suite (all operations × both `BarrierMode` values) | Pre-GA security gate |
| Plugin write attempts on AM storage | — | Zero — verified by privilege assertion at startup and by CI on role grants | Pre-GA security gate |
| Observability coverage | — | ≥ 3 telemetry instruments per applicable quality vector exported | Gear GA |

### 1.4 Non-goals

- **Source-of-truth tenant data.** The plugin never authors tenant records. Tenant CRUD, mode change, status change, and type validation are owned by AM.
- **Hierarchy closure maintenance.** `tenant_closure` is owned and maintained by AM under [`cpt-cf-account-management-fr-tenant-closure`](../PRD.md). The plugin reads it; it never writes it.
- **Authorization decisions.** The plugin returns hierarchy data and enforces barrier semantics at the data layer. Whether a caller is entitled to call `BarrierMode::Ignore` or to observe `suspended`/`deleted` tenants is an AuthZ Resolver / gateway concern.
- **Process-local caching of any plugin-owned data.** The plugin holds no in-process cache — no cache of tenants, ancestors, descendants, closure rows, or `tenant_type_uuid → tenant_type` mappings. Consistency is a property of AM transactional writes, not of a plugin cache-invalidation scheme. Tenant-type reverse-hydration goes through `TypesRegistryClient`, which owns the cache for that mapping.
- **REST / gRPC wire API.** The plugin is in-process behind the Tenant Resolver gateway and exposes no external endpoint. The gateway owns all network-facing contracts.
- **Multi-region reads.** v1 is single-region; cross-region routing is an SRE-level decision tied to the platform's multi-region posture.
- **Read-replica routing.** v1 reads from the primary. Replica routing is revisited per deployment profile.

### 1.5 Glossary

> **Canonical source**: platform-wide terms (Tenant, Barrier, Barrier Mode, Self-Managed Tenant, Tenant Barrier, Tenant Status, `tenant_closure`, GTS) are defined authoritatively in [Account Management PRD §1.5](../PRD.md#15-glossary) and [TENANT_MODEL.md](../../../../../docs/arch/authorization/TENANT_MODEL.md). This glossary does **not** redefine them — updates to those terms happen in the canonical source and are inherited here by reference. The table below covers tr-plugin-specific terms only.

| Term | Definition |
|------|------------|
| Query Facade | Architectural role of the plugin — it exposes the SDK trait and translates each call into a parameterized SQL read against AM-owned storage, with no intermediate state. |
| Read-only Database Role | Dedicated database role provisioned for the plugin with `SELECT`-only grants on `tenants` and `tenant_closure` and no other privileges. |
| Barrier Flag | Platform-canonical column on `tenant_closure` noting whether `BarrierMode::Respect` must stop traversal for a given `(ancestor_id, descendant_id)` pair. AM defines it over the path `(ancestor, descendant]` (ancestor excluded, descendant included), with self-rows fixed to `false`, and maintains it transactionally with tenant writes. |
| Denormalized Status | Platform-canonical `descendant_status` column on `tenant_closure`, maintained by AM so status filtering does not require joining `tenants` on every hot-path read. |
| Provisioning Row | Transient internal tenant state (`tenants.status = 'provisioning'`) written by AM during bootstrap and the tenant-create saga and compensated away or finalized to `active`. Provisioning tenants have **no** rows in `tenant_closure` — closure entries are inserted atomically with the `provisioning → active` transition, so the closure contract never exposes provisioning state to downstream consumers. The SDK `TenantStatus` enum has no provisioning variant. The plugin additionally filters these rows out of direct `tenants` reads as defense-in-depth (see [`cpt-cf-tr-plugin-fr-provisioning-invisibility`](#provisioning-row-invisibility)). |

## 2. Actors

### 2.1 Human Actors

#### Platform Operator

**ID**: `cpt-cf-tr-plugin-actor-operator`

- **Role**: Operates the Gears control plane; owns the plugin's connection-pool configuration, the read-only database role provisioning, and the plugin's observability thresholds and alerts.
- **Needs**: Size and tune the connection pool against gateway concurrency; provision and rotate the plugin's read-only database role; observe query latency, pool saturation, error rates, barrier-enforcement metrics, and tenant-not-found rates.

### 2.2 System Actors

#### Tenant Resolver Gateway

**ID**: `cpt-cf-tr-plugin-actor-tenant-resolver-gateway`

- **Role**: In-process delegator that receives every platform call on the [Tenant Resolver](../../../tenant-resolver/) SDK and resolves the configured plugin through `ClientHub`. This plugin is one such implementation; the gateway does not evaluate hierarchy logic itself.

#### AuthZ Resolver Plugin

**ID**: `cpt-cf-tr-plugin-actor-authz-resolver`

- **Role**: Issues `get_ancestors`, `is_ancestor`, and `get_descendants` calls through the gateway during policy evaluation. Drives the hot-path read traffic that the plugin's connection pool and AM's closure indexes are sized for.

#### Policy Enforcement Point (PEP)

**ID**: `cpt-cf-tr-plugin-actor-pep`

- **Role**: Domain-module query compiler that reads subtree-membership information. In SQL-backed deployments PEP reads AM's canonical `tenant_closure` directly at query compilation time; the plugin exposes the same data through the SDK for non-SQL consumers.

#### Account Management

**ID**: `cpt-cf-tr-plugin-actor-account-management`

- **Role**: Source-of-truth tenant service. Owns `tenants` and `tenant_closure` and maintains them transactionally on every tenant write. See [AM PRD — Tenant Hierarchy](../PRD.md#52-tenant-hierarchy-management) and [AM DESIGN — Tenant Service](../DESIGN.md#tenantservice).

#### Platform Telemetry

**ID**: `cpt-cf-tr-plugin-actor-platform-telemetry`

- **Role**: Collects OpenTelemetry metrics, traces, and structured logs emitted by the plugin; surfaces them to operator dashboards and alert routes. Consumes only output; does not call the plugin.

## 3. Operational Concept & Environment

The plugin is a Gear hosted inside the Tenant Resolver gateway process. It participates in the platform gear lifecycle (start → ready → stop) and uses the shared `ClientHub` for gateway registration. This section records only gear-specific deviations from project defaults.

### 3.1 Core Boundary

The plugin:

- Implements the [`TenantResolverPluginClient`](../../../tenant-resolver/tenant-resolver-sdk/src/plugin_api.rs) SDK trait and registers with the gateway through `ClientHub` under the plugin's GTS instance scope.
- Connects to the AM-owned database through a dedicated SecureConn pool bound to a read-only database role (`SELECT` on `tenants` and `tenant_closure`).
- Translates SDK calls to parameterized SQL reads, applies `BarrierMode` / status filter / `max_depth` as SQL predicates, and hydrates SDK types from the result rows.

The plugin does not:

- Own any schema, table, index, migration, or persisted state.
- Cache tenants, ancestors, descendants, closure rows, or any derived hierarchy state in memory.
- Run a sync loop, rebuild a projection, or consume a change-token from AM.
- Evaluate authorization decisions, issue SQL predicates beyond the SDK contract, or apply `SecurityContext` enforcement.
- Write to AM-owned storage — the database role forbids it.
- Expose any REST, gRPC, or external wire API; all observability is via OpenTelemetry signals and structured logs.
- Participate in user, group, or resource lifecycle flows.

### 3.2 Deployment Context

- **Process:** In-process gear hosted by the Tenant Resolver gateway. Gateway registration traverses `ClientHub`.
- **Storage access:** SecureConn connection access to the AM-owned database. The original design calls for a dedicated read-only role; current implementation shares AM's writer pool pending a `toolkit-db` per-role pool abstraction (see DESIGN §3.5 `cpt-cf-tr-plugin-constraint-read-only-role` for the deviation note). No network boundary with AM's writer process; consistency is enforced at the database level by AM's transactional closure maintenance.
- **Config surface:** `tr_plugin.enabled`, `tr_plugin.priority` (gating + selection); `db_url` / `pool_max_connections` / `query_timeout` are inherited from the AM `Db` configuration today and will be augmented with a separate read-only endpoint when the per-role pool lands.
- **Isolation:** The plugin holds the same credentials AM's writer holds (deviation from §3.5; see DESIGN). When the per-role pool ships, the plugin will hold only the credentials for the read-only role and no AM writer / IdP / signing credentials.

## 4. Scope

### 4.1 In Scope

- Full implementation of the [`TenantResolverPluginClient`](../../../tenant-resolver/tenant-resolver-sdk/src/plugin_api.rs) trait (`get_tenant`, `get_root_tenant`, `get_tenants`, `get_ancestors`, `get_descendants`, `is_ancestor`) with SDK-contract ordering, null/barrier/status semantics, and `BarrierMode` handling (p1).
- Parameterized SQL reads against AM-owned `tenants` and `tenant_closure`, applying barrier, status, and `max_depth` as SQL predicates on the AM-canonical closure columns (p1).
- Full implementation of root-tenant discovery (`get_root_tenant`) with the SDK's single-root invariant and deterministic failure semantics when AM storage is inconsistent (p1).
- Reverse hydration of public `tenant_type` values from AM's stored `tenant_type_uuid` through `TypesRegistryClient` (caching for the mapping is owned by the registry client; the plugin maintains no parallel cache) (p1).
- SecureConn connection pool bound to a dedicated read-only database role, with privilege assertion at plugin startup and in CI (p2 — deferred follow-up; current implementation shares AM's writer pool, see DESIGN §3.5 deviation note).
- Per-statement query timeout and connection-pool backpressure (p2).
- OpenTelemetry telemetry set covering Performance, Reliability, Security, and Versatility vectors (p1).
- Structured logs carrying `trace_id` on every SDK call, plus `request_id` when the gateway provides one (p1).

### 4.2 Out of Scope

- Hierarchy closure maintenance — owned by AM under [`cpt-cf-account-management-fr-tenant-closure`](../PRD.md).
- Process-local caching of tenants, ancestors, descendants, or closure rows — by design; consistency is transactional at the database.
- Any synchronization loop, projection rebuild, drift detection, revision/change-token contract with AM, or staging-swap mechanism — AM owns closure transactionally.
- Multi-region reads and cross-region routing.
- Read-replica routing and topology tuning.
- Public REST / gRPC API surface or external SDK — callers interact with the gateway, not the plugin.
- Authorization decision logic, `SecurityContext` validation, or tenant-type trait interpretation.
- Tenant data mutations — the plugin never writes to AM; the database role forbids it.

## 5. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e) targeting 90%+ code coverage unless otherwise specified. Document verification method only for non-test approaches.

### 5.1 SDK Contract Implementation

#### Plugin Client Registration

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-fr-plugin-api`

**Actors**: `cpt-cf-tr-plugin-actor-tenant-resolver-gateway`

The plugin **MUST** implement the [`TenantResolverPluginClient`](../../../tenant-resolver/tenant-resolver-sdk/src/plugin_api.rs) SDK trait in full and register itself with the Tenant Resolver gateway through `ClientHub` under the plugin's configured GTS instance identifier. All SDK-defined semantics (nullability, ordering, barrier handling, error variants) **MUST** be preserved unchanged.

- **Rationale**: The gateway is the only consumer. Any deviation from the SDK contract would either fail at the gateway's trait-bound or silently break every downstream AuthZ decision that depends on hierarchy ordering and barrier enforcement.

#### Get Tenant

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-fr-get-tenant`

**Actors**: `cpt-cf-tr-plugin-actor-authz-resolver`, `cpt-cf-tr-plugin-actor-tenant-resolver-gateway`

The plugin **MUST** return a `TenantInfo` for any tenant present in `tenants` in `active`, `suspended`, or `deleted` status, and **MUST** return `TenantResolverError::TenantNotFound` for identifiers not present or whose row is in internal `provisioning` status (per [`cpt-cf-tr-plugin-fr-provisioning-invisibility`](#provisioning-row-invisibility)). The lookup **MUST** be served by a single indexed `SELECT` against `tenants` carrying the provisioning-exclusion predicate.

- **Rationale**: Callers depend on status-agnostic identity resolution across the three SDK-visible statuses; filtering on `active`/`suspended`/`deleted` is the caller's responsibility and happens at the AuthZ layer, not the data layer. The `provisioning` state is a different kind of invariant — it is internal to AM and has no SDK representation, so the plugin treats it as absence by construction.

#### Get Root Tenant

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-fr-get-root-tenant`

**Actors**: `cpt-cf-tr-plugin-actor-authz-resolver`, `cpt-cf-tr-plugin-actor-tenant-resolver-gateway`

The plugin **MUST** return the unique root tenant as `TenantInfo`, where the root is the single tenant with `parent_id = NULL` and `status <> 'provisioning'`. The lookup **MUST** be served from AM-owned `tenants`. If storage does not currently contain exactly one non-provisioning root tenant — including the bootstrap window when the root is still `provisioning` — the plugin **MUST** fail deterministically with `TenantResolverError::Internal` rather than synthesizing a value or returning a provisioning row (per [`cpt-cf-tr-plugin-fr-provisioning-invisibility`](#provisioning-row-invisibility)).

- **Rationale**: The SDK treats the single-root invariant as part of the tenant model contract. If AM storage violates that invariant — or if bootstrap has not yet finalized the root — returning an arbitrary row would hide a structural data-integrity fault on the authorization path.

#### Get Tenants (Batch)

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-fr-get-tenants`

**Actors**: `cpt-cf-tr-plugin-actor-authz-resolver`

The plugin **MUST** accept a batch of identifiers, deduplicate them, apply the caller-supplied `GetTenantsOptions.status` filter as a SQL predicate (`status = []` means all SDK-visible statuses), return a `TenantInfo` for each identifier found that matches the filter, and silently drop identifiers not present in `tenants` or whose row is in internal `provisioning` status (per [`cpt-cf-tr-plugin-fr-provisioning-invisibility`](#provisioning-row-invisibility)). Order of the response is **not** required to match input order (matches the SDK doc comment on `get_tenants`).

- **Rationale**: Bulk lookups are common in authorization evaluation; silent-drop semantics match the SDK contract and let callers treat absence as authoritative at the moment of the read. Provisioning rows are silently dropped for the same reason — they are never visible to SDK callers.

#### Get Ancestors

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-fr-get-ancestors`

**Actors**: `cpt-cf-tr-plugin-actor-authz-resolver`, `cpt-cf-tr-plugin-actor-tenant-resolver-gateway`

The plugin **MUST** return `GetAncestorsResponse`, where `tenant` is the requested tenant as `TenantRef` and `ancestors` is the strict-ancestor chain in deterministic direct-parent-first order (root last), with a stable tie-breaker across tenants at the same hierarchy level. The starting tenant **MUST** exist and **MUST NOT** be invisible per [`cpt-cf-tr-plugin-fr-provisioning-invisibility`](#provisioning-row-invisibility) — otherwise the plugin returns `TenantResolverError::TenantNotFound`. Ancestors that are themselves invisible per provisioning invisibility **MUST** be excluded. When `BarrierMode::Respect` is requested, barrier enforcement **MUST** follow [`cpt-cf-tr-plugin-fr-barrier-semantics`](#barrier-enforcement-at-the-data-layer): a self-managed starting tenant returns an empty ancestor chain; a self-managed ancestor is itself included and traversal stops above it. When `BarrierMode::Ignore` is requested, the full strict-ancestor chain **MUST** be returned. Ancestor ordering **MUST NOT** require any application-layer hierarchy walk — see [DESIGN §3.2 PluginImpl](DESIGN.md#pluginimpl) and [DESIGN §3.6 Ancestor Query](DESIGN.md#ancestor-query-hot-path) for the allocation.

- **Rationale**: AuthZ traverses ancestor chains to evaluate inherited policy and resource-scope eligibility; deterministic direct-parent-first ordering is required so that callers can short-circuit at the nearest-ancestor match. Ancestor ordering is derived from AM's denormalized hierarchy depth (`cpt-cf-account-management-nfr-context-validation-latency` index strategy) and is served by indexed reads; the concrete mechanism is an allocation decision recorded in DESIGN.

#### Get Descendants

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-fr-get-descendants`

**Actors**: `cpt-cf-tr-plugin-actor-authz-resolver`, `cpt-cf-tr-plugin-actor-tenant-resolver-gateway`

The plugin **MUST** return `GetDescendantsResponse`, where `tenant` is the requested tenant as `TenantRef` and `descendants` is the descendant subtree in the SDK's deterministic pre-order traversal (parent before descendants of that parent; stable sibling ordering), honouring the caller-supplied `BarrierMode`, `max_depth`, and `status_filter`. The starting tenant **MUST** exist and **MUST NOT** be invisible per [`cpt-cf-tr-plugin-fr-provisioning-invisibility`](#provisioning-row-invisibility) — otherwise the plugin returns `TenantResolverError::TenantNotFound`. `max_depth` **MUST** be enforced as a bound on traversal depth from the starting tenant. Barrier enforcement under `BarrierMode::Respect` **MUST** follow [`cpt-cf-tr-plugin-fr-barrier-semantics`](#barrier-enforcement-at-the-data-layer): self-managed children and their subtrees are excluded, while queries that start from inside a self-managed tenant's own subtree are allowed. Descendants invisible per provisioning invisibility **MUST** be unconditionally excluded regardless of caller-supplied `status_filter`. The `status_filter` **MUST** apply only to `descendants`, not to the starting `tenant` (per SDK contract). Results **MUST** exclude the starting tenant from `descendants`. The concrete ordering mechanism and `max_depth` bounding are allocation decisions recorded in [DESIGN §3.2 PluginImpl](DESIGN.md#pluginimpl) and [DESIGN §3.6 Descendant Query](DESIGN.md#descendant-query-barrier-aware).

- **Rationale**: Subtree queries are the second most frequent call from AuthZ; bounding depth and filtering by `descendant_status` as a SQL predicate on the closure row avoids transporting rows the caller would discard. Pre-order is computed at query time because AM exposes neither a materialized pre-order key nor a per-closure-row depth; the cost of the bounded recursive walk scales with the returned subtree rather than with total tenant count. Barrier enforcement remains a single SQL predicate on the closure — the recursive walk is *ordering* work, not *barrier* work.

#### Is Ancestor

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-fr-is-ancestor`

**Actors**: `cpt-cf-tr-plugin-actor-authz-resolver`

The plugin **MUST** answer `is_ancestor(ancestor_id, descendant_id)` with `true` iff `descendant_id` is a strict descendant of `ancestor_id` under the supplied `BarrierMode`, using AM-owned `tenant_closure` after validating that both tenant identifiers exist and are not in `provisioning` status. Under `BarrierMode::Respect`, the existence probe **MUST** apply the canonical no-barrier predicate on the closure row (v1: `AND barrier = 0`); because AM defines `barrier` over `(ancestor, descendant]`, this also enforces the SDK's endpoint case that a self-managed `descendant_id` is not considered reachable from any ancestor. Self-reference (`ancestor_id == descendant_id`) **MUST** return `false`. If either tenant identifier is absent from `tenants` or its row is in `provisioning` status (per [`cpt-cf-tr-plugin-fr-provisioning-invisibility`](#provisioning-row-invisibility)), the plugin **MUST** return `TenantResolverError::TenantNotFound`.

- **Rationale**: `is_ancestor` is the lightest-weight decision primitive for policy evaluation; strict-descendant semantics match the authorization model (a tenant is not its own ancestor).

### 5.2 Barrier and Status Semantics

#### Barrier Enforcement at the Data Layer

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-fr-barrier-semantics`

**Actors**: `cpt-cf-tr-plugin-actor-authz-resolver`, `cpt-cf-tr-plugin-actor-pep`

Every SDK operation invoked with `BarrierMode::Respect` **MUST** enforce the per-pair barrier against AM's canonical barrier state as the sole barrier-evaluation step — no application-layer path walking, no per-row policy evaluation, and no plugin-local recomputation of barrier state from tenant mode. Barrier enforcement **MUST** reduce to a single-point lookup per ancestor-descendant pair so that the three SDK edge cases are all satisfied by the same check: self-managed starting tenants return empty ancestors; self-managed children are excluded from descendants (`Respect`); `is_ancestor` returns `false` when the descendant endpoint is self-managed or another barrier lies on the path. `BarrierMode::Ignore` **MUST** return rows regardless of barrier state and **MUST** be observable on a dedicated metric so operators can audit bypass usage. Alignment with AM's canonical barrier semantics — defined over the interval `(ancestor, descendant]` per [`cpt-cf-account-management-principle-barrier-as-data`](../DESIGN.md#barrier-as-data) and [TENANT_MODEL.md §Closure Table](../../../../../docs/arch/authorization/TENANT_MODEL.md#closure-table) — is a correctness precondition.

Ordering and `max_depth` bounding for `get_descendants` (see [`cpt-cf-tr-plugin-fr-get-descendants`](#get-descendants)) **MUST NOT** re-evaluate, replicate, or override the barrier check — barrier enforcement remains a single lookup per pair. The concrete mechanism (schema, predicates, CTE shape) is an allocation recorded in [DESIGN §3.2 PluginImpl](DESIGN.md#pluginimpl) and [DESIGN §3.6 Descendant Query](DESIGN.md#descendant-query-barrier-aware).

- **Rationale**: Barrier enforcement must be answered in hot-path time per authorization decision; AM maintains the canonical barrier state transactionally with tenant mutations — see [`cpt-cf-account-management-principle-barrier-as-data`](../DESIGN.md#barrier-as-data). Keeping query-time traversal strictly out of barrier evaluation preserves auditability: whether a row crosses a respected barrier is always a property of one canonical data point, never of a code path.

#### Status Filtering at the Data Layer

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-fr-status-filtering`

**Actors**: `cpt-cf-tr-plugin-actor-authz-resolver`

The plugin **MUST** apply caller-supplied status filters against AM's canonical tenant-status state without any application-layer hierarchy walk for status filtering. For `get_descendants`, caller-supplied status filtering **MUST NOT** apply to the starting tenant — the caller observes the starting tenant regardless of its status. Provisioning exclusion is enforced structurally by AM's closure contract (provisioning tenants have no closure rows, so closure-driven reads cannot surface them) and by the plugin's defense-in-depth filter on direct `tenants` reads (per [`cpt-cf-tr-plugin-fr-provisioning-invisibility`](#provisioning-row-invisibility)); the caller cannot opt in to provisioning rows. The concrete mechanism (use of AM's denormalized closure-status column vs. per-row join) is an allocation recorded in [DESIGN §3.2 PluginImpl](DESIGN.md#pluginimpl).

- **Rationale**: Subtree-scoped authorization frequently excludes `Suspended`/`Deleted` tenants; leveraging AM's denormalized descendant-status column avoids amplifying read cost per descendant, and the SDK contract requires the starting tenant not to be filtered out. The provisioning exclusion is orthogonal — it is a plugin-boundary invariant, not an authorization choice, so caller intent cannot override it.

#### Provisioning Row Invisibility

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-fr-provisioning-invisibility`

**Actors**: `cpt-cf-tr-plugin-actor-tenant-resolver-gateway`, `cpt-cf-tr-plugin-actor-authz-resolver`

The plugin **MUST** treat AM rows carrying the internal `provisioning` status as non-existent from the SDK perspective. Provisioning invisibility is enforced in two layers:

1. **Structural (primary)** — AM's closure contract ([`cpt-cf-account-management-fr-tenant-closure`](../PRD.md)) guarantees `tenant_closure` contains no rows for provisioning tenants. Closure-driven reads (`get_descendants`, `is_ancestor`, strict-ancestor side of `get_ancestors`) therefore cannot surface provisioning tenants at all, by construction.
2. **Defense-in-depth (plugin-side)** — direct reads of `tenants` (existence probes for every SDK method, bulk-by-ids for `get_tenants`, ancestor hydration JOINs for `get_ancestors`) **MUST** carry a provisioning-exclusion predicate so that a corrupted or transitional `tenants` row cannot leak into an SDK response.

Concretely:

- `get_tenant` and `is_ancestor` **MUST** return `TenantResolverError::TenantNotFound` when the matched `tenants` row is provisioning.
- `get_tenants` **MUST** silently drop provisioning rows from the response.
- `get_root_tenant` **MUST** apply the provisioning filter before evaluating the single-root invariant; if the sole root-candidate is still provisioning (e.g. bootstrap in progress), the plugin **MUST** fail with `TenantResolverError::Internal`.
- `get_ancestors` and `get_descendants` **MUST** treat a provisioning starting tenant as absent (`TenantNotFound`).
- The exclusion **MUST** be enforced uniformly across every SDK method.

The exclusion applies regardless of caller-supplied `GetTenantsOptions.status` / `GetDescendantsOptions.status_filter` — provisioning is not a caller-selectable status. The concrete enforcement point (query-builder boundary in `PluginImpl`, uniform across every SDK method) is an allocation recorded in [DESIGN §3.2 PluginImpl](DESIGN.md#pluginimpl) and [DESIGN §1.2 Functional Drivers row for `cpt-cf-tr-plugin-fr-provisioning-invisibility`](DESIGN.md#12-architecture-drivers).

- **Rationale**: AM's tenant-create saga and bootstrap flow persist transient `provisioning` rows in the `tenants` table **only** — `tenant_closure` rows are not written during the provisioning window. Closure rows are inserted in a single transaction with the `provisioning → active` transition at saga step 3, and removed in a single transaction on hard-deletion (see [AM ADR-0007 — Exclude Provisioning Tenants from `tenant_closure`](../ADR/0007-cpt-cf-account-management-adr-provisioning-excluded-from-closure.md) and the DB-level guard `CHECK (descendant_status IN (1, 2, 3))` on `tenant_closure` in [migration.sql](../migration.sql)). Compensation of a stuck provisioning tenant deletes only the `tenants` row; no closure cleanup is needed because nothing was ever written. Provisioning invisibility on closure-driven reads is therefore structural — by construction, not by filtering. The plugin-side provisioning-exclusion predicate on direct `tenants` reads (existence probes, bulk-by-ids, ancestor hydration JOINs) is defense-in-depth against a stray `tenants`-row leak: the SDK `TenantStatus` enum defines only `Active` / `Suspended` / `Deleted`, so surfacing a `provisioning` row — as a raw DB value or by synthesizing a fake status — would be both a contract violation and a pre-activation visibility leak. Centralizing the filter at the plugin boundary makes correctness verifiable by construction and keeps the rest of the SDK path oblivious to AM's internal saga states.

### 5.3 Observability

#### Metric and Log Surface

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-fr-observability`

**Actors**: `cpt-cf-tr-plugin-actor-operator`, `cpt-cf-tr-plugin-actor-platform-telemetry`

The plugin **MUST** export OpenTelemetry telemetry covering query latency per operation, database query duration, connection-pool utilization and waiters, query error rates by cause, barrier-enforcement rate, barrier-bypass usage (distinguishable on a dedicated metric), barrier-mode / query-shape mix, and tenant-not-found rate. Every SDK call **MUST** produce a database span carrying OpenTelemetry trace/span context so that logs, traces, and metric points can be joined in the platform telemetry backend. Structured logs **MUST** include `trace_id` and, when the gateway provides one, `request_id`.

- **Rationale**: Query latency, pool saturation, and barrier-enforcement rate are the operational signals that distinguish healthy traffic from a stuck pool, a scanning adversary, or a regression. Without the defined telemetry set operators cannot diagnose hot-path behavior.

## 6. Non-Functional Requirements

> **Global baselines**: Project-wide NFRs (reliability, security posture, observability conventions) are inherited from the root PRD and from AM. This section enumerates only gear-specific NFRs with measurable thresholds.
>
> **Testing strategy**: NFRs verified via automated benchmarks, load tests, chaos tests, and production telemetry on the approved deployment profile (see AM §13 Review Baseline Decisions — 100K tenants, plus the plugin's 200K-tenant scaling target).

### 6.1 Query Latency

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-nfr-query-latency`

The plugin **MUST** answer `get_tenant`, `get_root_tenant`, `get_ancestors`, and `is_ancestor` within **p95 ≤ 5 ms** on the approved deployment profile. `get_tenants` **MUST** answer within **p95 ≤ 10 ms** for batches of up to 128 identifiers; larger batches scale linearly with batch size and are explicitly out of the per-call budget.

- **Threshold**: p95 ≤ 5 ms for single-tenant and ancestor operations; p95 ≤ 10 ms for `get_tenants` at batch size ≤ 128. Measured from SDK call entry to SDK response return under nominal load with warm connection pool and warm `TypesRegistryClient` cache.
- **Rationale**: Every authorization decision may issue one or more of these calls; staying inside a 5 ms budget per call keeps authorization latency within the platform's overall request-path budget. `get_tenant` / `get_root_tenant` are one indexed `SELECT` (primary key or partial index on `parent_id IS NULL`), both carrying the `status <> 'provisioning'` predicate; `is_ancestor` is at most two indexed reads (existence probe on `tenants` PK + `EXISTS` on `tenant_closure` PK); `get_ancestors` is two indexed reads (existence probe on `tenants` PK + `tenant_closure` index on `descendant_id` joined to `tenants` PK, ordered by `tenants.depth` DESC). Batch lookups amortize connection-pool overhead across multiple identifiers but remain bounded so bulk calls do not swamp the hot-path connection pool.
- **Architecture Allocation**: See [DESIGN §3.2 `PluginImpl`](DESIGN.md#pluginimpl) and [§3.7 index coverage](DESIGN.md#37-database-schemas--tables).

### 6.2 Subtree Query Latency

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-nfr-subtree-latency`

`get_descendants` **MUST** complete at **p99 ≤ 20 ms** on the approved deployment profile when the result set is bounded to **≤ 1 000 rows** (typically by the caller supplying `max_depth` or operating on a subtree of that size). Unbounded calls (`max_depth = None` on a large subtree) are explicitly outside this NFR; they are governed by per-statement `query_timeout` and gateway-level rate limiting.

- **Threshold**: p99 ≤ 20 ms when `|descendants| ≤ 1 000`, measured end-to-end from SDK call to SDK response with warm connection pool and warm `TypesRegistryClient` cache.
- **Rationale**: Subtree queries are broader than ancestor queries and scale with the caller-controlled depth / subtree size. The plugin issues a starting-tenant existence probe plus a single recursive-CTE query that walks the `tenants.parent_id` tree rooted at the starting tenant (bounded by `max_depth`) and joins into `tenant_closure` on `(ancestor_id = starting_tenant, descendant_id)` to apply `barrier` and `descendant_status` predicates. For an N-row result set the walk does O(N) work on `tenants(parent_id, status)` plus O(N) primary-key lookups on `tenant_closure`. Bounding the NFR to the 1 000-row cut-off matches typical PEP compilation shapes and keeps the hot-path connection pool protected from unbounded scans; unbounded calls (e.g., operator rollups) are a separate regime owned by `query_timeout` and gateway-level rate limiting, not by the latency budget.
- **Architecture Allocation**: See [DESIGN §3.2 `PluginImpl`](DESIGN.md#pluginimpl) and [§3.7 index coverage](DESIGN.md#37-database-schemas--tables).

### 6.3 Closure Consistency

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-nfr-closure-consistency`

Every SDK call **MUST** observe a transactionally consistent `(tenants, tenant_closure)` pair. No configuration, load, or failure condition may produce a response in which tenant-hierarchy state disagrees between the two tables.

- **Threshold**: Zero partial-state windows observed under concurrent AM writes + plugin reads across 24 h of soak test; integration tests verify closure invariants on create/delete/status/convert without any intervening sync step.
- **Rationale**: Consistency here is a security property — a stale barrier or a stale denormalized status is a visibility bug. By binding closure maintenance to AM's write transactions (see [`cpt-cf-account-management-fr-tenant-closure`](../PRD.md)) and forbidding all plugin-side caches, the platform removes the failure mode entirely rather than bounding it. Tenant-type reverse-hydration goes through `TypesRegistryClient`'s own cache, which does not touch barrier or status semantics.
- **Architecture Allocation**: See [DESIGN §2.1 Single-Store Hierarchy](DESIGN.md#single-store-hierarchy) and [§3.5 Account Management Storage](DESIGN.md#account-management-storage).

### 6.4 Tenant Isolation

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-nfr-tenant-isolation`

Queries with `BarrierMode::Respect` **MUST NOT** return any row that lies across a respected barrier. Zero barrier-crossing leaks **MUST** be observed on the full barrier matrix test suite (every SDK operation × both `BarrierMode` values) during pre-GA and at every release.

- **Threshold**: Zero observed leaks.
- **Rationale**: A single leak is a security incident. Keeping barrier enforcement as a single SQL predicate on the AM-canonical `barrier` column makes isolation verifiable by construction.
- **Architecture Allocation**: See [DESIGN §2.1 Barrier as Data, Not as Policy](DESIGN.md#barrier-as-data-not-as-policy) and [§3.3 AM-owned schema](DESIGN.md#api-contracts-am-owned-schema-consumed-not-defined).

### 6.5 Audit Trail

- [ ] `p2` - **ID**: `cpt-cf-tr-plugin-nfr-audit-trail`

Every SDK call (success or failure) **MUST** produce a structured log carrying `trace_id` and OpenTelemetry trace/span context that identify the same call in logs, traces, and metrics. When the gateway provides a `request_id`, that identifier **MUST** be included in the same log record. Every use of `BarrierMode::Ignore` **MUST** be recorded on a dedicated telemetry instrument distinguishable from `Respect`.

- **Threshold**: 100% of SDK calls have matching log + trace context; `BarrierMode::Ignore` uses are separately countable in telemetry.
- **Rationale**: Correlating a failed call or a barrier-bypass with the corresponding trace requires shared trace context, and `request_id` improves log-level debugging when the call originates from an HTTP gateway. Without those identifiers, operators cannot diagnose a bypass or a regression from telemetry alone.
- **Architecture Allocation**: See [DESIGN §4.2 Audit](DESIGN.md#audit) and [§4.3 Feature Telemetry](DESIGN.md#feature-telemetry).

### 6.6 Observability Coverage

- [ ] `p2` - **ID**: `cpt-cf-tr-plugin-nfr-observability`

The plugin **MUST** export at least **3 telemetry instruments** per applicable quality vector: Performance, Reliability, Security, and Versatility. Efficiency is not a plugin-owned vector because the plugin holds no state.

- **Threshold**: ≥ 3 telemetry instruments × 4 vectors (Performance, Reliability, Security, Versatility); dashboards and alerts wired before GA.
- **Rationale**: Without per-vector coverage, operators cannot distinguish "slow but correct" from "fast but leaking" from "healthy but degrading"; alert fatigue and misdiagnosis follow.
- **Architecture Allocation**: See [DESIGN §4.3 Feature Telemetry](DESIGN.md#feature-telemetry).

### NFR Exclusions

- **Multi-region availability / cross-region RPO**: Not applicable — v1 is single-region. Revisit when the platform grows multi-region deployments.
- **End-user PII protection**: Not applicable — the plugin stores no state; AM is the data custodian.
- **Offline / disconnected operation**: Not applicable — the plugin is in-process behind the gateway and reads from AM-owned storage; "offline" means the database is unreachable.
- **Freshness / staleness bounds**: Not applicable — consistency is transactional at the database, not bounded by a sync interval.
- **Plugin-level uptime SLO**: Not applicable as a standalone target — plugin availability tracks the AM-owned database availability governed by AM's reliability posture; there is no independent degraded mode and no projection to fall back to. Operator-facing availability is observed via `tenant_resolver_query_errors_total{op,kind}` and `tenant_resolver_db_pool_waiters`.
- **Safety (operational safety / fail-safe / hazard warnings / emergency shutdown / human override)**: Not applicable — the plugin is a pure information system with no physical-world interaction, no actuators, no medical-device or industrial-control coupling. Safety-class requirements (ISO/IEC 25010:2023 §4.2.9) do not apply.
- **User experience (UX) / accessibility / internationalization / device-platform / inclusivity**: Not applicable — the plugin is an in-process Rust gear with no user-facing interface. Callers interact with the Tenant Resolver gateway, not with this plugin; UX / WCAG / i18n / offline-device / cognitive-accessibility concerns are out of scope at the plugin layer.
- **Plugin-level user authentication (MFA, SSO / federation, session management, credential policies)**: Not applicable — user authentication and session lifecycle are owned by the Tenant Resolver gateway's upstream chain; the plugin authenticates only its own read-only database role via SecureConn and has no user sessions or tokens of its own.
- **Write-level non-repudiation**: Not applicable — the plugin issues no writes; read-level attribution via the gateway-supplied `request_id` and OpenTelemetry trace context (see §6.5 Audit Trail) is sufficient for forensic review of plugin-observed events.
- **Plugin-level recovery (RPO / RTO / backup / DR / BC / data replication)**: Not applicable — the plugin holds no state; recovery is entirely AM-owned and operates on AM-owned tables under AM's data-governance stance.
- **Release cadence / rollback / blue-green / canary / environment parity**: Not applicable at the plugin level — the plugin ships as an in-process gear co-deployed with the Tenant Resolver gateway and inherits the platform release pipeline; rollback is "redeploy previous plugin binary" with no data migration.
- **Plugin-level log retention / incident response hooks**: Not applicable — log retention is inherited from the platform telemetry pipeline; incident alerts route through platform SRE on-call via the metrics defined in [DESIGN §4.3 Feature Telemetry](DESIGN.md#feature-telemetry).
- **Regulatory compliance / industry-standard certifications / OWASP-ASVS / NIST-800-53 plugin-specific controls**: Not applicable at the plugin scope — the plugin stores no credentials, no user PII, no tokens; compliance is inherited from the platform security posture and AM's data-governance controls.
- **OpenAPI / Swagger API documentation**: Not applicable — the plugin's contract is a Rust trait (`TenantResolverPluginClient`); the SDK trait's rustdoc is authoritative. No HTTP wire API exists at the plugin boundary.
- **End-user documentation, admin guides, training material, help system, support tier, self-service support, third-party data processors (GDPR Art. 28), user-generated content ownership, data cleansing/validation/catalog/lineage/MDM, seasonal / historical growth patterns, report-generation latency**: Not applicable — the plugin is an internal platform gear with no end-user UI, no third-party data processors, no user-generated content surface, and no report-generation path; developer documentation is the SDK trait's rustdoc plus this PRD and DESIGN.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### TenantResolverPluginClient (SDK trait, consumed)

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-interface-plugin-client-contract`

- **Type**: Rust trait
- **Stability**: stable — version-bounded by the Tenant Resolver SDK
- **Description**: The public read contract the plugin implements. Defined by [`tenant-resolver-sdk`](../../../tenant-resolver/tenant-resolver-sdk/src/plugin_api.rs); this plugin does not extend or shadow the trait.
- **Breaking Change Policy**: SDK-owned. Any breaking change to the trait requires an SDK major version bump and a coordinated plugin update.

### 7.2 External Integration Contracts

#### AM Read-Only Storage Contract

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-contract-am-read-only-role`

- **Direction**: required from AM (plugin consumes)
- **Protocol/Format**: SecureConn over an AM-owned database connection. **Target shape (deferred):** SecureConn pool over a dedicated read-only database role; **current shape:** plugin reuses AM's writer pool pending the `toolkit-db` per-role pool abstraction (see DESIGN §3.5).
- **Consumed state** (AM-owned; the plugin is a read-only consumer): the AM tenant row data and the platform-canonical closure shape defined in [TENANT_MODEL.md §Closure Table](../../../../../docs/arch/authorization/TENANT_MODEL.md#closure-table) — including AM's denormalized hierarchy-depth attribute used for ancestor ordering and descendant depth bounding, AM's canonical barrier state over the interval `(ancestor, descendant]`, and AM's denormalized descendant status. The canonical schema (column names, types, constraints, indexes) and the allocation of plugin-visible fields to the SDK projection are recorded in [DESIGN §3.3 API Contracts — AM-owned schema (consumed, not defined)](DESIGN.md#api-contracts-am-owned-schema-consumed-not-defined), [DESIGN §3.5 External Dependencies — Account Management Storage](DESIGN.md#account-management-storage), and [AM DESIGN §3.7 `tenants` + `tenant_closure`](../DESIGN.md#37-database-schemas--tables). The plugin reads only the subset of AM columns required for SDK projection; it does not read AM administrative columns (e.g., soft-delete timestamps) on the SDK path.
- **Public `tenant_type` hydration**: `tenant_type_uuid` values returned from AM are reverse-resolved through `TypesRegistryClient`; caching for the mapping lives inside the registry client (the plugin holds no parallel cache).
- **Privileges**: read-only on AM's canonical tenant tables; no mutation privileges on any AM-owned object **once the per-role pool ships**. Until then, read-only behavior is enforced **structurally** in the `tr_plugin` gear (no `secure_insert` / `secure_update` / `secure_delete` calls anywhere) and pinned by a startup audit warning. The grant-set assertion at startup / in CI lands together with the per-role pool work.
- **Compatibility**: any rename, removal, or type change to the consumed columns requires a coordinated AM + plugin release. AM is the sole writer; the plugin never invokes any mutation. Closure maintenance on every tenant write is AM's responsibility under [`cpt-cf-account-management-fr-tenant-closure`](../PRD.md).

#### Types Registry Reverse Lookup Contract

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-contract-types-registry-reverse-lookup`

- **Direction**: required from Types Registry (plugin consumes)
- **Protocol/Format**: In-process gear call through the platform gear boundary used for GTS type lookup.
- **Consumed / Provided Data**: `tenant_type_uuid -> chained tenant_type` reverse lookup for the UUIDs returned from AM-owned `tenants` rows.
- **Caching**: Caching is owned by `TypesRegistryClient` (bounded TTL-aware LRU configured under `local_client.cache.*`); the plugin issues lookups directly and maintains no parallel cache.
- **Failure semantics**: If a public `tenant_type` cannot be resolved for a tenant row, the plugin **MUST** fail the call deterministically with `TenantResolverError::Internal`; it **MUST NOT** return raw UUIDs in place of the SDK's public `tenant_type` field.
- **Compatibility**: Mapping semantics must remain stable for AM-stored `tenant_type_uuid` values produced from the shared UUIDv5 convention.

## 8. Use Cases

### 8.1 Get Root Tenant

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-usecase-get-root-tenant`

**Actor**: `cpt-cf-tr-plugin-actor-authz-resolver` (via `cpt-cf-tr-plugin-actor-tenant-resolver-gateway`)

**Preconditions**:
- Plugin is ready; connection pool is warm.
- AM storage satisfies the single-root invariant (`parent_id = NULL` on exactly one tenant row) and the root is in a non-provisioning status (bootstrap has completed).

**Main Flow**:
1. Gateway calls `get_root_tenant(ctx)` on the plugin.
2. The plugin issues a `SELECT` against AM's `tenants` for the unique row with `parent_id IS NULL AND status <> 'provisioning'`.
3. On hit, the plugin reverse-resolves `tenant_type_uuid` to the public chained `tenant_type` identifier through `TypesRegistryClient` (which absorbs repeated reads via its own bounded TTL-aware cache).
4. The plugin projects the row onto `TenantInfo` and returns it.

**Postconditions**:
- Caller receives the unique root `TenantInfo`.
- `tenant_resolver_query_duration_seconds{op="get_root_tenant"}` recorded.

**Alternative Flows**:
- **No root or multiple roots present, or sole root is still `provisioning`**: `TenantResolverError::Internal` returned; the inconsistency (or the bootstrap-incomplete state) is surfaced to operators.
- **Database unreachable / query timeout**: `TenantResolverError::Internal` returned; `tenant_resolver_query_errors_total{op="get_root_tenant",kind}` increments.

### 8.2 Get Tenant

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-usecase-get-tenant`

**Actor**: `cpt-cf-tr-plugin-actor-authz-resolver` (via `cpt-cf-tr-plugin-actor-tenant-resolver-gateway`)

**Preconditions**:
- Plugin is ready; connection pool is warm.

**Main Flow**:
1. Gateway calls `get_tenant(ctx, id)` on the plugin.
2. The plugin issues a single indexed `SELECT` against AM's `tenants` with `id = $1 AND status <> 'provisioning'`.
3. On hit, the plugin reverse-resolves `tenant_type_uuid` to the public chained `tenant_type` identifier through `TypesRegistryClient` (which absorbs repeated reads via its own bounded TTL-aware cache).
4. The plugin projects the row onto `TenantInfo` and returns it.

**Postconditions**:
- Caller receives `TenantInfo`.
- `tenant_resolver_query_duration_seconds{op="get_tenant"}` recorded.

**Alternative Flows**:
- **Tenant not found or in `provisioning` status**: `TenantResolverError::TenantNotFound` returned; `tenant_resolver_tenant_not_found_total` increments. The plugin does not distinguish "absent" from "provisioning" in the public error.
- **Database unreachable / query timeout**: `TenantResolverError::Internal` returned; `tenant_resolver_query_errors_total{op="get_tenant",kind}` increments; gateway decides retry.

### 8.3 Ancestor Query

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-usecase-ancestor-query`

**Actor**: `cpt-cf-tr-plugin-actor-authz-resolver` (via `cpt-cf-tr-plugin-actor-tenant-resolver-gateway`)

**Preconditions**:
- Tenant `T` exists in `tenants`.

**Main Flow**:
1. AuthZ calls `get_ancestors(ctx, T, options)` through the gateway.
2. The plugin validates that tenant `T` exists in `tenants` with `status <> 'provisioning'` and uses the AM-owned tenant row to populate `response.tenant`.
3. The plugin reads AM's `tenant_closure` with `descendant_id = T AND ancestor_id <> descendant_id`, joining `tenants` on the ancestor side with `status <> 'provisioning'`, applying the caller-supplied barrier mode, and ordering by `tenants.depth DESC, tenants.id`.
4. The plugin reverse-resolves the returned rows' `tenant_type_uuid` values through `TypesRegistryClient` in a single batched call (the registry client's own cache absorbs repeated reads).
5. Results are returned as `GetAncestorsResponse { tenant, ancestors }`, with `ancestors` ordered from the tenant outward (direct parent first, root last).

**Postconditions**:
- Caller receives the ancestor chain in documented order.
- `tenant_resolver_query_duration_seconds{op="get_ancestors"}` and `tenant_resolver_barrier_enforced_total` increment as applicable.

**Alternative Flows**:
- **Starting tenant not found**: `TenantResolverError::TenantNotFound` returned; `tenant_resolver_tenant_not_found_total` increments.
- **Ignore mode**: Full chain returned regardless of barrier flag; `tenant_resolver_barrier_bypass_total{op="get_ancestors"}` increments for operator audit.

### 8.4 Descendant Query

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-usecase-descendant-query`

**Actor**: `cpt-cf-tr-plugin-actor-authz-resolver` (via `cpt-cf-tr-plugin-actor-tenant-resolver-gateway`)

**Preconditions**:
- Tenant `T` exists in `tenants`.

**Main Flow**:
1. AuthZ calls `get_descendants(ctx, T, options{barrier_mode, status_filter, max_depth})` through the gateway.
2. The plugin validates that tenant `T` exists in `tenants` with `status <> 'provisioning'` and uses the AM-owned tenant row to populate `response.tenant`.
3. The plugin issues a single bounded recursive read rooted at `T` that walks the `tenants.parent_id` tree (with siblings ordered by `id`), bounded by `max_depth`. The walk is joined to `tenant_closure` on `(ancestor_id = T, descendant_id)` to apply the caller-supplied barrier mode (barrier-clear filter under `Respect`) and the caller-supplied `descendant_status` filter. Provisioning exclusion is structural — `tenant_closure` contains no provisioning rows by AM's closure contract ([`cpt-cf-account-management-fr-tenant-closure`](../PRD.md)) — so no additional predicate is needed on the join.
4. The plugin reverse-resolves the returned rows' `tenant_type_uuid` values through `TypesRegistryClient` in a single batched call (the registry client's own cache absorbs repeated reads).
5. Results are returned as `GetDescendantsResponse { tenant, descendants }`, with `descendants` in the SDK's documented pre-order traversal (parent before descendants of that parent) — ordering is produced by the CTE, not by a materialized AM column.

**Postconditions**:
- Caller receives the bounded subtree.
- `tenant_resolver_query_duration_seconds{op="get_descendants"}` recorded.

**Alternative Flows**:
- **`max_depth` = 1**: Returns direct children only.
- **Empty subtree**: Empty vector returned; not an error.
- **Status filter excludes everything**: Empty vector; `tenant_resolver_query_types_total{op="get_descendants"}` still increments.

### 8.5 Is Ancestor

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-usecase-is-ancestor`

**Actor**: `cpt-cf-tr-plugin-actor-authz-resolver`

**Preconditions**:
- Both `ancestor_id` and `descendant_id` are syntactically valid identifiers.

**Main Flow**:
1. AuthZ calls `is_ancestor(ctx, ancestor_id, descendant_id, options)`.
2. The plugin validates that both tenant identifiers resolve to rows in `tenants` with `status <> 'provisioning'`. If either row is missing or in `provisioning` status, the plugin returns `TenantResolverError::TenantNotFound` — this check **MUST** occur before any early return, consistent with `cpt-cf-tr-plugin-fr-provisioning-invisibility` and the §8.5 Alternative Flow below.
3. If `ancestor_id == descendant_id`, the plugin returns `false` without querying `tenant_closure` (a tenant is never a strict ancestor of itself; this optimization runs only after step 2 has already confirmed the id exists and is non-provisioning).
4. Otherwise the plugin issues an `EXISTS` probe on `tenant_closure` with `ancestor_id <> descendant_id` and the supplied barrier mode.

**Postconditions**:
- Caller receives a `bool`.
- `tenant_resolver_query_duration_seconds{op="is_ancestor"}` recorded.

**Alternative Flows**:
- **Either identifier absent from `tenants` or in `provisioning` status**: `TenantResolverError::TenantNotFound` returned.

### 8.6 Barrier-Respecting Query

- [ ] `p1` - **ID**: `cpt-cf-tr-plugin-usecase-barrier-respect`

**Actor**: `cpt-cf-tr-plugin-actor-authz-resolver`

**Preconditions**:
- Hierarchy: `Root → A → B (self-managed) → C`.
- AM has committed the tenant tree and the corresponding `tenant_closure` rows in the same transaction.

**Main Flow**:
1. AuthZ calls `get_descendants(Root, BarrierMode::Respect)` through the gateway.
2. Plugin returns `{A}` only; the subtree beyond the self-managed boundary at `B` is excluded by `AND barrier = 0`.
3. AuthZ calls `get_ancestors(C, BarrierMode::Respect)`; plugin returns `{B}` (ancestors beyond the barrier are excluded).
4. AuthZ calls `is_ancestor(Root, B, BarrierMode::Respect)`; plugin returns `false` because the descendant endpoint `B` itself is self-managed and `barrier = 1` on `(Root, B]`.
5. AuthZ separately calls `get_descendants(Root, BarrierMode::Ignore)` on the billing-rollup path; plugin returns `{A, B, C}`.

**Postconditions**:
- Barrier enforcement is observed at the data layer; `tenant_resolver_barrier_enforced_total` increments for steps 1–3.
- `BarrierMode::Ignore` usage is distinguishable on `tenant_resolver_barrier_bypass_total` for operator review.

**Alternative Flows**:
- **Nested barriers**: Hierarchy `Root → A (self-managed) → B → C (self-managed) → D`. `get_descendants(Root, Respect)` returns `{}` (the direct child `A` is self-managed, so `A` and its subtree are excluded); `get_descendants(A, Respect)` returns `{B}` (`B` is a plain descendant; `C` is self-managed so it and `D` are excluded); `get_descendants(C, Respect)` returns `{D}` (starting tenant `C` is self-managed but the SDK contract excludes only self-managed *descendants*; `D` is a plain descendant with `(C, D] = {D}` and `barrier = 0`, so it is included). Traversal never continues *past* a self-managed descendant, but a self-managed tenant's own subtree remains visible when queried directly.

## 9. Acceptance Criteria

- [ ] Plugin registers with the Tenant Resolver gateway via `ClientHub` under its configured GTS instance identifier and passes the gateway's SDK-conformance probe.
- [ ] Every SDK operation (`get_tenant`, `get_root_tenant`, `get_tenants`, `get_ancestors`, `get_descendants`, `is_ancestor`) passes the SDK-contract test suite, including empty-batch handling, self-reference semantics, root-tenant invariants, null/absent handling, and documented ordering guarantees.
- [ ] `BarrierMode::Respect` excludes barrier-crossing rows and `BarrierMode::Ignore` returns them, verified by the barrier-matrix test suite covering every operation × mode combination; zero leaks observed. Barrier enforcement is a single SQL predicate on `tenant_closure.barrier`; the descendant pre-order walk does not replicate or override it.
- [ ] `get_descendants` returns descendants in the SDK's deterministic pre-order with siblings ordered by `id`; `max_depth` is enforced before emission. The pre-order is produced by an in-memory walk of the parent map built from the barrier-bounded `tenant_closure` scan; the target shape (recursive CTE) is a tracked follow-up (DESIGN §3.4 `nfr-subtree-latency`). `get_ancestors` returns ancestors in `tenants.depth DESC, tenants.id` order, expressed as an `ORDER BY` on the joined `tenants` row. Verified by ordering tests on mixed-shape fixtures.
- [ ] Status filtering on `get_tenants` is applied as a SQL predicate on `tenants.status` (canonical column). Status filtering on `get_descendants` is applied as an in-memory emission predicate during the pre-order walk so that mixed-status branches (e.g., `Root → Suspended → Active` filtered by `[Active]`) still emit their matching descendants — folding the filter into the closure scan would prune whole branches whose intermediate parent fails the filter. Neither filter applies to the starting tenant.
- [ ] Every SDK call observes a transactionally consistent `(tenants, tenant_closure)` pair under concurrent AM writes, verified by an integration test that seeds tenant mutations through AM and reads through the plugin without any intervening step.
- [ ] `get_tenant`, `get_root_tenant`, `get_ancestors`, and `is_ancestor` stay within p95 ≤ 5 ms on the approved deployment profile; `get_descendants` stays within p99 ≤ 20 ms.
- [ ] **Current PR (`enabled = true` deploys, deferred enforcement):** read-only behavior is enforced **structurally** in the `tr_plugin` gear — every entry point is a `find()` / `count()`, and the gear contains no `secure_insert` / `secure_update` / `secure_delete` calls (CI may grep for these symbols inside `src/tr_plugin/` to enforce the shape). When `enabled = true`, AM emits a startup audit warning under `target = "am.tr_plugin.audit"` recording the deviation. **Future state (deferred to the `toolkit-db` per-role pool follow-up):** the plugin's database role exposes only `SELECT` on `tenants` and `tenant_closure`; a startup assertion and a CI check both confirm the grant set; any mutation attempted through the role is rejected by the database. See DESIGN §3.5 `cpt-cf-tr-plugin-constraint-read-only-role` for the implementation status note.
- [ ] Connection pool waits and errors surface on `tenant_resolver_db_pool_waiters` and `tenant_resolver_query_errors_total{op,kind}`; dashboards and alert routes exist pre-GA.
- [ ] Telemetry set exports ≥ 3 instruments per applicable quality vector (Performance, Reliability, Security, Versatility); dashboards and alert routes exist pre-GA.
- [ ] Every SDK call emits a structured log with `trace_id` and OpenTelemetry trace/span context, plus `request_id` when provided by the gateway, so that the same execution is joinable across `tenant_resolver_query_duration_seconds`, logs, and traces; every use of `BarrierMode::Ignore` is countable on a dedicated bypass metric.
- [ ] No plugin-owned REST / gRPC endpoint exists; the gateway is the sole external surface.
- [ ] `TenantResolverError::TenantNotFound` is returned (not a synthesized value) for identifiers absent from `tenants`, or for identifiers whose row is in internal `provisioning` status, on the SDK operations that require existence (`get_tenant`, `get_ancestors`, `get_descendants`, `is_ancestor`); absence and provisioning both are authoritative at the moment of the read and are not distinguished in the public error.
- [ ] Provisioning invisibility is enforced in two layers: (1) structurally, `tenant_closure` contains no rows for provisioning tenants (AM's closure contract) so closure-driven reads cannot surface them; (2) every direct read the plugin issues against `tenants` that participates in an SDK response carries the provisioning-exclusion predicate as defense-in-depth. An integration test seeds a `provisioning` tenant via AM's saga path and asserts it is invisible on every SDK method (`get_tenant`, `get_root_tenant`, `get_tenants`, `get_ancestors`, `get_descendants`, `is_ancestor`) under both `BarrierMode` values and all `status` filter choices.
- [ ] Public `tenant_type` values are returned as chained GTS schema identifiers or the call fails deterministically; raw `tenant_type_uuid` values never leak through the SDK.

## 10. Dependencies

**Plugin depends on:**

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| [Account Management](../PRD.md) | Source-of-truth tenant hierarchy; owns `tenants` and `tenant_closure` and maintains them transactionally on every tenant write. **Current:** AM exposes its writer pool to the plugin (deviation from DESIGN §3.5 — see `cpt-cf-tr-plugin-constraint-read-only-role`). **Future:** provisions the dedicated `SELECT`-only role consumed by the plugin once the `toolkit-db` per-role pool follow-up lands. | p1 |
| [Tenant Resolver gateway](../../../tenant-resolver/) | Delegation target that resolves the plugin through `ClientHub` under the plugin's GTS instance scope. | p1 |
| [Tenant Resolver SDK](../../../tenant-resolver/tenant-resolver-sdk/) | `TenantResolverPluginClient` trait and models. Changes to the trait require a coordinated plugin update. | p1 |
| AM-owned database | Physical PostgreSQL instance that hosts `tenants` and `tenant_closure`; reached via SecureConn under the plugin's read-only role. | p1 |
| [Types Registry](../../../types-registry/) | Reverse-resolves `tenant_type_uuid` values from AM storage into public chained `tenant_type` identifiers for SDK responses. | p1 |
| Platform telemetry | OpenTelemetry metrics/traces pipeline and structured-log pipeline for operator dashboards and alerts. | p2 |

**Depend on this plugin (consumers):**

| Consumer | What it consumes |
|----------|------------------|
| Tenant Resolver gateway | SDK trait methods on every tenant-hierarchy call from the platform. |
| AuthZ Resolver Plugin | Hot-path hierarchy queries via the gateway. |
| Domain-gear PEPs | Non-SQL consumers that need subtree membership through the SDK; SQL-backed PEPs read AM's `tenant_closure` directly at compilation time. |

## 11. Assumptions

- **AM transactional closure maintenance is available at implementation time.** The plugin's correctness assumes AM updates `tenant_closure` in the same transaction as every tenant write (see [`cpt-cf-account-management-fr-tenant-closure`](../PRD.md)).
- **AM-owned read-only role provisioning is a deferred follow-up.** **Future state:** once `toolkit-db` ships a per-role connection pool abstraction, the operator provisions `SELECT`-only grants on `tenants` and `tenant_closure` to the plugin's role before plugin startup, and the plugin asserts the grant set at boot. **Current state:** the plugin reuses AM's writer pool; read-only behavior is enforced **structurally** in the `tr_plugin` gear (every entry point is `find()` / `count()` and the gear contains no `secure_insert` / `secure_update` / `secure_delete` calls), and a startup audit warning under `target = "am.tr_plugin.audit"` records the deviation whenever `tr_plugin.enabled = true` (see DESIGN §3.5 `cpt-cf-tr-plugin-constraint-read-only-role`).
- **AM and the plugin share a database.** v1 assumes co-located tenant storage; cross-region or dedicated-replica routing is out of scope.
- **Types Registry is reachable for cache misses.** Public `tenant_type` hydration depends on Types Registry whenever `TypesRegistryClient`'s cache is cold or misses.
- **`SecurityContext` propagation is gateway-owned.** The plugin trusts the `SecurityContext` it receives from the gateway and does not re-validate tokens or re-authenticate callers.
- **Approved deployment profile matches AM's.** Scale targets inherit AM's profile (100K tenants, 300K users, 1K rps peak — see AM PRD §13), plus the plugin's 200K-tenant scaling target.
- **Plugin computes descendant ordering and `max_depth` at query time.** AM does not materialize a pre-order key on `tenant_closure` or a depth-from-ancestor column. **Target shape:** the plugin emits the SDK's deterministic pre-order for `get_descendants` by walking the `tenants.parent_id` tree rooted at the starting tenant in a bounded recursive CTE, with siblings ordered by `id`, and applies `max_depth` as the recursion bound. **Current shape:** `toolkit-db` does not yet expose a safe raw-SQL hook, so the plugin issues a single non-recursive `tenant_closure` scan for the barrier-bounded subtree, hydrates rows in one bulk read, and walks the parent map pre-order on the client; `max_depth` is enforced during the in-memory walk, NOT at the DB layer (server-side cost therefore scales with the full barrier-bounded subtree under the pivot — see DESIGN §3.4 `nfr-subtree-latency` for the budget impact). The recursive-CTE optimization is a tracked follow-up. Ancestor ordering for `get_ancestors` is expressed at the SQL layer using AM's stable `tenants.depth` column (absolute depth from the root) — no recursive walk is needed on the ancestor path. Barrier enforcement remains a single SQL predicate on the AM-canonical closure row; status filter is an in-memory emission predicate (so filtering by `[Active]` still emits an `Active` leaf whose intermediate parent is `Suspended`).
- **Hot-path latency budgets are evaluated in steady state.** The stated p95/p99 targets assume a warm connection pool and a warm `TypesRegistryClient` cache; cold-start and cache-miss behavior remain observable and must fail deterministically if Types Registry is unavailable.

## 12. Risks

| Risk | Likelihood | Severity | Impact | Mitigation |
|------|-----------|----------|--------|------------|
| AM's `tenant_closure` shape or index coverage diverges from the plugin's read expectations. | Low | High | Hot-path reads regress or fail under load. | Shared schema is versioned as part of AM; renames / removals require coordinated AM + plugin releases. Integration tests pin column names and index coverage. |
| Plugin role is provisioned with broader privileges than `SELECT` on the two tables. | Low | High | A plugin compromise could corrupt canonical hierarchy state. | **Current:** structural read-only enforcement inside the `tr_plugin` gear — every entry point is `find()` / `count()` and the gear contains no `secure_insert` / `secure_update` / `secure_delete` calls; a startup audit warning under `target = "am.tr_plugin.audit"` records the deviation whenever `tr_plugin.enabled = true`. **Deferred / future-state** (per DESIGN §3.5 `cpt-cf-tr-plugin-constraint-read-only-role`, lands with the `toolkit-db` per-role pool follow-up): startup assertion on the role's grant set; CI check against role definitions; operator-run provisioning playbook reviewed as part of deployment; database-side rejection of any mutation independent of plugin-code structure. |
| Connection-pool saturation under burst load stalls hot-path reads. | Medium | Medium | Elevated latency and queueing errors at the gateway. | Pool sized against the gateway's concurrency profile; `tenant_resolver_db_pool_waiters` alert; per-statement `query_timeout`. |
| Types Registry is slow or unavailable on a `tenant_type` cache miss. | Medium | Medium | Calls that need public `tenant_type` hydration fail with `Internal` or exceed hot-path latency targets during cold-start / miss bursts. | Caching for the `tenant_type_uuid → tenant_type` mapping lives inside `TypesRegistryClient` (bounded TTL-aware LRU); the plugin maintains no parallel cache. Fail deterministically rather than returning raw UUIDs; validate steady-state budgets with a warm registry-client cache and exercise cold-start separately. |
| Long-running `get_descendants` on a very large subtree monopolizes connections. | Low | Medium | Shared pool contention degrades other operations. | `max_depth` supported at the SDK; per-statement timeout enforced; gateway-level rate limiting is the primary control. |
| Database unavailability while AM writer is healthy. | Low | High | Plugin fails all reads; no independent degraded mode. | By design — the plugin has no projection to fall back to. The database is the unit of availability; HA is an AM-level concern. |
| Scaling beyond 200K tenants changes the cost profile of subtree reads. | Low | Medium | `get_descendants` latency regresses beyond documented NFR. | 200K-tenant scale fixture exercised pre-GA; AM's `tenant_closure` index design is the scaling lever, not an application-level change in the plugin. |

## 13. Open Questions

- **Read-replica routing for the plugin's role.** Owner: Platform SRE. Target resolution: deferred — v1 reads from the primary. Revisiting requires defining plugin-visible consistency semantics on the chosen replica topology.
- **Cross-region reads.** Owner: Platform SRE. Target resolution: deferred — v1 is single-region.

## 14. Traceability

- **Upstream requirements**: No UPSTREAM_REQS document exists for this plugin. Requirements are derived directly from the in-repo [AM PRD](../PRD.md), [AM DESIGN](../DESIGN.md), the [Tenant Resolver SDK trait](../../../tenant-resolver/tenant-resolver-sdk/src/plugin_api.rs), and [TENANT_MODEL.md](../../../../../docs/arch/authorization/TENANT_MODEL.md).
- **Downstream artifacts**: [DESIGN.md](DESIGN.md) maps every FR and NFR ID in this PRD to DESIGN components, principles, interfaces, and contracts — see [DESIGN §5 Traceability](DESIGN.md#5-traceability). [ADR-001](ADR/ADR-001-tenant-hierarchy-closure-ownership.md) captures the closure-ownership decision that shapes this PRD. [`schemas/tr_plugin.v1.schema.json`](schemas/tr_plugin.v1.schema.json) publishes the chained Gears Toolkit plugin instance type registered with types-registry at gear startup; ordering contracts are owned by the SDK trait docs, not by the instance schema. The plugin's existing implementation-planning artifacts are [DECOMPOSITION.md](DECOMPOSITION.md) and [features/feature-tenant-resolver-plugin.md](features/feature-tenant-resolver-plugin.md). They are registered in `.cf-studio/config/artifacts.toml` under the `[[systems.autodetect.children]]` entry whose `system_root = "{project_root}/gears/system/account-management/docs/tr-plugin"` (with `artifacts_root = "{system_root}"`), with DOCS-ONLY traceability for `DECOMPOSITION.md` and `features/*.md`.
- **Canonical platform references**:
  - [Account Management PRD](../PRD.md) — source-of-truth tenant model, closure ownership, barrier semantics.
  - [Account Management DESIGN](../DESIGN.md) — `tenants` + `tenant_closure` schema, barrier-as-data principle, transactional closure maintenance.
  - [TENANT_MODEL.md](../../../../../docs/arch/authorization/TENANT_MODEL.md) — platform-canonical `tenant_closure` schema.
  - [Tenant Resolver SDK — `TenantResolverPluginClient`](../../../tenant-resolver/tenant-resolver-sdk/src/plugin_api.rs) — authoritative trait surface.
  - [Tenant Resolver SDK — models](../../../tenant-resolver/tenant-resolver-sdk/src/models.rs) — authoritative public types reused by this plugin.
