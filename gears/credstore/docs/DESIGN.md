Updated:  2026-07-07 by Virtuozzo International GmbH

# Technical Design — CredStore


<!-- toc -->

- [1. Architecture Overview](#1-architecture-overview)
  - [1.1 Architectural Vision](#11-architectural-vision)
  - [1.2 Architecture Drivers](#12-architecture-drivers)
  - [1.3 Architecture Layers](#13-architecture-layers)
- [2. Goals / Non-Goals](#2-goals--non-goals)
  - [2.1 Goals](#21-goals)
  - [2.2 Non-Goals](#22-non-goals)
- [3. Principles & Constraints](#3-principles--constraints)
  - [3.1 Design Principles](#31-design-principles)
  - [3.2 Constraints](#32-constraints)
- [4. Technical Architecture](#4-technical-architecture)
  - [4.1 Domain Model](#41-domain-model)
  - [4.2 Component Model](#42-component-model)
  - [4.3 API Contracts](#43-api-contracts)
  - [4.4 External Interfaces & Protocols](#44-external-interfaces--protocols)
  - [4.5 Service-to-Service Pattern](#45-service-to-service-pattern)
  - [4.6 Interactions & Sequences](#46-interactions--sequences)
  - [4.7 Database schemas & tables](#47-database-schemas--tables)
  - [4.8 Deployment Topology](#48-deployment-topology)
  - [4.9 Technology Stack](#49-technology-stack)
  - [4.10 Value-Fingerprint Fence](#410-value-fingerprint-fence)
- [5. Secret Types (GTS-Based, Registry-Driven)](#5-secret-types-gts-based-registry-driven)
  - [5.1 Concept](#51-concept)
  - [5.2 Type Traits](#52-type-traits)
  - [5.3 Built-in Type Catalog (Registry Seeds)](#53-built-in-type-catalog-registry-seeds)
  - [5.4 Enforcement Points](#54-enforcement-points)
  - [5.5 Category Registry (planned, ADR-0004)](#55-category-registry-planned-adr-0004)
  - [5.6 Storage & API Changes](#56-storage--api-changes)
- [6. Secret Lifecycle & Sagas](#6-secret-lifecycle--sagas)
  - [6.1 Status Model](#61-status-model)
  - [6.2 Provisioning Saga](#62-provisioning-saga)
  - [6.3 Deprovisioning Saga](#63-deprovisioning-saga)
  - [6.4 Reaper](#64-reaper)
- [7. Risks / Trade-offs](#7-risks--trade-offs)
  - [7.1 Architectural Trade-offs](#71-architectural-trade-offs)
  - [7.2 Security and Performance Risks](#72-security-and-performance-risks)
- [8. Migration Plan](#8-migration-plan)
- [9. Open Questions](#9-open-questions)
- [10. Additional context](#10-additional-context)
  - [Plugin Registration](#plugin-registration)
  - [Configuration](#configuration)
  - [Error Mapping](#error-mapping)
  - [Observability](#observability)
- [11. Traceability](#11-traceability)

<!-- /toc -->

<!--
=============================================================================
TECHNICAL DESIGN DOCUMENT
=============================================================================
PURPOSE: Define HOW the system is built — architecture, components, APIs,
data models, and technical decisions that realize the requirements.

DESIGN IS PRIMARY: DESIGN defines the "what" (architecture and behavior).
ADRs record the "why" (rationale and trade-offs) for selected design
decisions; ADRs are not a parallel spec, it's a traceability artifact.

SCOPE:
  ✓ Architecture overview and vision
  ✓ Design principles and constraints
  ✓ Component model and interactions
  ✓ API contracts and interfaces
  ✓ Data models and database schemas
  ✓ Technology stack choices

NOT IN THIS DOCUMENT (see other templates):
  ✗ Requirements → PRD.md
  ✗ Detailed rationale for decisions → ADR/
  ✗ Step-by-step implementation flows → features/

DESIGN LANGUAGE:
  - Be specific and clear; no fluff, bloat, or emoji
  - Reference PRD requirements using `cpt-cf-credstore-fr-{slug}` IDs
  - Sections marked **Planned** describe target design not yet implemented;
    everything else describes the shipped implementation.
=============================================================================
-->

## 1. Architecture Overview

### 1.1 Architectural Vision

CredStore follows the ToolKit Gear + Plugins pattern: a **stateful gear** (`credstore`) owns all secret *metadata* (identity, sharing, ownership, lifecycle status, version) in its own database table, enforces authorization and hierarchical resolution, and exposes the public API; backend **plugins** are pure per-tenant *value stores* selected at runtime by GTS vendor configuration. The backend stores the secret value only — it carries no metadata schema, no sharing semantics, and no policy.

The SDK crate (`credstore-sdk`) defines two trait boundaries: `CredStoreClientV1` for consumers and `CredStorePluginClientV1` for backend implementations. Consumers depend only on the gear trait and never interact with plugins directly, which allows runtime backend selection without changing consumer code.

Because metadata is local, hierarchical resolution (the walk-up that searches for secrets across tenant ancestors) is a **single indexed SQL query** over the metadata table followed by at most **one** backend read for the winning row. Writes are **compensating sagas** over the metadata row and the backend value, made crash-safe by an explicit lifecycle status and a periodic reaper.

Authorization is delegated to the platform PDP (`authz-resolver`) via `PolicyEnforcer`: each operation evaluates an `AccessScope` that is enforced **in SQL** through SecureORM clamps on the metadata table. Tenant isolation is therefore enforced at the data layer, consistent with the rest of the platform.

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-credstore-fr-put-secret` | Write saga: insert `provisioning` metadata row → plugin `put` (value only) → mark `active`; overwrite ordering follows the precondition kind — version validator claims the metadata CAS before `plugin.put`, `If-Match: *` is backend-first then version bump (§6.2). **Superseded in part** by ADR-0004: the REST form (a single `POST` carrying record and value) is replaced by two requests (§4.3) |
| `cpt-cf-credstore-fr-get-secret` | Single SQL resolution over the ancestor chain, then one plugin `get` for the winning row. **Superseded in part** by ADR-0004: the value moves to `GET .../secret`, independent of the metadata read (§4.3) |
| `cpt-cf-credstore-fr-delete-secret` | Deprovisioning saga: mark `deprovisioning` → backend delete → row delete (§6.3). **Superseded in part** by ADR-0004: `DELETE` addresses the credential record (value included), not a standalone secret (§4.3) |
| `cpt-cf-credstore-fr-tenant-scoping` | Gear derives tenant from `SecurityContext.subject_tenant_id()`; own-tenant gate + SecureORM scope clamp |
| `cpt-cf-credstore-fr-sharing-modes` | `sharing` column in the gear metadata table; partial unique indexes let private and tenant/shared coexist under one reference |
| `cpt-cf-credstore-fr-authz-pdp` | PDP `AccessScope` per operation on the secret GTS resource type, enforced in SQL; fail-closed. Shipped action set is `read`/`write`/`delete`; ADR-0004 replaces it with the six actions of `cpt-cf-credstore-fr-authz-action-split` below, with no synonym for the old pair |
| `cpt-cf-credstore-fr-optimistic-concurrency` | Monotonic `version` column; `GET` returns a strong generation-bound `ETag` (`"<id>.<version>"`, §4.10); `PUT`/`DELETE` require `If-Match` (a validator or `*`) |
| `cpt-cf-credstore-fr-secret-types` | GTS-based secret types with enforceable traits (§5) |
| `cpt-cf-credstore-fr-deprovisioning` | `deprovisioning` status + compensating delete saga swept by the reaper (§6.3) |
| `cpt-cf-credstore-fr-credential-record` | Resource split (ADR-0004): the `credentials` collection item carries metadata only; the secret value lives at the `secret` sub-resource address, never on the record (§4.1, §4.3) |
| `cpt-cf-credstore-fr-list-credentials` | Upward-rooted collection read (ADR-0005): ancestor chain from tenant-resolver, tenant dimension as a PDP gate rather than a SQL clamp, `category`/`type`/`sharing`/`reference` as SQL clamps, reference-boundary cursor over reduced rows (§4.4, §4.6, §4.7) |
| `cpt-cf-credstore-fr-get-credential` | `GET /credentials/{ref}`: hierarchical resolution of the record without the value; carries the strong `ETag` so a value-blind caller can still perform a guarded write (§4.3) |
| `cpt-cf-credstore-fr-write-credential-record` | `PUT /credentials/{ref}` create-or-replace of the record only (`If-None-Match`/`If-Match`); type stays immutable; a record may legally exist without a value (§4.1, §4.3) |
| `cpt-cf-credstore-fr-read-secret` | `GET /credentials/{ref}/secret`: value-only sub-resource, `Cache-Control: no-store`, one audit record per returned value (§4.3) |
| `cpt-cf-credstore-fr-write-secret` | `PUT /credentials/{ref}/secret`: set or rotate the value under a required precondition evaluated against the **record's** validator; grants no read of that value, and never creates a record (§4.3.2) |
| `cpt-cf-credstore-fr-bulk-read-secrets` | `POST /credentials:read-secrets`: explicit-references or scoped `$filter` selector, per-item authorization and fence verification, hard cap enforced by fetching `cap + 1` rows with `TOO_MANY_MATCHES` instead of truncation, no pagination (§4.3, §4.6) |
| `cpt-cf-credstore-fr-authz-action-split` | PDP actions split into `list_meta` / `read_meta` / `write_meta` / `read_value` / `write_value` / `delete`; no action is accepted as a synonym for the previous single `read` (§4.3, §4.4) |
| `cpt-cf-credstore-fr-secret-category` | `category` column, validated against a closed registry (and a type's `allowed_categories`, §5.2) on record write; changing it requires `write_meta` and bumps `version` (§4.1, §5.4) |
| `cpt-cf-credstore-fr-override-category-consistency` | Record write resolves the reference upward and refuses a category differing from the credential it overrides (`CATEGORY_MISMATCH_WITH_INHERITED`); the reaper scans for chains that slipped past it (§5.4, §6.4) — the invariant the `category` clamp of §4.4 rests on for selectivity, though not for correctness |
| `cpt-cf-credstore-fr-inheritance-status` | `inheritance` (own / inherited / overridden) computed at resolution/reduction time from the ancestor-chain walk; never a filterable or orderable column (§4.1, §4.4) |

#### NFR Allocation

| NFR ID | NFR Summary | Allocated To | Design Response | Verification Approach |
|--------|-------------|--------------|-----------------|----------------------|
| `cpt-cf-credstore-nfr-confidentiality` | Secret values never in logs or caches | SDK + gear + plugins | `SecretValue` wrapper with redacting `Debug`/`Display` and zeroize-on-drop; hand-written redacted `Debug` on REST DTOs; `Cache-Control: no-store` on `GET`; no lossy UTF-8 decode | Unit tests on redaction; code review |
| `cpt-cf-credstore-nfr-tenant-isolation` | No cross-tenant access outside PDP scope | Gear + repo | Scope clamps in SQL; own-tenant gate with `cross_tenant_denied` metric | Repo/service tests incl. scope cases |
| `cpt-cf-credstore-nfr-observability` | Operational visibility | Gear | OpenTelemetry metrics: walk-up depth, read outcome, dependency timings, saga rollback/reap counters, inventory gauge | Metrics unit tests |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-credstore-adr-stateful-gear` | Stateful gear, value-only backend: the gear owns the `credstore_secrets` metadata table (identity, sharing, ownership, lifecycle status, version); the backend plugin stores only the value ([ADR-0001](./ADR/0001-cpt-cf-credstore-adr-stateful-gear.md)) |
| `cpt-cf-credstore-adr-deprovisioning-saga` | Delete is a saga symmetric to provisioning: a `deprovisioning` status holds the unique name until backend cleanup completes; stuck rows are swept by the reaper ([ADR-0002](./ADR/0002-cpt-cf-credstore-adr-deprovisioning-saga.md)) |
| `cpt-cf-credstore-adr-value-fingerprint-fence` | Value-fingerprint fence + generation-bound ETag: the gear stamps `value_fp = HMAC(fence_key, value)` in the same write as `sharing` and verifies it on read (fail-closed 404 on mismatch), and binds the strong ETag to `<row-id>.<version>`; closes the crosswise-PUT cross-tenant disclosure and the recreate ABA lost-update ([ADR-0003](./ADR/0003-cpt-cf-credstore-adr-value-fingerprint-fence.md)) |
| `cpt-cf-credstore-adr-secret-value-exposure` | **Proposed.** Credential record and secret value become separate resources: the record (`credentials` collection) never carries a value, the value is a sub-resource with its own address, and bulk value reads use a capped, non-paginated selector ([ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md)) |
| `cpt-cf-credstore-adr-upward-collection-read` | **Proposed.** The credential-record collection is rooted at the caller's tenant and reads upward only; the tenant dimension of PDP scope gates the caller's own tenant rather than clamping rows in SQL, so inherited rows survive ([ADR-0005](./ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md)) |

### 1.3 Architecture Layers

```
┌───────────────────────────────────────────────────────────────┐
│                Consumers (OAGW, mini-chat, gears)             │
├───────────────────────────────────────────────────────────────┤
│  credstore-sdk    │ Public API traits, models, errors, GTS    │
├───────────────────────────────────────────────────────────────┤
│  credstore        │ PDP authz, resolution, write sagas, REST, │
│  (stateful)       │ reaper, metrics — owns credstore_secrets  │
├───────────────────────────────────────────────────────────────┤
│  Plugins          │ Pure per-tenant value stores              │
│  ┌────────────────────────────┐  ┌──────────────────────────┐ │
│  │ static-credstore-plugin    │  │ production vaults        │ │
│  │ (in-memory, dev/test)      │  │ (future: external store, │ │
│  │                            │  │  OS keychain, KMS-backed)│ │
│  └────────────────────────────┘  └──────────────────────────┘ │
├───────────────────────────────────────────────────────────────┤
│  Platform deps    │ authz-resolver (PDP), tenant-resolver,    │
│                   │ types-registry (GTS), toolkit-db          │
└───────────────────────────────────────────────────────────────┘
```

| Layer | Responsibility | Technology |
|-------|---------------|------------|
| SDK | Public and plugin trait definitions, models, errors, GTS type declarations | Rust crate (`credstore-sdk`) |
| Gear | PDP authorization, hierarchical resolution, sharing enforcement, write sagas, lifecycle reaper, plugin selection, REST API | Rust crate (`credstore`), Axum, SeaORM/SecureORM |
| Plugins | Backend-specific secret **value** storage (per-tenant key-value CRUD; no policy, no hierarchy, no metadata) | Rust crates |
| Platform | Policy decisions (PDP), tenant hierarchy, plugin discovery | authz-resolver, tenant-resolver, types-registry |

## 2. Goals / Non-Goals

### 2.1 Goals

- Provide secure, hierarchical secret storage for platform gears and tenant administrators
- Enable flexible sharing modes: `private` (owner-only), `tenant` (tenant-wide, default), `shared` (hierarchical)
- Support service-to-service secret retrieval (e.g., OAGW retrieving secrets on behalf of customer tenants)
- Enforce authorization via the platform PDP with SQL-level scope clamps (real tenant isolation at the data layer)
- Make writes crash-safe and self-healing (saga + reaper), and reads race-free against half-written secrets
- Enforce optimistic concurrency (version / `ETag` / mandatory `If-Match`) for lost-update detection — every update/delete states its concurrency stance; creation is the only preconditionless write
- Support multiple backend value stores via plugin architecture with GTS-based runtime selection
- Ensure secret values never appear in logs, error messages, debug traces, or intermediary caches
- Enable secret shadowing: child tenants can override parent credentials without breaking existing references
- Classify secrets by GTS-based *secret types* with enforceable traits (§5)
- Symmetric, crash-safe deletion via a `deprovisioning` saga (§6.3)

### 2.2 Non-Goals

The following capabilities are explicitly out of scope:

- **Granular ACL beyond hierarchical**: fine-grained per-secret ACLs (role- or attribute-based) are out of scope. The three-tier sharing model plus PDP scope covers primary use cases.
- **Secret value history / rollback**: the `version` column supports optimistic locking only; previous values are not retained.
- **Secret rotation automation**: automatic rotation is out of scope. (Secret types may carry *advisory* rotation traits, §5.2 — enforcement/automation is future work.)
- **Direct end-user access**: unauthenticated or untrusted client access is out of scope.
- **Secret templates or composition**: dynamic secret generation or derivation is out of scope.
- **Hierarchical resolution in backends**: plugins are pure per-tenant value stores — all hierarchy, sharing, and policy logic lives in the gear.
- **Secret discovery / search**: full-text search over values or references stays out of scope. A metadata listing is **no longer** a non-goal: it is required by `cpt-cf-credstore-fr-list-credentials` and designed in [ADR-0005](./ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md); it is upward-rooted, never carries values, and never enumerates descendants.
- **MySQL support**: migrations target PostgreSQL and SQLite; MySQL fails fast with a typed error.

## 3. Principles & Constraints

### 3.1 Design Principles

#### Stateful Gear, Value-Only Backend

- [ ] `p1` - **ID**: `cpt-cf-credstore-principle-stateful-gear`

The gear owns all secret metadata in its own `credstore_secrets` table; the backend plugin stores the value only, keyed by `(tenant_id, key, key-class)` where the key class is `private-per-owner` (`owner_id = Some`) or `tenant` (`owner_id = None`). This removes any backend metadata-schema prerequisite, eliminates encoded-external-ID collision risk, and makes resolution and authorization a single transactional query.

#### Authorization via PDP, Enforced in SQL

- [ ] `p1` - **ID**: `cpt-cf-credstore-principle-authz-pdp`

Every operation evaluates a PDP `AccessScope` for its action on the secret resource type (`gts.cf.core.credstore.secret.v1~`) and enforces that scope in SQL through SecureORM clamps on the metadata table. The shipped action set is `read`, `write`, `delete`; ADR-0004 splits it into `list_meta`, `read_meta`, `write_meta`, `read_value`, `write_value` and `delete` (`cpt-cf-credstore-fr-authz-action-split`, §4.3.2), and accepts neither `read` nor `write` as a synonym for any of them, so every policy granting the old pair is re-issued. Both read and write paths additionally gate on an explicit own-tenant invariant (`scope_includes_tenant`) and emit a `cross_tenant_denied` metric. Out-of-scope access is fail-closed and surfaces as the canonical 404 (anti-enumeration) or 403. Plugins MUST NOT implement authorization.

**The collection read is the exception to "one evaluation per operation"** (§4.4): its resource is a concrete secret type, and a page can span several, so it evaluates `list_meta` once per distinct type the candidate probe found — bounded by the number of types a tenant actually uses, not by the page size, and cacheable per subject. Every other operation addresses exactly one row and therefore one type, so for those the single-evaluation rule holds unchanged.

#### Tenant from SecurityContext

- [ ] `p1` - **ID**: `cpt-cf-credstore-principle-tenant-from-ctx`

The operating tenant is always derived from `SecurityContext.subject_tenant_id()`, and the owner from `SecurityContext.subject_id()`. This reduces API surface, prevents misuse, and aligns with platform patterns. Service-to-service consumers (OAGW) construct a `SecurityContext` for the target tenant rather than passing tenant parameters.

#### Crash-Safe Writes (Saga + Reaper)

- [ ] `p1` - **ID**: `cpt-cf-credstore-principle-write-saga`

A write that spans the metadata table and the backend is a compensating saga with an explicit lifecycle status (§6). Every failure mode either rolls back, self-heals on retry, or is swept by the periodic reaper — a crash can never permanently wedge a reference or leak a readable half-written secret.

### 3.2 Constraints

#### No Secret Logging

- [ ] `p1` - **ID**: `cpt-cf-credstore-constraint-no-secret-logging`

Secret values MUST NOT appear in any log output, error messages, or debug traces. `SecretValue` implements redacting `Debug`/`Display` and zeroizes on drop; request/response DTOs carry hand-written redacted `Debug`; the `GET` response sets `Cache-Control: no-store`; non-UTF-8 values are rejected with a typed error rather than lossily decoded.

#### Canonical Error Model

- [ ] `p1` - **ID**: `cpt-cf-credstore-constraint-canonical-errors`

All trait-boundary and REST errors follow the platform canonical error model ([ADR 0005](../../../docs/arch/errors/ADR/0005-cpt-cf-adr-sdk-canonical-projection.md)): domain errors map to canonical categories with stable `reason` codes (e.g. `OPTIMISTIC_LOCK_FAILURE`), and the wire strips internal diagnostics.

## 4. Technical Architecture

### 4.1 Domain Model

**Technology**: Rust structs (`#[domain_model]`)

**Core Entities**:

| Entity | Description |
|--------|-------------|
| `SecretRef` | Validated secret reference key (e.g., `partner-openai-key`). **Format**: `[a-zA-Z0-9_-]+`, 1–255 chars; validated on construction and re-validated by a DB `CHECK`. |
| `SecretValue` | Opaque byte wrapper (`Vec<u8>`) for secret data. Redacting `Debug`/`Display`, zeroize-on-drop, deliberately not `Serialize`/`Deserialize`. |
| `SharingMode` | Enum: `Private`, `Tenant` (default), `Shared` — controls access scope within the tenant hierarchy. |
| `OwnerId` | UUID identifying the creator (`SecurityContext.subject_id()`) — access control key for `Private` mode. |
| `SecretStatus` | Lifecycle status of the metadata row: `Provisioning` (1), `Active` (2), `Deprovisioning` (3), and — planned with ADR-0004 — `Declared` (4) for a record written before its value (§4.1, §6.1). Only `Active` rows are visible to value resolution; `Declared` is additionally visible to the collection read, and no other status is visible to anything. |
| `SecretRow` | Metadata row: `{ id, tenant_id, reference, sharing, owner_id, status, version, value_fp, fp_key_id }`. `value_fp` is the internal value-fingerprint fence (§4.10), never serialized to the wire. |
| `NewSecret` | Insert shape for the create saga (always carries the fence fingerprint of the value being written). |
| `WritePrecondition` | Parsed `If-Match`, mandatory on update/delete: `Exists` (`*`, explicit last-writer-wins) or `Version { id, version }` (quoted `"<id>.<version>"`, generation-bound). |
| `GetSecretResponse` | SDK read result: `{ value, id, owner_tenant_id, sharing, is_inherited, version }`; `(id, version)` is the strong-validator pair. |
| `SecretType` | Catalog-resolved secret type binding the enforceable traits (§5); immutable per secret. |
| `CredentialRecord` (planned, ADR-0004) | The addressable **metadata** resource: reference, sharing, type, `category`, version, expiry, `inheritance` — structurally never the value, and never the owning tenant (ADR-0004, "What a response says about tenants above"). Identified by `SecretRef` in the `credentials` collection (§4.3); the secret value is a separate sub-resource of it, not a field on it. |
| `Category` (planned, `cpt-cf-credstore-fr-secret-category`) | Operator-chosen label drawn from a closed registry, attached to a `CredentialRecord`; usable as a PDP attribute predicate (§4.4) so an application can be granted values of one category only. Changing it requires the `write_meta` action and bumps `version` (§5.2, §5.4). |
| `InheritanceStatus` (planned, `cpt-cf-credstore-fr-inheritance-status`) | Enum: `Own`, `Inherited`, `Overridden` (a tenant's own record shadows an ancestor's `shared` record under the same reference). `Suppressed` is a P2 addition (§6.1) that would win resolution without altering the ancestor. Computed at resolution/reduction time, never a stored or filterable column (§4.4). |

**Relationships & uniqueness**:

- A secret belongs to exactly one tenant (`tenant_id`) and has exactly one owner (`owner_id`).
- For `tenant`/`shared` modes, `(tenant_id, reference)` is unique; for `private` mode, `(tenant_id, reference, owner_id)` is unique. Both are enforced as **partial unique indexes** (§4.7), which lets one private secret per owner and one tenant/shared secret **coexist** under the same reference.
- The uniqueness indexes ignore `status`, so an in-flight (`provisioning`) row holds the reference; failed sagas are rolled back or reaped to un-wedge it.

**Sharing-mode access control** (evaluated during SQL resolution):

| Mode | Visible to | Inherited by descendants? |
|------|-----------|---------------------------|
| `private` | Only where `owner_id` equals the caller's subject id; wins over non-private at the same tenant level | No |
| `tenant` (default) | Only the owning tenant | No |
| `shared` | The owning tenant **and** all its descendants (including through isolation barriers) | Yes |

`sharing` is a **visibility** mode (who may read the secret), not a quota/limit that composes as `min(parent, child)`; resolution picks the closest accessible secret up the ancestor chain, which is how a child tenant *shadows* a parent's `shared` secret under the same reference.

**Record without a value (planned, ADR-0004).** Splitting creation into a record write and a value write (§4.3) makes "a record exists but has no value yet" a legal, defined state rather than a transient artifact of a crashed write. It is a **stored lifecycle state** — `status = 4`, `declared`, widening the initial schema's `CHECK (status IN (1, 2, 3))` — and not an inference from a missing fingerprint; §6.1 carries the reasoning and the predicate table. Naming it here without naming its column would leave each of the properties below resting on application logic over something the metadata row does not hold:

- It **does not resolve** for a value read, which is not the same as "the read returns not-found". The row is simply not a candidate: resolution already selects `status = 2`, so a `declared` row is excluded by a filter that is already there, and the walk up the ancestor chain continues past it. What the caller gets therefore depends on the chain — an ancestor's `shared` value if one exists (next bullet), and the ordinary not-found only when the whole chain offers nothing. Reading these two bullets as "declared implies 404" is the mistake they exist to prevent.
- It **does not shadow** an ancestor: an inherited `shared` value from a parent keeps resolving for the tenant and its descendants exactly as if the value-less record did not exist. A half-finished create must not silently break inheritance that was working before it started. The collection read has to honour the same rule when it reduces a reference to one item, or the listing would claim the credential is configured locally while the point read serves the ancestor's value (ADR-0005 "Reducing a reference to one item").
- It is **distinct from `value_fp IS NULL`** (§4.10, out-of-band seeding): that case already has a value in the backend and is only missing its fingerprint, served on trust until backfilled; a record without a value has no backend value at all, so there is nothing to serve on trust. This is precisely why the state is its own `status` rather than a second meaning for a null fingerprint — one column cannot mean both "serve on trust" and "there is nothing to serve".
- It is **not swept by the reaper** as a stuck write (§6.1, §6.4): the second request may legitimately arrive much later than any saga timeout, so this resting state must not be confused with a crashed `provisioning` row. Again no exception is needed — the sweep selects `provisioning` rows past their timeout, and this is not one.
- It **is visible in the catalogue**, unlike every other non-`active` status: the collection read selects `status IN (2, 4)` and surfaces the state, because an administrator who has created a record and not yet set its value needs to see exactly that (§6.1).

### 4.2 Component Model

```mermaid
graph TB
    Consumer[Consumers<br/>OAGW, mini-chat, gears]
    SDK[credstore-sdk<br/>traits + models + GTS]
    GW[credstore gear<br/>service / saga / reaper]
    REPO[(credstore_secrets<br/>SecureORM, Scopable)]
    SP[static-credstore-plugin<br/>in-memory value store]
    PDP[authz-resolver<br/>PolicyEnforcer / PDP]
    TR[tenant-resolver]
    TReg[types-registry]

    Consumer -->|ClientHub| SDK
    SDK --> GW
    GW -->|AccessScope, SQL clamps| REPO
    GW -->|GTS instance query| TReg
    GW -->|scoped ClientHub| SP
    GW -->|ancestor chain, cached| TR
    GW -->|scope evaluation| PDP
```

**Components**:

- [ ] `p1` - **ID**: `cpt-cf-credstore-component-sdk`

`credstore-sdk` — trait definitions (`CredStoreClientV1`, `CredStorePluginClientV1`), models, canonical `CredStoreError`, and the GTS declarations: the plugin spec type (`gts.cf.toolkit.plugins.plugin.v1~cf.core.credstore.plugin.v1~`) and the secret resource type (`gts.cf.core.credstore.secret.v1~`, exported as `SECRET_RESOURCE_TYPE` — the single source of truth pinned by unit tests).

- [ ] `p1` - **ID**: `cpt-cf-credstore-component-gear`

`credstore` — the stateful gear. Layers: `api/rest` (Axum routes, DTOs with redacted `Debug`, `If-Match` parsing), `domain` (service with get/put/ delete sagas, authz scope evaluation, resolver port, metrics port, plugin selector port), `infra` (SecureORM repo, migrations, tenant-resolver adapter with TTL+LRU ancestor cache, GTS plugin selector, OTel metrics, canonical error mapping). Declares `deps = [authz-resolver, tenant-resolver, types-registry]` and capabilities `system, db, rest, stateful`; its lifecycle entry runs the reaper loop (§6.4).

- [ ] `p1` - **ID**: `cpt-cf-credstore-component-static-plugin`

`static-credstore-plugin` — in-memory per-tenant value store for development and testing, optionally seeded from YAML config, writable at runtime. Registers its GTS instance and scoped `CredStorePluginClientV1` in ClientHub.

- [ ] `p2` - **ID**: `cpt-cf-credstore-component-production-backend`

Production value-store backends (external secret vault, OS keychain, KMS-backed store) — future plugins implementing the same `CredStorePluginClientV1` contract. Not part of the current codebase.

**Interactions**:

- Consumer → Gear: `CredStoreClientV1` via ClientHub (in-process) or REST.
- Gear → Repo: all metadata reads/writes clamped by the PDP `AccessScope` (SecureORM `Scopable`).
- Gear → Plugin: `CredStorePluginClientV1` via scoped ClientHub; the plugin is resolved lazily by GTS instance query filtered by the configured `vendor`.
- Gear → tenant-resolver: ancestor chain (`BarrierMode::Ignore` — inheritance crosses isolation barriers), cached in-process with TTL + LRU eviction.
- Gear → PDP: `PolicyEnforcer.access_scope_with` per operation; no PEP capabilities advertised — the PDP hands the gear flat, pre-expanded tenant predicates (§4.4).

### 4.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-credstore-interface-clienthub`

**Technology**: Rust traits (ClientHub) + REST/OpenAPI

#### ClientHub API (in-process)

`CredStoreClientV1` (public consumer API):

| Method | Signature | Description |
|--------|-----------|-------------|
| `get` | `(ctx: &SecurityContext, key: &SecretRef) → Result<Option<GetSecretResponse>, CredStoreError>` | Hierarchical read. `Ok(None)` covers both "does not exist" and "inaccessible" (single 404 surface, anti-enumeration). |
| `put` | `(ctx, key, value: SecretValue, sharing: SharingMode, precondition: WritePrecondition) → Result<(), CredStoreError>` | Update within the target sharing class; never creates. The mandatory precondition is `Matches { id, version }` (CAS for read-modify-write) or `Exists` (explicit last-writer-wins for rotation/provisioning and fence-heal). |
| `create` | `(ctx, key, value, sharing) → Result<(), CredStoreError>` | Create-only; `Conflict` if a secret of the same sharing class exists (the 409 path behind REST `POST`). The only preconditionless write. |
| `delete` | `(ctx, key, precondition: WritePrecondition) → Result<(), CredStoreError>` | Delete the caller's own-tenant secret, guarded by the mandatory precondition. |

`CredStorePluginClientV1` (backend SPI — pure value store):

| Method | Signature | Description |
|--------|-----------|-------------|
| `get` | `(ctx, tenant_id, key, owner_id: Option<&OwnerId>) → Result<Option<SecretValue>, CredStoreError>` | `owner_id = Some` selects the owner's private key class; `None` the tenant key class. Returns the value only. |
| `put` | `(ctx, tenant_id, key, value, owner_id: Option<&OwnerId>) → Result<(), CredStoreError>` | Store the value for the addressed key class. |
| `delete` | `(ctx, tenant_id, key, owner_id: Option<&OwnerId>) → Result<(), CredStoreError>` | Delete the value; `NotFound` is treated as success by the gear (idempotent). |

**Design rationale**: the plugin returns **no metadata** — sharing, ownership, inheritance, and version all come from the gear's metadata row resolved *before* the backend is touched. This keeps every policy decision in one place and backends trivially simple.

**Planned (ADR-0004, `cpt-cf-credstore-adr-secret-value-exposure`).** `CredStoreClientV1` gains three record-oriented methods — `metadata` (point read of one record), `list_metadata` (the collection of §4.4) and `read_secrets` (bulk value read) — re-pointed at the addresses in §4.3.2. The same three names appear in `credstore-sdk/README.md`; there is one spelling, not two. `get`, `put` and `delete` keep their names and are re-pointed at the value sub-resource and the record respectively. `create` is the one method that cannot survive unchanged: it carries a value, and a single-request create of record plus value is precisely what the split removes, so it becomes either a documented non-atomic wrapper over both writes or is dropped in favour of them. New methods ship with default "unsupported" implementations so existing implementors and test doubles keep compiling (Backward Compatibility, ADR-0004).

#### 4.3.1 REST API (Gear)

Routes are registered under `/credstore/v1` (served behind the platform API gear under its public prefix, e.g. `/cf/credstore/v1/...`). All routes are authenticated; errors use the canonical `Problem` envelope.

The machine-readable API is generated from the handlers: the platform-wide OpenAPI document lives at [`docs/api/api.json`](../../../docs/api/api.json) (regenerate with `make openapi`), and a credstore-scoped rendering is at [`openapi.yaml`](./api/openapi.yaml).

| Method | Path | Success | Description |
|--------|------|---------|-------------|
| `POST` | `/credstore/v1/secrets` | `201` + `Location` | Create-only; `409` if the same sharing class already holds the reference |
| `PUT` | `/credstore/v1/secrets/{ref}` | `204` | Update only (never creates); `If-Match` mandatory |
| `GET` | `/credstore/v1/secrets/{ref}` | `200` + `ETag`, `Cache-Control: no-store` | Hierarchical read |
| `DELETE` | `/credstore/v1/secrets/{ref}` | `204` | Delete own secret; `If-Match` mandatory |

**Create Secret Request (`POST`)**:
```json
{
  "reference": "partner-openai-key",
  "value": "demo-secret-value-123",
  "sharing": "tenant",
  "type": "gts.cf.core.credstore.secret.v1~cf.core.credstore.api_key.v1~"
}
```

**Update Secret Request (`PUT`)** — same body without `reference`. `sharing` values: `"private"`, `"tenant"` (default), `"shared"`. `type` (optional): the secret type's **full GTS type id** (§5.6), built-in or custom; defaults to the generic type. `expires_at` (optional, RFC 3339) for expirable types.

**Field notes**:

- `reference` is a **caller-chosen opaque label**, not a GTS id — `^[A-Za-z0-9_-]+$`, 1–255 chars. It is deliberately free-form because it is user-facing (a human-picked name such as `partner-openai-key`); requiring a GTS identifier would be too restrictive. Uniqueness is per sharing class within one tenant (§4.7).
- `value` is carried over REST as a **UTF-8 JSON string**, but is stored as raw **bytes** end-to-end, so binary secrets (e.g. a DER certificate or a raw key) are supported — for secret types whose `utf8_only` trait is `false` (currently `generic`) and via the **SDK/ClientHub**, which carries bytes. Over REST a binary value must be text-encoded by the caller (e.g. PEM, base64). A PEM certificate is UTF-8 text and works on any type; see the `utf8_only` trait (§5.3).

**Get Secret Response (200)**:
```json
{
  "value": "demo-secret-value-456",
  "metadata": {
    "owner_tenant_id": "22222222-2222-2222-2222-222222222222",
    "sharing": "shared",
    "is_inherited": true,
    "version": 3,
    "type": "gts.cf.core.credstore.secret.v1~cf.core.credstore.api_key.v1~"
  }
}
```

- `owner_tenant_id`: tenant that owns the resolved secret (differs from the requesting tenant when inherited)
- `is_inherited`: `true` when resolved from an ancestor
- `version`: monotonic per-generation version; the strong `ETag` is the generation-bound pair `"<row-id>.<version>"` (the row UUID is minted fresh for every recreated secret, so validators never repeat across delete+recreate — no ABA lost update)
- `type`: the secret type's full GTS type id (plus `expires_at` when set)

**Optimistic concurrency**: `PUT` and `DELETE` **require** `If-Match` — either `*` (target must exist; an explicit last-writer-wins overwrite for rotation/provisioning flows that hold no version, and the healing path for a fence-poisoned reference whose `GET` fails closed) or `If-Match: "<id>.<version>"` (both the generation id and the version must match the current row). Every write states its concurrency stance; there are no unconditional overwrites, and a `PUT` never creates (creation is `POST` only). The precondition is checked before the backend write *and* re-enforced as a `version = ?` SQL filter on the metadata commit (the id is already the UPDATE key). A mismatch — including a validator minted for an earlier generation of a recreated secret — surfaces as canonical `Aborted` / **409** with reason `OPTIMISTIC_LOCK_FAILURE` (the canonical model has no 412; 409 is the deliberate platform-correct status). A malformed `If-Match` (including a bare quoted version) is a 400 (`INVALID_IF_MATCH`); a missing `If-Match` is a 400 with its own reason (`IF_MATCH_REQUIRED` — no 428 in the canonical model); a precondition on a non-existent target is a 409.

**Error Responses**:

| Status | Canonical category | Scenario |
|--------|--------------------|----------|
| 400 | `InvalidArgument` | Invalid `reference` format, malformed `If-Match` (`INVALID_IF_MATCH`), missing `If-Match` on `PUT`/`DELETE` (`IF_MATCH_REQUIRED`), unsupported sharing transition (private ↔ tenant/shared) |
| 401 | `Unauthenticated` | Invalid or missing token |
| 403 | `AccessDenied` | PDP denies the action, or the caller's scope excludes their own tenant |
| 404 | `NotFound` | Secret not found **or** inaccessible (anti-enumeration: always 404, never 403, for per-secret access) |
| 409 | `AlreadyExists` / `Aborted` | `POST` conflict; `If-Match` version conflict (`OPTIMISTIC_LOCK_FAILURE`) |
| 503 | `ServiceUnavailable` | PDP evaluation failure, types-registry outage / stored secret type unresolvable (§5.4), no storage plugin registered (with stable detail, no `retry_after`), backend outage (with `Retry-After` when hinted) |
| 500 | `Internal` | Invariant violations, non-UTF-8 stored value on `GET` (binary written via the SDK cannot cross the JSON transport); diagnostic stripped from the wire |

#### 4.3.2 REST API — Planned Credential Surface (ADR-0004)

> Decision: [ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md) (`cpt-cf-credstore-adr-secret-value-exposure`), **status: proposed**. Supersedes the `/credstore/v1/secrets/*` surface above per the Backward Compatibility table in the ADR: the entity URL stops returning the value and starts returning the credential record. `{ref}` stays the caller-chosen `SecretRef` — never the row UUID, which is a generation id kept out of every response body so the `ETag` remains the one CAS validator source.

| Address | PDP action | Success | Key headers | Preconditions |
|---|---|---|---|---|
| `GET /credstore/v1/credentials` | `list_meta` | `200` | `Cache-Control: no-store` — value-free, but the body varies by tenant and by subject | OData `$filter`/`$orderby` on an indexed allowlist (§4.7), opaque cursor, `limit`; no total count |
| `GET /credstore/v1/credentials/{ref}` | `read_meta` | `200` | `ETag` (the CAS validator source, D4 of the ADR); `Cache-Control: no-store` | — |
| `PUT /credstore/v1/credentials/{ref}` | `write_meta` | `201` create, `204` replace | `Location` **and `ETag`** on create; `ETag` on replace | `If-None-Match: *` (create-only, **409** if present), `If-Match: "<id>.<version>"` (guarded replace, 409 on mismatch), or `If-Match: *` (last-writer-wins) |
| `DELETE /credstore/v1/credentials/{ref}` | `delete` | `204` | — | `If-Match` mandatory |
| `GET /credstore/v1/credentials/{ref}/secret` | `read_value` | `200` | `Cache-Control: no-store`; audited | — |
| `PUT /credstore/v1/credentials/{ref}/secret` | `write_value` | `204` | — | `If-Match` mandatory; the validator is read from the record, not from the value |
| `POST /credstore/v1/credentials:read-secrets` | `read_value`, evaluated per item | `200` | `Cache-Control: no-store` | selector-based, capped, no precondition (see below) |
| `POST` / `DELETE /credstore/v1/credentials/{ref}/suppression` | `write_meta` (P2, not yet designed in full) | — | — | — |

**Metadata responses are `no-store` too, not only value responses.** A record and a page of records carry no secret, but both vary by requesting tenant and by subject: the same URL legitimately yields a different catalogue to two callers, and an inherited entry depends on the caller's ancestor chain. An intermediary that cached one and served it to the other would disclose one tenant's catalogue to another — reconnaissance rather than value disclosure, but disclosure. The alternative, an identity-aware cache partition, would have to key on tenant *and* subject *and* the resolved chain, which is more contract than a catalogue read is worth. So every credential address, metadata included, is `no-store`; only the value addresses additionally carry per-value audit.

**No `PATCH`, no `POST` on the collection.** Each resource has exactly one write verb, `PUT`; the intent of a write is carried by its precondition rather than by the verb. A partial-update verb would let a client change one field of a record (e.g. `sharing`) without ever holding the whole thing — precisely how an expiry gets dropped unnoticed by a `sharing` edit. `PUT` on the record is a whole-value replace of the mutable metadata: fields absent from the body reset to their defaults, exactly as today's value `PUT` already clears an omitted `expires_at`. `PUT` with `If-None-Match: *` already expresses create-only and keeps the write idempotent, which is what removes the need for a collection-level `POST`. The record's `type` stays immutable regardless of which precondition is used.

**No single-request create of a record with a value.** Creating a credential is two requests:

```
PUT /credstore/v1/credentials/smtp-default        If-None-Match: *   → 201
PUT /credstore/v1/credentials/smtp-default/secret If-Match: *        → 204
```

This is an accepted cost of the resource split, not an oversight (§4.1): between the two requests the record exists without a value, which is a legal state, and the gear provides no atomicity across the two requests — a client that abandons the sequence leaves an empty record behind.

**What a value-write precondition is compared against.** The value sub-resource has no validator of its own: a record and its value share one `version`, which a value write bumps exactly as a record write does (§4.1). A precondition on `PUT /credentials/{ref}/secret` is therefore evaluated against the **record's** validator, the same `"<id>.<version>"` the record's `ETag` carries. This is a deliberate deviation from RFC 9110, which evaluates a precondition against the target resource's own current representation, and it is stated here rather than left to be inferred: without it, `If-Match` on a record whose value has never been written would have to be evaluated against an absent representation and could not succeed, which would make the first value write of the two-request flow above unexpressible.

The consequence is that the first value write needs no special case. The record's `ETag` exists from the moment the record is created — which is why record creation returns it (§4.3.2 table) rather than making the client issue a metadata `GET` to obtain one — so `If-Match: "<id>.<version>"` is available for the very first value write, and `If-Match: *` means "the record must exist", which is the guard that flow actually wants. A value `PUT` against a reference with no record is a 404, not a create: the record is the resource that gets created, and only by its own `PUT`.

**Bulk secret read** (`POST /credstore/v1/credentials:read-secrets`, `cpt-cf-credstore-fr-bulk-read-secrets`). Exactly one selector per request, and the two live in different places: the **explicit** selector is a `references` list in the body (`{"references": [...]}`), and the **filtered** selector is a `$filter` query parameter in platform OData syntax over `category`, `type` and `reference` with `eq`/`in`, sent with an empty body. Naming both is a 400. Every filterable field is backed by an index (§4.7). There is no `limit`, `cursor`, `$orderby` or `$top`, and the response is a flat per-reference outcome list, never a `Page<T>`. A selector matching more than the configured cap (proposed: 25) fails the whole request with `400 TOO_MANY_MATCHES` rather than truncating; the cap is enforced by fetching `cap + 1` candidate rows, never by a `COUNT` query. Every returned value is independently re-verified against its row's fingerprint (§4.10); a per-item refusal, miss, suppression or fence mismatch all report the same `not_found` outcome (anti-enumeration) and never abort the rest of the batch.

**Suppression (P2, `cpt-cf-credstore-fr-suppression`, not yet designed in full).** If adopted, `POST`/`DELETE /credentials/{ref}/suppression` lets a tenant declare an ancestor's credential disabled for itself even though the ancestor still publishes it; it attaches to the record rather than to the value, and — like the rest of this surface — is not designed beyond what this paragraph and §6.1 state.

### 4.4 External Interfaces & Protocols

#### PDP (authz-resolver)

- [ ] `p1` - **ID**: `cpt-cf-credstore-design-interface-pdp`

**Type**: platform service (in-process client via ClientHub)

Every operation calls `PolicyEnforcer.access_scope_with(ctx, resource, action, …)` **once**, with the `owner_tenant_id` PEP property and `action ∈ {read, write, delete}` as shipped — `∈ {list_meta, read_meta, write_meta, read_value, write_value, delete}` once ADR-0004 lands, where the per-address mapping in §4.3.2 is authoritative. The `resource` is always the secret's **full concrete type** — including `generic` (`…secret.v1~cf.core.credstore.generic.v1~`) — so policies can target any type without a separate base-type gate (§5.4). The type is known before the evaluation via a prefetch (post-resolution on read, post-lookup on overwrite/delete, from the requested/default type on create) followed by a types-registry resolution of the stored `secret_type_uuid` to its GTS type id (§5.4); the returned `AccessScope` is enforced in SQL. Enforcement is fail-closed: `Denied`/`CompileFailed` → 403 (404 on read, anti-enumeration), `EvaluationFailed` → 503.

**No PDP capabilities / no downward projection tables**: the gear advertises no PEP capabilities, so the PDP hands it pre-expanded, flat tenant predicates (`Eq`/`In` on `owner_tenant_id`) and resolves any subtree grant on its own side — the standard no-projection scenarios ([AUTHZ_USAGE_SCENARIOS](../../../docs/arch/authorization/AUTHZ_USAGE_SCENARIOS.md) S09–S11). What this rules out is **downward** expansion: the gear has no closure table to enumerate a subtree, so a structured `InTenantSubtree` predicate reaching it is a capability-contract breach and fails closed — unchanged by the collection read below.

**Upward-rooted collection read** ([ADR-0005](./ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md), `cpt-cf-credstore-adr-upward-collection-read`, status: proposed): the credential-record collection (§4.3.2, `cpt-cf-credstore-fr-list-credentials`) does not break the no-projection premise, because it never expands downward either. It is rooted at the caller's tenant and spans only that tenant and its ancestor chain — the same chain already fetched from the Tenant Resolver for every point read (below), never from descendants. So "there is no LIST" is restated precisely as **"there is no downward listing"**.

For that collection, the flat tenant predicate is applied as a **gate** on the caller's own tenant, not as a SQL clamp on `owner_tenant_id`. The reason is precise, not just "it would drop rows": a PDP scope that respects isolation barriers excludes a barrier tenant's ancestors **by construction**, so a clamp built from such a scope would zero out the inherited half of the catalogue for exactly the tenants sitting behind a barrier — even though their applications keep receiving that inherited value from the point read (`cpt-cf-credstore-fr-hierarchical-resolve`), via the same barrier-bypassing ancestor-chain lookup described below. Clamping the tenant dimension would therefore make the listing lie about what the point read actually returns. The SQL query instead selects candidate rows across the whole ancestor chain under the same visibility rules the point read uses (private/tenant/shared, §4.1), and the PDP decision gates the **request** — "does the scope admit the caller's own tenant" — exactly as it does for a point read, so one authorization path serves both reads and a change to visibility rules cannot apply to one and miss the other. Attribute predicates drawn from the same scope are applied as ordinary SQL clamps (§4.7), but only the ones that are **invariant across a reference's chain** — `category` and `type`, each held invariant by an override-consistency requirement (§5.4), plus `reference`, which is the grouping key. `sharing`, `updated_at`, `expires_at` and `owner_tenant_id` vary along a chain, so clamping them would change which row wins the reduction and could report an ancestor's credential as the effective one where a point read refuses it; they are filtered after reduction, or not at all. Even for the clampable fields the clamp only **narrows candidate references**: the rows of each candidate are then read whole and the winner is authorized, so no invariant violation can turn into a false catalogue entry (ADR-0005 "How authorization applies to a collection"). Only the tenant dimension is a gate rather than a predicate, and that asymmetry is deliberate, not an inconsistency to "fix" by adding a clamp.

Cross-tenant listing itself is still not provided: a parent that needs a descendant's catalogue acts in that tenant's context (§4.5), exactly as it already does for a point read (`cpt-cf-credstore-fr-service-retrieve`). Consequently credstore still projects no `tenant_closure`, requires no co-location with the Account Management database, and declares no new PEP capability for the collection; downward hierarchy knowledge stays entirely in the PDP, and upward hierarchy knowledge comes from the Tenant Resolver gear (below).

#### Tenant hierarchy (tenant-resolver)

- [ ] `p1` - **ID**: `cpt-cf-credstore-design-interface-tenant-resolver`

The gear fetches the requesting tenant's ancestor chain (self first, root last) with `BarrierMode::Ignore`: a `shared` secret is inherited by **all** descendants, including through `self_managed` (isolation-barrier) boundaries — publishing as `shared` is the owner's explicit sharing decision, and whether a caller may read at all is the PDP's decision, not the chain's. The chain carries no caller-specific data and is cached in-process with a TTL (`hierarchy.ancestor_cache_ttl_secs`, default 300 s) and LRU eviction.

**Why the barrier is bypassed here, and only here** ([ADR-0005](./ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md), `cpt-cf-credstore-adr-upward-collection-read`, status: proposed). Building an inheritance chain requires knowing **all** ancestors, so this ancestor-chain lookup is the single place in the gear that looks past an isolation barrier — deliberate, valid behaviour, not an oversight. A `self_managed` barrier isolates *management*, not previously published data: it stops a parent from administering a customer that runs its own subtree, but it does not retract a `shared` credential the parent already published downward — `cpt-cf-credstore-fr-hierarchical-resolve` states that consequence as a requirement (inheritance across `self_managed` boundaries). Four invariants keep the bypass narrow:

- it reads **ancestor identifiers only** — which of an ancestor's rows are then visible is still decided by the ordinary visibility rules, which admit `shared` rows and nothing else from an ancestor; `tenant` and `private` rows never leave their own tenant, barrier or not;
- it grants **no authority** — access is still decided by the PDP gate on the caller's own tenant, and role inheritance *into* a barrier tenant continues to respect barriers, so a parent still cannot manage or read inside a `self_managed` customer;
- it never traverses **downward** — the bypass makes ancestors visible to a descendant, never a subtree visible to an ancestor;
- it is confined to **one call site**, this ancestor-chain lookup, so the exception stays reviewable rather than becoming an ambient property of the gear.

In one sentence: data flows down through the barrier; authority does not.

#### Secret-type resolution & plugin discovery (types-registry)

- [ ] `p1` - **ID**: `cpt-cf-credstore-design-interface-gts`

The types-registry is a hard `deps` of the gear (fail-closed `init` when `TypesRegistryClient` is absent from ClientHub) and serves two roles:

**Secret-type resolution** (§5): every operation resolves the secret's stored type UUID via `get_type_schema_by_uuid` — one lookup against the registry client's built-in TTL cache; credstore adds no cache of its own, so type re-registrations take effect within the client TTL. The resolver (`GtsSecretTypeResolver`, mirroring AM's `GtsTenantTypeChecker`) verifies the schema descends from the secret base type (`gts.cf.core.credstore.secret.v1~`), merges the chain's effective traits (`x-gts-traits`, leaf wins, base fills defaults), and deserializes them into `SecretTypeTraits`. Failure mapping is fail-closed: an unregistered/non-secret type is `UNKNOWN_SECRET_TYPE` (400) when the caller named it, 503 when it came from a stored row (deregistration is an operational inconsistency, not a caller error); registry outage / timeout (2 s probe) / malformed traits → 503. Calls are recorded on the `types_registry` dependency-health metrics.

**Plugin discovery**: plugins register GTS instances derived from the plugin spec type (`…~cf.core.credstore.plugin.v1~`). The gear lazily queries instances by type-id prefix, filters by the configured `vendor`, picks the highest-priority active instance, and resolves its scoped `CredStorePluginClientV1` from ClientHub. No plugin available surfaces as a non-retryable 503 ("no storage plugin registered").

#### Sharing-mode transitions

`tenant ↔ shared` is an in-place metadata update (same unique-index class). `private ↔ (tenant|shared)` crosses key classes; since private and non-private secrets coexist by design, such a "transition" has no atomic meaning and is **rejected** as `UnsupportedTransition` (400) rather than performed non-atomically. Writes always address the row of their own sharing class, so a write of one class never affects the other.

### 4.5 Service-to-Service Pattern

Two integration patterns share one API:

1. **Self-Service**: gears and tenant admins operate on their own tenant; tenant and owner derive from their `SecurityContext`.
2. **Service-to-Service**: authorized service accounts (OAGW) act on behalf of arbitrary tenants by constructing a `SecurityContext` for the target tenant (S2S client-credentials exchange) and calling the same `get`.

**Flow (OAGW)**: OAGW builds a `SecurityContext` with the target tenant, calls `get(ctx, key)`; the PDP decides whether that subject may `read` secrets in that tenant's scope; resolution and metadata behave identically to self-service. Audit/metrics record the caller subject and target tenant. OAGW consumes credstore only through the SDK client — there is no separate integration path.

**Provisioning note**: with the stateful gear a provider's backend secret may not exist at startup (secrets are created at runtime through the credstore API; there is no startup seed). Consumers that provision infrastructure from secrets at boot (e.g. mini-chat registering OAGW upstreams) treat a failed secret lookup as non-fatal: the affected provider is skipped and remains unavailable until its secret is provisioned.

### 4.6 Interactions & Sequences

#### Hierarchical read

- [ ] `p1` - **ID**: `cpt-cf-credstore-seq-hierarchical-read`

```mermaid
sequenceDiagram
    participant C as Consumer
    participant GW as credstore
    participant TR as tenant-resolver
    participant DB as credstore_secrets
    participant GTS as types-registry
    participant PDP as authz-resolver
    participant P as Plugin

    C->>GW: get(ctx, key)
    GW->>TR: ancestor_chain(tenant) [cached, barriers ignored]
    TR-->>GW: [self, parent, ..., root]
    GW->>DB: resolve_for_get(reference, chain, subject) — one query
    DB-->>GW: winning row (or none → 404, no PDP call)
    GW->>GTS: get_type_schema_by_uuid(secret_type_uuid) [client TTL cache]
    GTS-->>GW: type id + effective traits
    GW->>PDP: access_scope(read, concrete type)
    PDP-->>GW: AccessScope
    GW->>DB: scope_includes_tenant(caller tenant)?
    GW->>P: get(owner_tenant, key, owner?) — value only
    P-->>GW: SecretValue
    GW-->>C: value + {owner_tenant_id, sharing, is_inherited, version, type}
```

**Resolution query semantics** (single indexed query over `idx_credstore_lookup`): filter by `reference`, tenant ∈ ancestor chain, `status = active`, and sharing-class visibility (`private` rows only for the caller's `owner_id`; `tenant` rows only for the requesting tenant; `shared` rows for any chain member). The winner is picked in-process: **closest tenant wins; `private` beats non-private at the same level**. The backend is read once, for the winning row only. Walk-up depth and read outcome (own/inherited/miss) are recorded as metrics.

**Shadowing**: a child's accessible secret always wins over an ancestor's; an *inaccessible* child secret (e.g. someone else's private) does not block fallback to an ancestor's shared secret.

#### Write (create saga) — see §6.2

- [ ] `p1` - **ID**: `cpt-cf-credstore-seq-write-saga`

The full step-by-step sequence, failure handling, and the overwrite path are described in §6.2 Provisioning Saga.

#### Delete (deprovisioning saga) — see §6.3

#### Credential listing (planned, ADR-0005)

- [ ] `p1` - **ID**: `cpt-cf-credstore-seq-list-credentials`

```mermaid
sequenceDiagram
    participant C as Consumer
    participant GW as credstore
    participant TR as tenant-resolver
    participant DB as credstore_secrets
    participant GTS as types-registry
    participant PDP as authz-resolver

    C->>GW: list(ctx, filter, orderby, cursor, limit)
    GW->>TR: ancestor_chain(tenant) [cached, barriers ignored]
    TR-->>GW: [self, parent, ..., root]
    GW->>PDP: access_scope(list_meta, …)
    PDP-->>GW: AccessScope (flat tenant predicate)
    GW->>DB: scope_includes_tenant(caller tenant)?
    GW->>DB: candidate rows across chain, reference ASC, id ASC — no tenant clamp; category/type/sharing/reference as SQL clamps
    DB-->>GW: candidate rows, page extended to the end of the last reference group
    GW->>GTS: get_type_schema_by_uuid per distinct secret_type_uuid on the page [client TTL cache]
    GTS-->>GW: type ids + effective traits
    GW->>PDP: access_scope(read_meta, per distinct type present)
    PDP-->>GW: per-type AccessScope
    GW-->>C: reduced items (own/inherited/overridden winner per reference) + next_cursor
```

The ancestor-chain fetch is the same `BarrierMode::Ignore` lookup as the point read (§4.4): for a tenant sitting behind an isolation barrier, this is exactly what makes its ancestors' `shared` rows candidates for the listing at all — data crosses the barrier, authority still does not, since the PDP gate below still targets only the caller's own tenant.

**Reduction and the cursor boundary.** Candidate rows are fetched across the ancestor chain under the point-read visibility rules (§4.1), sorted `reference ASC, id ASC`. Because the sort leads with `reference`, all rows of one reference are contiguous, so a page is extended to the end of the reference group it lands in, reduction picks one winner per group (`inheritance`: own/inherited/overridden), and the cursor always sits on a reference boundary — no winner can be split across pages (ADR-0005). Rows of a type the caller's scope does not admit are dropped after the query and the cursor still advances past them, so `items.len()` may be smaller than `limit`; clients treat `next_cursor`, not the item count, as the "more pages" signal, matching Account Management's own metadata listing.

#### Bulk secret read (planned, ADR-0004)

- [ ] `p1` - **ID**: `cpt-cf-credstore-seq-bulk-read-secrets`

```mermaid
sequenceDiagram
    participant C as Consumer
    participant GW as credstore
    participant TR as tenant-resolver
    participant DB as credstore_secrets
    participant GTS as types-registry
    participant PDP as authz-resolver
    participant P as Plugin

    C->>GW: read_secrets(ctx, references | filter)
    GW->>TR: ancestor_chain(tenant) [cached, barriers ignored]
    TR-->>GW: [self, parent, ..., root]
    GW->>DB: resolve candidates for up to cap+1 references — one query, own-tenant gate only
    DB-->>GW: rows (>cap ⇒ 400 TOO_MANY_MATCHES, no COUNT query)
    loop per resolved item
        GW->>GTS: get_type_schema_by_uuid(secret_type_uuid) [client TTL cache]
        GW->>PDP: access_scope(read_value, concrete type)
        PDP-->>GW: AccessScope
        GW->>DB: scope_includes_tenant(caller tenant)? and item visible?
        GW->>P: get(owner_tenant, key, owner?) — value only
        P-->>GW: SecretValue
        GW->>GW: verify value_fp (§4.10); mismatch ⇒ outcome "not_found"
    end
    GW-->>C: 200, per-item {outcome, secret?, credential?}, no cursor
```

**No pagination, per-item outcome.** Each item is authorized (`read_value`) and fenced independently; a refusal, a miss, a suppression and a fence mismatch all surface as the same `not_found` outcome and never abort the rest of the batch (§4.3.2). The response carries `returned` (the length of `items`) and the configured `cap`, never a cursor — the selector cannot be paginated through.

### 4.7 Database schemas & tables

The gear owns one table, `credstore_secrets` (migration `m0001_initial_schema`; raw per-backend SQL to preserve `CHECK` and partial-index semantics; PostgreSQL and SQLite; MySQL fails fast):

```sql
CREATE TABLE credstore_secrets (
    id         UUID PRIMARY KEY,
    tenant_id  UUID NOT NULL,
    reference  TEXT NOT NULL CHECK (length(reference) BETWEEN 1 AND 255),
    sharing    SMALLINT NOT NULL CHECK (sharing IN (1,2,3)),  -- private/tenant/shared
    owner_id   UUID NOT NULL,
    status     SMALLINT NOT NULL CHECK (status IN (1,2,3)),   -- provisioning/active/deprovisioning
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    version    BIGINT NOT NULL DEFAULT 1,
    secret_type_uuid UUID NOT NULL DEFAULT '<generic type v5 uuid>', -- deterministic v5 of the GTS type id
    expires_at TIMESTAMPTZ NULL,                                     -- expirable types only
    value_fp   BYTEA NULL,      -- value-fingerprint fence: HMAC-SHA256(fence_key, value); internal-only
    fp_key_id  SMALLINT NULL,   -- fence-key id the fp was computed under (keyring groundwork)
    CHECK ((value_fp IS NULL) = (fp_key_id IS NULL))  -- NULL only on out-of-band seeded rows
);
-- coexistence of a private and a tenant/shared secret under one reference:
CREATE UNIQUE INDEX uq_credstore_nonprivate ON credstore_secrets (tenant_id, reference)           WHERE sharing <> 1;
CREATE UNIQUE INDEX uq_credstore_private    ON credstore_secrets (tenant_id, reference, owner_id) WHERE sharing = 1;
-- walk-up resolution, reaper sweep (all non-active rows), expiry sweep:
CREATE INDEX idx_credstore_lookup  ON credstore_secrets (reference, tenant_id, status);
CREATE INDEX idx_credstore_pending ON credstore_secrets (updated_at) WHERE status <> 2;
CREATE INDEX idx_credstore_expiry  ON credstore_secrets (expires_at) WHERE expires_at IS NOT NULL AND status = 2;
```

Created by the single `m0001_initial_schema` migration (§8). The table is a `Scopable` SecureORM entity — PDP scope clamps are applied to every query, which is what makes authorization enforceable in SQL.

**Planned: `m0002` (ADR-0004, ADR-0005).** The `category` field (`cpt-cf-credstore-fr-secret-category`) and the filter/order allowlist behind the credential-record collection and the bulk secret read (§4.3.2, §4.4) require an additive migration on top of `m0001_initial_schema`, adding at least:

```sql
ALTER TABLE credstore_secrets ADD COLUMN category TEXT NULL;  -- validated at the domain layer (§5.2, §5.4)
CREATE INDEX idx_credstore_category ON credstore_secrets (tenant_id, category);
CREATE INDEX idx_credstore_type     ON credstore_secrets (tenant_id, secret_type_uuid);

-- The `declared` status (§6.1) needs both of these, not just the column above:
-- the initial CHECK admits 1..3 only, so `status = 4` cannot be stored, and
-- the reaper's pending index covers every non-active row, so a deliberately
-- long-lived `declared` record would be selected as a stale write and swept.
ALTER TABLE credstore_secrets DROP CONSTRAINT credstore_secrets_status_check;
ALTER TABLE credstore_secrets ADD  CONSTRAINT credstore_secrets_status_check
  CHECK (status IN (1, 2, 3, 4));
DROP INDEX idx_credstore_pending;
CREATE INDEX idx_credstore_pending ON credstore_secrets (updated_at)
  WHERE status IN (1, 3);   -- provisioning/deprovisioning only, never `declared`
```

The reaper's stale-row selection narrows with that index: it reads `status IN (1, 3)` rather than `status <> 2`, so `declared` is excluded by the same predicate that drives the sweep instead of by an exception inside it (§6.4). SQLite has no `DROP CONSTRAINT`, so on that backend the widened `CHECK` arrives by table rebuild or is simply absent — the domain layer is the enforcing party either way, and the constraint is a backstop.

`tenant_id` leads both, because no collection query omits the tenant-chain predicate. These two are not there for a future caller-supplied `$filter` only: they serve the **authorization clamp itself** (§4.4), which narrows candidate references by `category` and by type before the chain's rows are read. That is the difference between a category-scoped application reading its own handful of credentials and reading every credential it may see. `owner_id` is deliberately **not** indexed and not filterable: it selects the private key class, which resolution handles, and exposing it would let a caller probe other subjects' private references.

The exact column and index list is finalized with the migration; the ones above are the minimum the clamp and the allowlist rule below require.

**Current index gaps (before `m0002`).** As of `m0001_initial_schema`, `created_at` carries no index at all; neither `secret_type_uuid` nor `owner_id` is indexed on its own — the only index touching either column at all is `idx_credstore_lookup (reference, tenant_id, status)`, which names neither; `category` does not exist yet. `updated_at` is indexed only for **non-active** rows (`idx_credstore_pending (updated_at) WHERE status <> 2`), so it is unusable for filtering or ordering **active** rows — precisely the rows a listing or a bulk selector cares about. The platform rule that OData `$filter`/`$orderby` may only name indexed fields (ADR-0004, ADR-0005) is enforced by review today; there is no automatic check that a newly allowlisted field is actually indexed.

### 4.8 Deployment Topology

The gear is a gear inside the platform process; it requires a database (PostgreSQL in production, SQLite in dev/e2e) via the platform DB provider, and exactly one registered value-store plugin.

```mermaid
graph LR
    Platform["Platform<br/>(OAGW, mini-chat, gears)"] --> GW["credstore gear<br/>+ value-store plugin"]
    GW --> DB["PostgreSQL / SQLite<br/>(credstore_secrets)"]
    GW --> PDP["authz-resolver"]
    GW --> TR["tenant-resolver"]
```

Development/testing runs the in-memory `static-credstore-plugin`; production deployments substitute a real vault-backed plugin (future work) with no gear changes.

### 4.9 Technology Stack

| Layer | Technology | Rationale |
|-------|------------|-----------|
| Gear | Axum (REST), `#[toolkit::gear]` with `system, db, rest, stateful` capabilities | Platform standard |
| Persistence | SeaORM + SecureORM (`Scopable`), raw-SQL migrations | Scope clamps in SQL; partial unique indexes |
| AuthZ | `authz-resolver-sdk` `PolicyEnforcer` (PDP) | Platform policy plane |
| Hierarchy | `tenant-resolver-sdk` (`BarrierMode::Ignore`) + TTL/LRU cache | Full ancestor chains for upward inheritance |
| Discovery & typing | `toolkit-gts` / `types-registry-sdk` | Vendor-based plugin selection; runtime secret-type resolution (uuid → type id + traits) |
| Observability | OpenTelemetry metrics (typed port) | Operational visibility |
| Serialization | `serde` | Platform standard |
| Errors | `thiserror` + toolkit canonical errors | Platform standard ([ADR 0005](../../../docs/arch/errors/ADR/0005-cpt-cf-adr-sdk-canonical-projection.md)) |

### 4.10 Value-Fingerprint Fence

> Decision: [ADR-0003](./ADR/0003-cpt-cf-credstore-adr-value-fingerprint-fence.md) (`cpt-cf-credstore-adr-value-fingerprint-fence`). Closes the cross-tenant disclosure of two crosswise concurrent last-writer-wins (`If-Match: *`) PUTs and the ABA lost-update of a recreated secret.

A secret is a **dual write**: the value goes to the external value-store backend (`plugin.put`), the metadata (`sharing`, `version`, `expires_at`) to `credstore_secrets`. No transaction spans both stores, so two concurrent last-writer-wins PUTs (`If-Match: *`) to the same reference can interleave crosswise and commit one caller's value under the other caller's sharing label — a durable cross-tenant disclosure when the surviving label is `shared`. The mandatory write precondition does not close this: `Exists` writers still race, a version-gated writer that crashes between the metadata commit and the backend write leaves the same cross-writer mismatch, and no API-level precondition can bind two transaction-less stores — only the read-side fence can. The platform's coordination primitives deliberately exclude fencing of external effects ([cluster ADR-002](../../system/cluster/docs/ADR/002-async-boundary-no-remote-in-critical-section.md): no fencing tokens, no remote calls in a lock's critical section; `coord` leases likewise), so the fence lives at the application layer, as that ADR prescribes.

**Mechanism.** Each row stores `value_fp = HMAC-SHA256(fence_key, value)` of the value its metadata was written for. Every write stamps it in the same atomic `touch`/insert as `sharing`; every read recomputes it from the value the backend returned and serves the value only when the row agrees, else fails closed as an anti-enumeration miss (404) — with one deliberate exception: an out-of-band seeded row whose `value_fp` is still `NULL` is served on trust until backfilled (see **Out-of-band seeding** below); API-written rows always carry a fingerprint. Because the fingerprint and the sharing label are written by one writer in one UPDATE, a fingerprint match transitively proves the value and the metadata are from the same PUT — a value can never be served under a sharing label a different writer set. Metadata is not hashed into the fingerprint (the atomic write is what binds them).

**Fence key.** Deployment state, not configuration: auto-generated (OS RNG, 32 bytes) and stored in the value-store backend under the reserved entry `(tenant = nil, reference = "cfs-internal-fence-key", owner = None)`. It has no metadata row, so no API path resolves, overwrites, or deletes it (resolution requires a row; external callers always carry a real tenant). All replicas read the one shared entry; on a virgin deployment the first writer generates it (put-then-reread convergence). The service caches it in-process and, on a fingerprint mismatch, performs a one-shot re-read (a replica whose cached key went stale after a key re-creation self-heals before failing closed). Split knowledge: fingerprints live in the gear DB, the key with the values — a DB-only compromise yields HMACs under an unknown key (no offline dictionary attack), and backend compromise yields the plaintexts regardless, so the fence adds no attack surface.

**Failure semantics** are always fail-closed and self-healing: a half-completed overwrite (backend write committed, metadata `touch` failed, or vice-versa) leaves the row fingerprinting a different value than the backend holds, so reads 404 until a retried PUT lands both; a lost/replaced key makes fenced reads 404 (observable via `fence_verify{outcome="mismatch"}`) and re-PUT re-stamps each secret. The healing PUT uses `If-Match: *`: the poisoned read is a fail-closed 404, so no version validator can be obtained — which is exactly why the explicit `*` form remains part of the mandatory-precondition contract. No failure mode serves a value under mismatched metadata.

**Generation-bound validator.** The strong `ETag` is `"<row-id>.<version>"`. The row UUID is minted fresh per created secret, so a validator from a deleted-and-recreated secret's earlier generation never matches the new row even when the restarted version counters coincide (closing the ABA lost-update). `version` stays the per-generation optimistic-lock counter and the row-level CAS still gates on it (the id is already the UPDATE key).

**Out-of-band seeding.** A row provisioned directly in the DB with `value_fp = NULL` (value placed directly in the backend) is served on trust and its fingerprint backfilled — lazily on first read, or by the reaper's bounded sweep for rows nobody reads (both via a `value_fp IS NULL` CAS that never bumps the version). A re-seed of an existing row must reset `value_fp` to NULL in the same operation. This is the only path that produces a NULL fingerprint; API writes always stamp.

## 5. Secret Types (GTS-Based, Registry-Driven)

> Requirement: `cpt-cf-credstore-fr-secret-types`. Implemented: registry seeds in the SDK + runtime resolution (`SecretTypeResolver`), gear trait enforcement on the resolved traits (incl. the per-type PDP gate), UUID type column in `m0001_initial_schema`.

### 5.1 Concept

Today every secret is an opaque byte string with identical semantics. The platform, however, stores materially different kinds of secrets (LLM provider API keys consumed by OAGW, OAuth2 client credentials, personal tokens, certificates), and their handling rules differ — most importantly *whether a secret may be shared down the tenant hierarchy at all*.

A **secret type** is a GTS type derived from the credstore secret base type (GTS segments are `vendor.package.namespace.type.vN`, so the derived segment carries the type name directly), e.g. for the built-in types:

```
gts.cf.core.credstore.secret.v1~cf.core.credstore.<name>.v1~
```

Each type declares a set of **traits** — machine-readable behavioral properties the gear enforces uniformly. The **types-registry is the runtime source of truth** (mirroring tenant types in Account Management): the base type `SecretV1` carries the trait vocabulary as its `x-gts-traits-schema` (generated from `SecretTypeTraits`, closed to unknown keys), every registered type derived from it declares its `x-gts-traits` values against that shape, and the gear resolves a type's effective traits from the registry per operation (§5.4). The compiled-in SDK catalog (`credstore_sdk::SECRET_TYPE_CATALOG`) only **seeds** the built-in type schemas through the link-time inventory; unit tests pin the seeds to the catalog descriptors so the two views cannot drift.

**Adding a type requires no credstore release**: registering a GTS schema that descends from `gts.cf.core.credstore.secret.v1~` (with its `x-gts-traits`) makes the type immediately writable, trait-enforced, and addressable as a PDP resource type — enabling per-type RBAC (e.g., a role that may read `api-key` secrets but not `certificate` secrets) without new authorization machinery.

The type of a secret is chosen at creation (REST field `type`: the secret type's full GTS type id), defaults to `generic`, and is **immutable** for the lifetime of the secret (rejected with `TYPE_IMMUTABLE`, mirroring the private ↔ non-private rule).

### 5.2 Type Traits

| Trait | Type | Semantics (gear-enforced unless noted) |
|-------|------|-------------------------------------------|
| `allow_sharing` | list of `SharingMode` | Sharing modes permitted for secrets of this type. A `put`/`create` with a mode outside the list is rejected (400, `SHARING_NOT_ALLOWED_FOR_TYPE`) — including a disallowed mode change on update. The traits schema constrains entries to the `SharingMode` enum, so a typo fails at registration. |
| `value_schema` | embedded JSON Schema (optional) | Structural validation of the (JSON) value on write (400, `VALUE_SCHEMA_VIOLATION`); violation details never echo the value. Absent ⇒ opaque value. Carried in `x-gts-traits` like every other trait; the validator is compiled per write (schemas are dynamic; the registry client caches the resolution). A registered schema that fails to compile is a broken registration → 503, not 400. |
| `max_size_bytes` | integer (optional) | Upper bound on value size (400, `VALUE_TOO_LARGE`); absent ⇒ platform default only. |
| `expirable` | bool | Whether secrets of this type may carry `expires_at` (else 400, `EXPIRY_NOT_SUPPORTED_FOR_TYPE`; a past expiry is `EXPIRY_IN_THE_PAST`); expired secrets resolve as 404 (read-time SQL filter) and are moved into the deprovisioning saga by the reaper (§6.4). |
| `rotation_period_secs` | integer (optional, advisory) | Recommended rotation cadence; metadata-only — rotation automation stays a non-goal. |
| `utf8_only` | bool | Whether the value must be valid UTF-8 (400, `VALUE_NOT_UTF8`; only `generic` currently allows binary, reachable via the SDK). |
| `allowed_categories` (planned, `cpt-cf-credstore-fr-secret-category`) | list of category labels (optional) | Closed allowlist of the `category` values (§4.1) permitted on records of this type. A record write naming a category outside both this list and the platform category registry is rejected (400). Absent ⇒ any registry category is allowed. |

Trait values resolve through the GTS chain merge (`effective_traits`): leaf-declared values win, ancestors fill the rest — the base type declares generic values for every trait, so a derived type only states what it restricts. Traits are enforced in the gear domain layer at well-defined points (§5.4); plugins remain trait-agnostic value stores.

### 5.3 Built-in Type Catalog (Registry Seeds)

Built-in types seeded into the registry by the SDK (REST name → derived GTS segment uses `_`, e.g. `api-key` → `…~cf.core.credstore.api_key.v1~`):

| Type | `allow_sharing` | `value_schema` | Notes |
|------|-----------------|----------------|-------|
| `generic` | private, tenant, shared | — | Default; fully backward-compatible with existing secrets (binary allowed). |
| `api-key` | private, tenant, shared | — | Third-party provider keys (OpenAI etc.) — the core hierarchical-sharing use case (OAGW). |
| `personal-token` | **private only** | — ; `expirable` | Personal access tokens; the flagship `allow_sharing` restriction — can never be shared tenant-wide or inherited. |
| `oauth2-client` | tenant, shared | `{client_id, client_secret, [token_url, scopes]}` | Structured; consumed by OAGW OAuth2 client-credentials auth. |
| `basic-auth` | private, tenant, shared | `{username, password}` | Structured HTTP basic credentials. |
| `bearer-token` | private, tenant | — ; `expirable` | Short-lived tokens; expiry enforced on read. |
| `certificate` | tenant, shared | — ; `expirable`, `rotation_period_secs` = 90 d advisory | TLS material; `not_after` maps to `expires_at`. Format (PEM) checks deferred. |
| `ssh-key` | private, tenant | — | Deploy/automation keys. Format checks deferred. |
| `webhook-hmac` | tenant, shared | — | Webhook signing secrets shared with descendant integrations. |
| `connection-string` | tenant | — ; `max_size_bytes` = 4 KiB | DSNs; tenant-local by policy (contain embedded endpoints/credentials). |

Adding a **built-in** type (shipped with the platform, with a short REST name) = adding a catalog entry + its seed registration in the SDK (one release). Adding a **custom** type = registering a GTS schema derived from the secret base type — no release; the type is addressed by its full GTS id on the API. The enforcement code changes only when a new *trait* is introduced.

### 5.4 Enforcement Points

1. **Type resolution** (every operation): the type UUID — from the stored row (read/overwrite/delete prefetch) or from the request (create; default `generic`) — is resolved through `SecretTypeResolver` against the types-registry: envelope check (must descend from `gts.cf.core.credstore.secret.v1~`) + effective-traits merge (§4.4). Unknown/non-secret type: `UNKNOWN_SECRET_TYPE` (400) on create, 503 for a stored row; registry outage/timeout/malformed traits: 503. No credstore-side cache — the registry client's TTL cache bounds both latency and staleness.
2. **Create / put**: validate `sharing ∈ allow_sharing`, value against the `value_schema` trait (compiled per write), size against `max_size_bytes`, UTF-8 against `utf8_only`, and the expiry gate — all on the **resolved traits**, before any side effect. Violations → 400 (`InvalidArgument`) with the stable per-trait reason (§5.2).
3. **Update**: type immutable (`TYPE_IMMUTABLE` when an explicit differing `type` is sent — compared by UUID; absent `type` inherits the row's); the new value/sharing/expiry re-validated against the resolved traits. A PUT is a whole-value replace: omitting `expires_at` clears a stored expiry.
4. **Read**: rows with `expires_at <= now` are filtered out in the resolution SQL (404); `type` (and `expires_at`, when set) are returned in response metadata.
5. **Authorization**: a **single** PDP evaluation per operation targets the secret's **full concrete GTS type** — including `generic` — as returned by the type resolution (step 1). Its `AccessScope` is enforced in SQL and its gate must include the **caller's** tenant (hierarchical visibility of inherited/shared secrets is decided by the resolver, not the PDP). Denial surfaces as the anti-enumeration 404 on read and 403 on write/delete; a PDP outage is 503. Every type (incl. `generic` and custom types) reaches the PDP, so a per-type policy can be added with no credstore change. On read the PDP is consulted only for a secret that resolves (a missing secret is a 404 without a PDP or registry call).
6. **Reaper**: each tick first flips expired `active` rows into the ordinary deprovisioning saga (`mark_expired_deprovisioning`), which then cleans the backend value and releases the reference via the pending sweep (§6.4). The reaper never resolves types — sweeping is type-agnostic.
7. **Category** (planned, record write only, ADR-0004): the `category` field is validated against the registered category instances (§5.5) — a name that is not registered, or whose instance is `deprecated`, is refused and, when the type declares `allowed_categories` (§5.2), against that closed list — a violation is 400. Because changing a record's category changes which application may read its value through a category-scoped grant (and, under the bulk selector, which credentials appear in that application's result), a category change requires the `write_meta` action and bumps `version` like any other metadata edit (§4.3.2, `cpt-cf-credstore-fr-secret-category`). **The category must also match the credential this record overrides**, when the reference currently resolves to an ancestor's `shared` credential (`cpt-cf-credstore-fr-override-category-consistency`, the exact parallel of the type rule above): the write resolves the reference upward, which it must do anyway, and refuses a differing category as a conflict. This is enforced here rather than assumed, because §4.4's `category` clamp is only cheap if a reference's category is constant across its chain. The one path this check cannot cover is an ancestor changing or recreating its own credential, which no upward read can validate against descendants; §4.4 therefore reads the winner back rather than trusting the clamp, and the reaper raises a metric when it finds a chain with mixed categories.

### 5.5 Category Registry (planned, ADR-0004)

The category vocabulary lives in types-registry, as **instances** of one GTS type. Not a gear-config key, and not a table in this gear: the label is named by two independent parties, the write path here *and* the author of the PDP policy that grants it, and the policy author sits in the authorization engine. A vocabulary only this gear can see cannot be offered as a dropdown or validated when the policy is written. types-registry is the platform's registry and this gear already calls `list_instances` against it to select its backend plugin, so this adds no dependency and inherits that call's failure mode (registry unreachable ⇒ fail closed, 503).

**Type**: `gts.cf.core.credstore.category.v1~`. Its instances carry a short `name` — the string that goes in the `category` column, in a `$filter`, and in a policy constraint — plus a human description and a lifecycle flag:

```jsonc
// gts.cf.core.credstore.category.v1~cf.core.credstore.email_sender.v1
{ "name": "email-sender",
  "title": "Outbound email",
  "description": "SMTP and mail-API credentials an outbound mail service may read.",
  "state": "active" }
```

```jsonc
// gts.cf.core.credstore.category.v1~cf.core.credstore.payments.v1
{ "name": "payments",
  "title": "Payment processing",
  "description": "Payment-gateway keys. Separated from every other category so a mail service's grant can never reach them.",
  "state": "active" }
```

```jsonc
// gts.cf.core.credstore.category.v1~cf.core.credstore.branding.v1
{ "name": "branding",
  "title": "Branding and content",
  "description": "Non-secret-shaped tenant content served through the credential store (slogans, asset tokens).",
  "state": "active" }
```

```jsonc
// gts.cf.core.credstore.category.v1~cf.core.credstore.smtp.v1
{ "name": "smtp",
  "title": "SMTP (deprecated)",
  "description": "Superseded by `email-sender`. Kept registered so existing records keep resolving.",
  "state": "deprecated" }
```

**Who registers them**: the platform operator, by seeding instances — the same per-deployment seed path types-registry already uses for the operator-controlled platform-root tenant type (§10). A tenant cannot, because instance registration is not a tenant operation, which is what makes the PRD's "closed set maintained by the platform, not defined ad hoc by individual tenants" true by construction rather than by convention.

**Why instances rather than one type per category**: a type per category would put the full GTS id in the column, in every `$filter` and in every policy constraint, and would make the vocabulary a set of schemas rather than a set of values. An instance is a registered *value* with a short name, which is what a label is.

**Lifecycle, and the direction that actually bites.** Registering a category before any policy grants it is harmless: records may carry it and no one may read them, which is a closed refusal. The dangerous direction is removal — a record keeps a label whose registration is gone while policies keep referencing the string. types-registry cannot see this gear's rows, so only this gear could check it, and it would have to check on every deletion. So **categories are not deleted, they are deprecated**: `state: "deprecated"` keeps existing records resolving and existing policies working, while a record write naming a deprecated category is refused. The reaper's consistency scan (§6.4) additionally reports records whose category is not registered at all, which is the only way a label can go stale once removal is off the table.

**Relationship to `allowed_categories` (§5.2)**: two gates, not one. This registry says which names exist; the type trait says which of them are legal on records of that type. A record write checks both, and both speak the same short names.

### 5.6 Storage & API Changes

- `credstore_secrets` carries `secret_type_uuid UUID NOT NULL DEFAULT '<generic v5 uuid>'` — the deterministic v5 UUID of the type's GTS id (`GtsID::to_uuid`, pinned as `credstore_sdk::GENERIC_TYPE_UUID_STR`), like AM's `tenants.tenant_type_uuid` — and `expires_at TIMESTAMPTZ NULL`, plus the partial expiry-sweep index (§4.7, §8). The stored UUID is opaque to the storage layer; only the type resolution interprets it.
- REST: optional `type` (the secret type's full GTS type id) and `expires_at` (RFC 3339) on `POST`/`PUT`; `GET` metadata returns `type` (the resolved full GTS type id) and `expires_at`.
- SDK: `CredStoreClientV1` gains `put_opts`/`create_opts` taking `WriteOptions { secret_type: Option<GtsId>, expires_at }` — the gear resolves the `GtsId` to the type's deterministic UUID; `put`/`create` are provided methods delegating with defaults. The write precondition is a required `put`/`put_opts`/`delete` argument, not a `WriteOptions` field. `GetSecretResponse.secret_type` is the resolved full GTS type id.
- Plugin SPI: **unchanged** — types are a metadata/policy concern.

## 6. Secret Lifecycle & Sagas

### 6.1 Status Model

```
                 create saga                     delete saga
  ┌──────────────┐  plugin.put OK  ┌────────┐  mark deprovisioning  ┌────────────────┐
  │ provisioning ├────────────────►│ active ├──────────────────────►│ deprovisioning │
  └──────┬───────┘                 └────────┘                       └───────┬────────┘
         │ backend failure → rollback (row deleted)                         │ plugin.delete OK
         │ stuck → reaped                                                   │ → row deleted
         ▼                                                                  ▼ stuck → reaped
       (gone)                                                             (gone)
```

| Status | smallint | Visible to resolution | Holds unique index | Swept by reaper |
|--------|----------|----------------------|--------------------|-----------------|
| `provisioning` | 1 | no | yes | yes (after `provisioning_timeout_secs`) |
| `active` | 2 | **yes** | yes | no |
| `deprovisioning` | 3 | no | yes | yes (after `deprovisioning_timeout_secs`) |
| `declared` (planned, ADR-0004) | 4 | no | yes | **no** |

Only `active` rows are returned by `resolve_for_get`; a secret therefore becomes visible atomically at saga commit and invisible atomically at delete start.

**Declared (planned, ADR-0004).** A record created by `PUT /credentials/{ref}` before its value is ever written (§4.1, §4.3.2) is a distinct, deliberately long-lived resting state, not a mid-saga step. It differs from `provisioning` in exactly the property the reaper cares about: `provisioning` is always mid-saga and bounded by `provisioning_timeout_secs`, so a row stuck there past that timeout is by definition a crashed write and safe to reap; a `declared` record has no saga in flight and no timeout applies to it, because its value `PUT` may legitimately arrive much later than any saga timeout, or never. The stuck-`provisioning` sweep (§6.4) MUST NOT reap `declared` rows.

It is also distinct from the existing `value_fp IS NULL` out-of-band-seeding case (§4.10), and the two behave **oppositely** on a value read: a seeded row is `active` and has a backend value, so it resolves and is served on trust until its fingerprint is backfilled; a `declared` row has no backend value at all, is not `active`, and is never a resolution candidate. The non-shadowing rule (§4.1) belongs to the `declared` row alone, and only because it never competes: a seeded row *does* shadow an ancestor's `shared` value, legitimately, being both nearer and resolvable — which is ordinary resolution, not an exception. Stating it as "neither state shadows" would be wrong about the seeded half.

**How a record reaches it: one insert, no saga.** A record write touches the metadata table and nothing else — there is no backend value to write, so there is nothing to compensate and no intermediate state to pass through. `PUT /credentials/{ref}` with `If-None-Match: *` therefore inserts directly at `status = 4`, without the `provisioning` step the value-carrying create needs (§6.2). That is what makes the two incomplete outcomes distinguishable, and the distinction is the whole reason this status exists:

| What happened | Status left behind | Reaped |
|---|---|---|
| The record write succeeded, the value write never came | `declared` (4) | no — no timeout applies, the value `PUT` may arrive much later or never |
| The record write itself failed midway | nothing — a single insert either commits or does not | n/a |
| A value-carrying create crashed between its metadata insert and its backend write | `provisioning` (1) | yes, past `provisioning_timeout_secs` — a row stuck there is by definition a crashed write |

**It is a fourth `status`, not an inference.** `status = 4`, widening the `CHECK (status IN (1, 2, 3))` of `m0001_initial_schema`, and *not* derived from `value_fp IS NULL`, which already means "seeded out of band, fingerprint pending" and would conflate a deliberately empty record with a legacy row whose backend value must resolve on trust. Making it a stored status is what keeps every predicate that has to distinguish the two expressible in SQL rather than in application logic:

| Predicate | Today | With `declared` |
|---|---|---|
| Resolution (`resolve_for_get`, `list_candidates_for_records`) | `status = 2` | `status = 2` — unchanged, so a `declared` row neither resolves nor shadows, which is the requirement |
| Reaper's stuck-provisioning sweep | `status = 1 AND updated_at < …` | unchanged, so `declared` is excluded by construction rather than by a new exception |
| Unique-name hold | partial unique indexes over all statuses | unchanged, so a `declared` record still reserves its reference |
| Collection read | `status = 2` | `status IN (2, 4)`, with the record's state surfaced to the caller — a catalogue that hid declared-but-unset credentials would be useless to the administrator who is mid-configuration |

The consequence worth stating: the collection read is the only surface where the two statuses diverge, and it is the reason a record's state has to appear in the record's representation rather than being inferred from a missing value the response never carries.

**Suppression (P2, `cpt-cf-credstore-fr-suppression`, not yet designed in full).** If adopted, suppression is a separate per-record state — not a value and not a deletion — that a tenant sets on its own record to declare an ancestor's credential disabled for itself. A suppressed record wins hierarchical resolution at that tenant and its descendants (§4.1): value reads there resolve as not-found, while the ancestor's record and value are left untouched.

### 6.2 Provisioning Saga

Sequence ID: `cpt-cf-credstore-seq-write-saga` (declared in §4.6).

**Create path** (no row of the target sharing class exists):

1. Fail fast if no plugin is available (before any metadata side effect).
2. `INSERT` row with `status = provisioning` (scope-clamped).
3. `plugin.put(tenant, key, value, owner-class)`.
4. `UPDATE status = active` (`mark_active`).

**Failure handling**:

- Step 3 fails → **compensate**: best-effort delete of the provisioning row (metric `provisioning_rollback`); a failed cleanup defers to the reaper. The reference is never permanently wedged.
- Step 4 fails → the backend value exists but the row stays `provisioning`: never readable; a client retry overwrites it; otherwise reaped — and the reaper issues a best-effort `plugin.delete` for every row it reaps (§6.4), so the orphaned backend value is reconciled rather than leaked.
- **Create race** (unique-index conflict on step 2): a retryable 409. Only create-only specs reach the insert — an update fails earlier on its mandatory precondition — so the loser's own retry (or the caller's create→put fallback) resolves the race once the winner marks active.

**Overwrite path** (row of the target class exists): the ordering of the backend write vs the version-gated `touch` (version bump + sharing update + fence stamp) depends on the precondition kind. **`If-Match: *`** (explicit last-writer-wins): backend `plugin.put` first, then `touch` — a plugin failure leaves metadata untouched; a `touch` failure after a committed backend write fails closed (fence mismatch → 404) until the next put retries; a row that vanished concurrently (delete won) is reported as a retryable 409 so the caller re-runs the put. **Version validator** (optimistic concurrency): the version-gated `touch` claims the CAS **before** `plugin.put`, so a losing writer (0-row `touch` → 409) never reaches the backend and cannot clobber the winner's value; a backend failure after the claim fails closed the same way until the caller retries against the new version.

### 6.3 Deprovisioning Saga

> Requirement: `cpt-cf-credstore-fr-deprovisioning`. Status-driven saga symmetric to provisioning (statuses ship in `m0001_initial_schema`); replaced the earlier backend-first delete design.

**Delete path**:

1. `find_own` + version pre-check (`If-Match`, mandatory — `*` gates on existence only), as today.
2. `UPDATE status = deprovisioning` gated by `version = ?` when a version validator was given. From this instant the secret is invisible to resolution (single-status filter — no read/delete race), while the row keeps holding the partial unique index.
3. `plugin.delete(tenant, key, owner-class)` — `NotFound` is success (idempotent).
4. `DELETE` the metadata row.

**Failure handling**:

- Step 3/4 fails → the row stays `deprovisioning`; the caller gets the mapped error (503 for an unavailable backend, 500 for an internal plugin fault) and can retry. A **retry of `DELETE` resumes the saga**: `find_own` sees the `deprovisioning` row and re-runs steps 3–4 (both idempotent). Absent a retry, the reaper completes it.
- Crash between 2 and 4 → same recovery: reaper or client retry finishes cleanup. No state leaves a readable secret or a permanently held reference.

**Concurrent create during deprovisioning**: the row keeps the unique index until step 4, so a `POST`/`PUT` for the same reference conflicts (409, retryable) until cleanup completes. This is deliberate: releasing the index earlier would let a new secret's backend value be deleted by the old row's lagging step 3 (the backend key is identical). The window is bounded by the reaper cadence.

**Interface impact**: REST/SDK signatures unchanged (`DELETE` stays 204 on success); `SecretStatus` has `Deprovisioning = 3` (status `CHECK` in §4.7); config has `reaper.deprovisioning_timeout_secs` (default 300 s); metric `deprovisioning_reaped` mirrors `provisioning_reaped`.

### 6.4 Reaper

The gear's lifecycle entry (`serve`) runs a cancellable loop on `reaper.tick_secs` (default 60 s; delayed missed-tick behavior). Each tick:

1. **Expiry sweep**: flip expired `active` rows (`expires_at <= now`) into the deprovisioning saga (they already stopped resolving at read time; this cleans the backend value and releases the reference).
2. **List stale non-active rows** (bounded batch) older than `provisioning_timeout_secs` / `deprovisioning_timeout_secs` (both default 300 s), via the pending partial index. Planned with ADR-0004: the selection reads `status IN (1, 3)` rather than `status <> 2`, so a `declared` record — a resting state with no timeout — is never mistaken for a crashed write (§6.1, §4.7).
3. **Backend reconciliation**: issue a best-effort `plugin.delete` for every stale row (closing the orphaned-value debt of §6.2); `NotFound` counts as success.
4. **Remove rows**: `provisioning` rows unconditionally (the reference must not stay wedged); `deprovisioning` rows only after a successful backend delete — otherwise the row (and the name it holds) waits for the next tick. Counts → `provisioning_reaped` / `deprovisioning_reaped`. The asymmetry is deliberate: a `deprovisioning` row must hold the name until the backend is clean (releasing it early could let this saga's lagging delete erase a successor's value), while keeping a `provisioning` row on a failed backend delete would wedge the reference for as long as the backend stays unreachable. The cost is a possible orphaned backend value with no metadata row when step 3 fails for a reaped `provisioning` row — it is never readable (resolution requires a row) and is overwritten by the next create of the same reference; the accepted tradeoff mirrors §6.2's no-retry orphan.
5. **Refresh inventory gauges** (row counts per status).
6. **Category-consistency scan** (planned, ADR-0005): find references whose rows carry more than one `category` within a chain and raise a metric per finding. The write path refuses such a write (§5.4), so a finding means the one path a write cannot check — an ancestor changed or recreated its own credential — and the catalogue's `category` clamp is only as selective as this invariant is true. The same pass reports records whose category is not a registered instance at all (§5.5) — since a registration is deprecated rather than deleted, that can only happen through direct data manipulation, which is exactly why it is worth reporting. The scan detects, it does not repair: which category is the right one is an operator's decision, not the reaper's. Correctness does not wait for it, because §4.4 authorizes the reduced winner rather than trusting the clamp.

Errors are logged, never propagated — the reaper must survive transient DB or plugin outages.

## 7. Risks / Trade-offs

### 7.1 Architectural Trade-offs

#### Stateful gear (metadata table) vs stateless pass-through

**Decision**: the gear owns metadata; backends store values only.

- ✅ Hierarchical resolution and authorization in one transactional, indexed SQL query — latency independent of hierarchy depth on the metadata side
- ✅ No backend metadata-schema prerequisite; any dumb value store qualifies as a backend
- ✅ Sharing/uniqueness rules enforced by partial unique indexes rather than by backend-specific behavior
- ✅ No encoded-external-ID collision surface
- ❌ The gear needs a database and migrations (`stateful` capability)
- ❌ Metadata and backend value can diverge transiently — mitigated by the saga + reaper design (§6) and idempotent retries

#### Authorization via PDP scope in SQL (not permission strings)

**Decision**: PDP `AccessScope` + SecureORM clamps instead of coarse `Secrets:Read`/`Secrets:Write` permission checks.

- ✅ Real tenant isolation enforced at the data layer, consistent with platform RBAC/PDP
- ✅ No projection tables: subtree grants arrive pre-expanded from the PDP. This remains sufficient once the collection read is upward-rooted ([ADR-0005](./ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md)): the gear never expands a subtree, so no `tenant_closure` projection and no co-location with the Account Management database are required
- ❌ Every operation pays a PDP evaluation (timed as a dependency metric; 503 on PDP outage — fail-closed)

#### Three-tier sharing model (not RBAC/ABAC per secret)

Unchanged from the original design: simple to reason about, covers the primary use cases; per-secret ACLs remain out of scope. Secret types add a *type-level* restriction axis (`allow_sharing`, §5) without introducing per-secret ACLs.

#### 404 for inaccessible secrets (not 403)

Unchanged: prevents enumeration; per-secret access failures are indistinguishable from absence. 403 is reserved for PDP-level denial of the *operation* (e.g., subject may not `read` at all) and the own-tenant gate.

#### PUT-as-guarded-update + POST-as-create-only

`PUT` is an update of an existing secret with a mandatory `If-Match` (a version validator, or `*` as the explicit last-writer-wins opt-in) and never creates; `POST` is create-only with 409 on same-class conflict — the one preconditionless write. Blind create-or-replace flows compose the two: `POST`, and on 409 `PUT` with `If-Match: *` (retrying on 409 if the reference is deleted concurrently).

### 7.2 Security and Performance Risks

#### Risk: Secret values leaked through logs or caches

**Mitigation**: `SecretValue` redaction + zeroize; hand-written redacted `Debug` on DTOs; `Cache-Control: no-store`; no lossy UTF-8 decode; code review. **Likelihood**: Medium | **Impact**: Critical | **Priority**: P1

#### Risk: Metadata/backend divergence (saga partial failure)

**Impact**: value-less rows (404 until re-put), orphaned backend values (storage leak, never readable), wedged references behind the unique index.

**Mitigation**: compensating rollback on create; backend-first ordering on overwrite; the deprovisioning saga + reaper backend reconciliation (§6.3, §6.4); configurable timeouts; rollback/reap metrics for operational visibility.

**Likelihood**: Medium | **Impact**: Medium | **Priority**: P1

#### Risk: PDP or tenant-resolver outage

**Impact**: all operations fail (503) — fail-closed by design.

**Mitigation**: ancestor-chain TTL cache absorbs short tenant-resolver blips; dependency latency/outcome metrics; PDP evaluation is per-request with no local policy cache (deliberate: policy freshness over availability).

**Likelihood**: Low | **Impact**: High | **Priority**: P1

#### Risk: Ancestor-chain cache staleness

**Impact**: up to `ancestor_cache_ttl_secs` (300 s) of stale hierarchy — a re-parented tenant may briefly resolve secrets along the old chain.

**Mitigation**: short TTL + LRU; the chain carries no caller-specific data (safe to share across security contexts).

**What the mitigation does not cover, stated rather than implied.** The claim "a stale chain widens candidate rows but never authorized rows" holds for the *type* and *attribute* dimensions, which are checked against the row. It does **not** hold for the tenant dimension: the own-tenant gate validates the caller's own tenant, never the ancestors the chain names, because that is exactly what makes an inherited read work. So after a tenant is re-parented, a cached chain can keep naming the former parent, and that parent's `shared` rows keep resolving for up to the TTL. It is a bounded cross-tenant disclosure — one TTL wide, `shared` rows only, and only for a tenant that was actually moved — but it is a real one, and the gate is not the control for it.

Closing it needs a signal this gear does not have: a hierarchy version or change notification from the Tenant Resolver, checked or subscribed to before a chain is trusted. That is work outside credstore, so the window is accepted here and recorded as a risk (§12) rather than designed away. Operators re-parenting a tenant should treat the TTL as the settling time for credential inheritance.

**Likelihood**: Low | **Impact**: Medium | **Priority**: P2

#### Risk: Reference wedged by stuck saga rows

**Impact**: 409 on create for up to the reaper timeout after a crashed write (or delete, once deprovisioning ships).

**Mitigation**: bounded create-race retries; rollback-before-reaper on the common failure path; configurable `provisioning_timeout_secs` / `deprovisioning_timeout_secs`; `provisioning_reaped` alerting.

**Likelihood**: Low | **Impact**: Low | **Priority**: P2

## 8. Migration Plan

Schema is managed by SeaORM migrations (raw per-backend SQL, PostgreSQL + SQLite, MySQL fails fast). The stateful gear, the deprovisioning saga, and secret types shipped together — before any deployment of this gear — so the gear starts from one consolidated migration:

- **`m0001_initial_schema`**: the full `credstore_secrets` table of §4.7 — lifecycle statuses `(1,2,3)`, the monotonic `version`, `secret_type_uuid` (default = the generic type's v5 UUID) + `expires_at`, partial unique indexes, and the lookup / pending-sweep / expiry-sweep indexes.

Future schema changes are additive migrations on top. **Backward compatibility** for clients: untyped writes behave exactly as before (`generic` type, all sharing modes, no expiry). Rollback = revert the gear and run the migration `down`.

## 9. Open Questions

1. ~~**Batch retrieval**~~ **Answered** by [ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md): a dedicated, non-paginated bulk address (`cpt-cf-credstore-fr-bulk-read-secrets`), selected by an explicit reference list or by a filter over allowlisted metadata fields, authorized and fence-verified per item, capped by cardinality.
2. ~~**Human vs service access**~~ **Answered** by `cpt-cf-credstore-fr-authz-action-split`: the restriction is expressed by granting metadata actions without the value-read action, and it applies to any principal kind rather than being derived from whether the subject is human.
3. **Audit trail** (P2, from PRD): emit structured audit events (actor, tenant, outcome — never values) to a platform audit sink.
4. ~~**List/metadata endpoint**~~ **Answered** by [ADR-0005](./ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md): upward-rooted collection, tenant dimension as a gate rather than a SQL clamp, per-type authorization with post-query drops, and a cursor pinned to reference boundaries. **Still open**: the reference-boundary reduction is the platform's first row-reducing cursor pagination, so its page-boundary behaviour needs its own test suite before the endpoint ships.

(The former "dynamic type descriptors" question is resolved: types are registry-driven per §5, with the fail-closed semantics of §5.4.)

## 10. Additional context

### Plugin Registration

Following the ToolKit plugin pattern:

1. The SDK's `CredStorePluginSpecV1` schema reaches types-registry automatically via the `toolkit-gts` link-time inventory (no per-init registration).
2. Each plugin registers its GTS instance and its scoped `CredStorePluginClientV1` in ClientHub.
3. The gear lazily resolves the active plugin: instance query by type-id prefix → filter by configured `vendor` → highest-priority active instance.

**Exactly one storage plugin is active per deployment** (vendor match). The gear handles all cross-cutting concerns; plugins are per-tenant value stores only.

**GTS Types:**
- Plugin spec: `gts.cf.toolkit.plugins.plugin.v1~cf.core.credstore.plugin.v1~`
- Secret resource type: `gts.cf.core.credstore.secret.v1~` (= `SECRET_RESOURCE_TYPE`, the PDP resource type; registered with an empty property set — authorization needs only the type id)
- Secret types: derived from the secret base type — built-ins `gts.cf.core.credstore.secret.v1~cf.core.credstore.<name>.v1~` (one seed per catalog entry, traits as `x-gts-traits`), plus any custom registered descendant; §5
- Category type (planned, ADR-0004): `gts.cf.core.credstore.category.v1~` — the **source of truth for the category vocabulary**. Each category is an *instance* of this type, not a type of its own, so the vocabulary is enumerated the same way the gear already enumerates backend plugins: `list_instances` with the type-id prefix as the pattern (`infra/plugin_select.rs` does exactly this for `CredStorePluginSpecV1`). §5.5

### Configuration

```yaml
gears:
  credstore:
    database:
      server: "postgres_main"        # platform DB provider reference
    config:
      vendor: "virtuozzo"            # selects the value-store plugin by GTS vendor (default: "virtuozzo")
      hierarchy:
        ancestor_cache_ttl_secs: 300 # ancestor-chain cache TTL (> 0)
      reaper:
        tick_secs: 60                      # reaper cadence (> 0)
        provisioning_timeout_secs: 300     # stuck-provisioning sweep threshold (> 0)
        deprovisioning_timeout_secs: 300   # stuck-deprovisioning sweep threshold (> 0)
```

Config is validated at init (`deny_unknown_fields`; non-empty vendor; all periods > 0); an invalid config fails gear startup.

### Error Mapping

Domain → canonical (wire) mapping (`sdk_error_mapping`, pinned by tests):

| DomainError | Canonical category | HTTP |
|-------------|--------------------|------|
| `InvalidSecretRef`, `InvalidPrecondition`, `UnsupportedTransition` | `InvalidArgument` | 400 |
| `NotFound` | `NotFound` | 404 |
| `Conflict` | `AlreadyExists` | 409 |
| `VersionConflict` | `Aborted` (`OPTIMISTIC_LOCK_FAILURE`) | 409 |
| `AccessDenied` | `AccessDenied` | 403 |
| `ServiceUnavailable` (incl. "no storage plugin registered") | `ServiceUnavailable` (+ `Retry-After` when hinted) | 503 |
| `Internal` | `Internal` (diagnostic stripped from the wire) | 500 |

Plugin-layer `CredStoreError`s are normalized by `map_plugin_err`; a plugin returning `UnsupportedTransition` or `InvalidSecretRef` is a contract violation surfaced as `Internal`.

**Planned: listing and bulk-read reason codes (ADR-0004, ADR-0005).** `GET /credentials` and `POST /credentials:read-secrets` (§4.3.2) reuse the platform's standard OData/cursor-pagination reason codes (`guidelines/DNA/REST/PAGINATION.md`) rather than defining their own:

| Reason code | Canonical category | HTTP | Scenario |
|-------------|--------------------|------|----------|
| `INVALID_FILTER` | `InvalidArgument` | 400 | Malformed `$filter` expression |
| `INVALID_ORDERBY_FIELD` | `InvalidArgument` | 400 | `$orderby` names a field outside the indexed allowlist (§4.7) |
| `INVALID_CURSOR` | `InvalidArgument` | 400 | Opaque cursor is malformed or fails to decode |
| `INVALID_LIMIT` | `InvalidArgument` | 400 | `limit` outside the accepted range |
| `ORDER_MISMATCH` | `InvalidArgument` | 400 | `$orderby` on a later page does not match the order the cursor was minted with |
| `FILTER_MISMATCH` | `InvalidArgument` | 400 | `$filter` on a later page does not match the filter the cursor was minted with |
| `ORDER_WITH_CURSOR` | `InvalidArgument` | 400 | `$orderby` supplied together with a cursor (order is fixed at the first page) |
| `FILTER_TOO_LONG` | `InvalidArgument` | 400 | `$filter` expression exceeds the platform length limit |
| `FILTER_TOO_COMPLEX` | `InvalidArgument` | 400 | `$filter` expression exceeds the platform complexity limit |
| `TOO_MANY_MATCHES` | `InvalidArgument` | 400 | Bulk-read selector matches more than the configured cap (proposed: 25); the client narrows the selector rather than the gear truncating the result (ADR-0004) |
| `TYPE_MISMATCH_WITH_INHERITED` | `Aborted` | 409 | A record is created for a reference that currently resolves to an ancestor's `shared` credential, but names a different secret type (`cpt-cf-credstore-fr-override-type-consistency`) |
| `CATEGORY_MISMATCH_WITH_INHERITED` | `Aborted` | 409 | A record write sets a category differing from the ancestor `shared` credential the reference currently resolves to (`cpt-cf-credstore-fr-override-category-consistency`, §5.4) |

### Observability

Typed OpenTelemetry metrics via `CredStoreMetricsPort`:

- `walkup_depth` — ancestor distance of the winning row
- `read_outcome` — own / inherited / miss
- `dependency` — latency + outcome per dependency (PDP evaluate, tenant-resolver chain, types-registry type resolution, plugin get/put/delete)
- `cross_tenant_denied` — own-tenant gate rejections
- `provisioning_rollback`, `provisioning_reaped`, `deprovisioning_reaped` — saga health
- inventory gauge — row counts per status, refreshed by the reaper

## 11. Traceability

- **PRD**: [PRD.md](./PRD.md)
- **ADRs**: [ADR/](./ADR/) — [ADR-0001 stateful gear](./ADR/0001-cpt-cf-credstore-adr-stateful-gear.md), [ADR-0002 deprovisioning saga](./ADR/0002-cpt-cf-credstore-adr-deprovisioning-saga.md), [ADR-0003 value-fingerprint fence](./ADR/0003-cpt-cf-credstore-adr-value-fingerprint-fence.md), [ADR-0004 credential record / secret value split](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md) (proposed), [ADR-0005 upward-rooted collection read](./ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md) (proposed)
- **Requirements added by ADR-0004 / ADR-0005** (PRD §5.8): `cpt-cf-credstore-fr-credential-record`, `-fr-list-credentials`, `-fr-get-credential`, `-fr-write-credential-record`, `-fr-read-secret`, `-fr-write-secret`, `-fr-bulk-read-secrets`, `-fr-authz-action-split`, `-fr-secret-category`, `-fr-inheritance-status`, `-fr-override-type-consistency`, `-fr-override-category-consistency`; `-fr-suppression` (P2)
- **Features**: features/ (planned)
