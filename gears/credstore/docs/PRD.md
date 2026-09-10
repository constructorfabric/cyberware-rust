Updated:  2026-07-07 by Virtuozzo International GmbH

# PRD — CredStore


<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Actors](#2-actors)
  - [2.1 Human Actors](#21-human-actors)
  - [2.2 System Actors](#22-system-actors)
- [3. Operational Concept & Environment](#3-operational-concept--environment)
  - [3.1 Gear-Specific Environment Constraints](#31-gear-specific-environment-constraints)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 P1 — Core Operations](#51-p1--core-operations)
  - [5.2 P1 — Hierarchical Sharing](#52-p1--hierarchical-sharing)
  - [5.3 P1 — Authorization](#53-p1--authorization)
  - [5.4 P1 — Reliability & Concurrency](#54-p1--reliability--concurrency)
  - [5.5 P1 — Secret Types](#55-p1--secret-types)
  - [5.6 P1 — Deprovisioning Lifecycle](#56-p1--deprovisioning-lifecycle)
  - [5.7 P2 — Planned](#57-p2--planned)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Gear-Specific NFRs](#61-gear-specific-nfrs)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

<!--
=============================================================================
PRODUCT REQUIREMENTS DOCUMENT (PRD)
=============================================================================
PURPOSE: Define WHAT the system must do and WHY — business requirements,
functional capabilities, and quality attributes.

SCOPE:
  ✓ Business goals and success criteria
  ✓ Actors (users, systems) that interact with this gear
  ✓ Functional requirements (WHAT, not HOW)
  ✓ Non-functional requirements (quality attributes, SLOs)
  ✓ Scope boundaries (in/out of scope)
  ✓ Assumptions, dependencies, risks

NOT IN THIS DOCUMENT (see other templates):
  ✗ Technical architecture, design decisions → DESIGN.md
  ✗ Why a specific technical approach was chosen → ADR/
  ✗ Detailed implementation flows, algorithms → features/

REQUIREMENT LANGUAGE:
  - Use "MUST" or "SHALL" for mandatory requirements (implicit default)
  - Do not use "SHOULD" or "MAY" — use priority p2/p3 instead
  - Requirements marked **Planned** are specified but not yet implemented;
    everything else is implemented.
  - Be specific and clear; no fluff, bloat, duplication, or emoji
  - Keep transport/mechanism detail (endpoints, status codes, headers) out of
    this doc — it lives in DESIGN.md; the PRD states capabilities and outcomes.
=============================================================================
-->

## 1. Overview

### 1.1 Purpose

CredStore provides per-tenant secret storage and retrieval for the platform. It owns all secret metadata (identity, sharing, ownership, lifecycle status, version) and enforces policy; pluggable backends store only the secret values. This abstracts backend differences behind a unified API, enabling platform gears to store and access credentials without coupling to a specific storage technology.

### 1.2 Background / Problem Statement

Platform gears — most notably the Outbound API Gateway (OAGW) — need access to secrets (API keys, tokens, credentials) for making upstream API calls on behalf of tenants. These secrets must be stored securely, scoped per tenant, and accessible only to authorized consumers.

Standard credential stores provide per-tenant isolation but do not support hierarchical multi-tenant sharing. In the platform's business model, parent tenants (partners) share API credentials with child tenants (customers). For example, a partner with an OpenAI API key and quota allows their customers to make requests through OAGW using the partner's key — without the customer ever seeing the actual secret value. This requires a hierarchical resolution model: when a customer requests a secret, the system walks up the tenant tree to find a shared secret from an ancestor.

Keeping secret metadata in the gear's own database (rather than in the backend) makes hierarchical resolution and authorization a single transactional query, removes any backend schema prerequisite, and allows any simple key-value store to serve as a backend plugin.

### 1.3 Goals (Business Outcomes)

- Enable OAGW to retrieve tenant credentials for upstream API calls without exposing secret values to end users
- Support hierarchical credential sharing so partners can share API access with customers
- Decouple platform gears from specific credential storage backends
- Enforce least-privilege access through the platform policy plane (PDP), with tenant isolation guaranteed at the data layer
- Make secret writes and deletes crash-safe: no partial failure may leak a readable half-written secret or permanently block a secret name

### 1.4 Glossary

| Term | Definition |
|------|------------|
| Secret | A key-value pair where the value is sensitive (API key, token, password) |
| Secret reference | A human-readable key identifying a secret within a tenant's namespace (e.g., `partner-openai-key`). **Format**: `[a-zA-Z0-9_-]+`, 1–255 characters. |
| Sharing mode | Controls secret access scope: `private` (owner only), `tenant` (all users in tenant, default), or `shared` (tenant + descendants) |
| Owner | The specific actor (identified by `subject_id` from SecurityContext) that created the secret |
| Hierarchical resolution | Lookup that resolves a reference against the requesting tenant and its ancestors, returning the closest accessible secret |
| Secret shadowing | When a child tenant creates a secret with the same reference as a parent's shared secret, the child's own secret takes precedence |
| Secret status | Lifecycle state of a secret: `provisioning` (write in flight), `active` (readable), `deprovisioning` (delete in flight) |
| Secret type | A GTS-registered classification of a secret (e.g., `api-key`, `personal-token`) carrying enforceable traits such as `allow_sharing`; `generic` by default, immutable per secret |
| Version | Monotonic per-secret counter used for optimistic concurrency (lost-update detection) |
| SecurityContext | Request security context carrying the authenticated tenant ID, subject ID, and claims |
| PDP | The platform policy decision point (`authz-resolver`) that evaluates access scopes |

## 2. Actors

### 2.1 Human Actors

#### Tenant Admin

**ID**: `cpt-cf-credstore-actor-tenant-admin`

<!-- cpt-cf-id-content -->
**Role**: Authenticated user managing secrets for their tenant. Creates, updates, and deletes secrets. Configures sharing mode to control descendant access. **Needs**: CRUD operations on secrets within their own tenant namespace. Ability to share secrets with descendants or keep them private.
<!-- cpt-cf-id-content -->

### 2.2 System Actors

#### Outbound API Gateway (OAGW)

**ID**: `cpt-cf-credstore-actor-oagw`

<!-- cpt-cf-id-content -->
**Role**: Service that proxies outbound API calls to external services. Retrieves secrets on behalf of tenants by constructing a SecurityContext for the target tenant. Primary consumer of hierarchical secret resolution.
<!-- cpt-cf-id-content -->

#### Platform Gear

**ID**: `cpt-cf-credstore-actor-platform-gear`

<!-- cpt-cf-id-content -->
**Role**: Any internal gear consuming secrets via the ClientHub in-process API. Reads or writes secrets using the calling tenant's SecurityContext.
<!-- cpt-cf-id-content -->

#### Value-Store Backend (Plugin)

**ID**: `cpt-cf-credstore-actor-backend`

<!-- cpt-cf-id-content -->
**Role**: Pluggable per-tenant key-value store that persists secret **values only** (no metadata, no policy). Current implementation: `static-credstore-plugin` (in-memory, for development/testing). Production vault-backed plugins are planned. Accessed exclusively through the gear.
<!-- cpt-cf-id-content -->

#### Platform Policy & Directory Services

**ID**: `cpt-cf-credstore-actor-platform-services`

<!-- cpt-cf-id-content -->
**Role**: `authz-resolver` (PDP) evaluates per-operation access scopes; `tenant-resolver` supplies tenant ancestor chains; `types-registry` provides GTS-based plugin discovery and receives the secret-type registrations.
<!-- cpt-cf-id-content -->

## 3. Operational Concept & Environment

> **Note**: Project-wide runtime, OS, architecture, lifecycle policy, and integration patterns defined in root PRD. Document only gear-specific deviations here.

### 3.1 Gear-Specific Environment Constraints

- The gear is a **stateful** gear: it requires a database (PostgreSQL or SQLite; MySQL is rejected at migration time)
- Exactly one value-store plugin is active per deployment (selected by GTS `vendor` configuration)
- The gear depends on `authz-resolver`, `tenant-resolver`, and `types-registry`, and initializes at system priority (its consumers, e.g. OAGW, resolve the client during their own init)
- A background reaper task runs for the lifetime of the gear (lifecycle entry), sweeping stuck lifecycle rows and refreshing inventory metrics

## 4. Scope

### 4.1 In Scope

- Store, retrieve, and delete per-tenant secrets (ClientHub + REST)
- Sharing modes: private (owner-only), tenant (tenant-wide, default), shared (hierarchical)
- Owner-based access control for private secrets (`subject_id` from SecurityContext)
- Hierarchical secret resolution across tenant ancestry
- Secret shadowing (child overrides parent)
- Service-to-service retrieval on behalf of arbitrary tenants (OAGW pattern)
- PDP-based authorization with tenant-scope enforcement at the data layer
- Crash-safe write and delete lifecycles (provisioning/deprovisioning sagas + reaper)
- Optimistic concurrency: per-secret version with mandatory update/delete preconditions (creation is the only preconditionless write)
- Gear + plugin architecture with runtime backend selection; in-memory static plugin for development/testing
- GTS-based secret types with enforceable traits (`allow_sharing`, value schemas, size/format limits, expiry)
- Operational metrics (resolution depth/outcome, dependency health, saga health, inventory)

### 4.2 Out of Scope

- Secret value history or rollback (the version counter serves optimistic locking only)
- Automatic secret rotation (type-level rotation traits are advisory only)
- Cross-tenant secret transfer (secrets cannot change ownership)
- Unauthenticated or untrusted client access (all access requires platform authentication via SecurityContext)
- Secret listing or search operations (only retrieval by known reference)
- Granular per-secret ACLs beyond the sharing modes (e.g., "share with tenants A, B, C only" or sharing outside the hierarchy)
- Hierarchical or policy logic in backend plugins (plugins are pure value stores)
- MySQL as a metadata database

## 5. Functional Requirements

### 5.1 P1 — Core Operations

#### Store Secret

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-put-secret`

<!-- cpt-cf-id-content -->
The system **MUST** allow a tenant to store a secret with a reference (key), a value, and a sharing mode. Two write operations exist: a create-only operation that fails with a conflict when a secret of the same sharing class already exists, and a precondition-guarded update of an existing secret (see the optimistic-concurrency requirement) that fails with a conflict when the target does not exist — an update never creates. For `tenant` and `shared` modes a write updates the single non-private secret for `(tenant, reference)`; for `private` mode each owner has an independent secret under `(tenant, reference, owner)`. A private secret and a tenant/shared secret with the same reference coexist; a write of one sharing class **MUST NOT** affect the other. Changing a secret between `private` and `tenant`/`shared` is rejected as an unsupported transition.

**Rationale**: Core capability — tenants manage their own credentials; the coexistence rule makes private and team secrets independent under common names. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

#### Retrieve Secret

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-get-secret`

<!-- cpt-cf-id-content -->
The system **MUST** allow a caller to retrieve the decrypted value of an accessible secret by reference, together with access metadata: owning tenant, sharing mode, whether the secret was inherited from an ancestor, and its version. Only fully provisioned (`active`) secrets are visible. Not-found and inaccessible are indistinguishable in the response (a single not-found surface).

**Rationale**: Consumers need the value plus enough metadata to understand inheritance and support concurrency control. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`, `cpt-cf-credstore-actor-oagw`
<!-- cpt-cf-id-content -->

#### Delete Secret

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-delete-secret`

<!-- cpt-cf-id-content -->
The system **MUST** allow a tenant to delete their own secret by reference (own-tenant only; the private class targets the caller's own private secret). Descendants using a shared secret lose access immediately upon deletion. Deleting a missing backend value is not an error (idempotent delete).

**Rationale**: Tenants must be able to revoke credentials reliably. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

#### Tenant Scoping

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-tenant-scoping`

<!-- cpt-cf-id-content -->
The system **MUST** derive the operating tenant from the request SecurityContext (`subject_tenant_id`) and the owner from `subject_id` for all operations. Tenants **MUST NOT** create, update, or delete secrets belonging to other tenants. If the caller's authorized scope does not include their own tenant, the operation is denied before any side effect and the denial is recorded (cross-tenant metric).

**Rationale**: Prevents cross-tenant data manipulation; fail-closed before side effects. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

#### Secret Reference Validation

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-secretref-validation`

<!-- cpt-cf-id-content -->
The system **MUST** validate the secret reference format: `[a-zA-Z0-9_-]+`, 1–255 characters. Invalid references are rejected with a validation error at the API boundary, and the same constraint is enforced by a database `CHECK`.

**Rationale**: A restricted, portable key alphabet keeps references safe for every backend key namespace and URL path segment. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

### 5.2 P1 — Hierarchical Sharing

#### Sharing Modes

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-sharing-modes`

<!-- cpt-cf-id-content -->
Each secret **MUST** have a sharing mode: `private`, `tenant` (default), or `shared`.
- `private`: accessible only to the owner (the actor identified by `subject_id` that created the secret)
- `tenant`: accessible to all users and services within the owning tenant
- `shared`: accessible to all users in the owning tenant and all descendant tenants in the hierarchy

**Rationale**: Partners need flexible credential sharing. Personal API keys should be owner-only (`private`), team credentials tenant-wide (`tenant`), platform-level credentials for customer access hierarchical (`shared`). **Actors**: `cpt-cf-credstore-actor-tenant-admin`
<!-- cpt-cf-id-content -->

#### Hierarchical Secret Resolution

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-hierarchical-resolve`

<!-- cpt-cf-id-content -->
The system **MUST** resolve a secret reference against the requesting tenant and its ancestor chain (parent, grandparent, … root), returning the closest accessible secret; at the same tenant level the caller's private secret takes precedence over a tenant/shared one. If no accessible secret exists, the system returns not-found.

**Hierarchical direction**: resolution is **upward-only** (child → parent → root). A tenant can access ancestor secrets marked `shared`, but parents **cannot** access child secrets.

**Isolation barriers**: a `shared` secret **MUST** be inherited by all descendant tenants, including across `self_managed` (isolation-barrier) boundaries — publishing as `shared` is the owner's explicit sharing decision; read authorization remains the PDP's.

**Rationale**: Enables the core business use case — OAGW retrieves a partner's shared API key when acting for a customer — including for customers that manage their own sub-hierarchy. **Actors**: `cpt-cf-credstore-actor-oagw`
<!-- cpt-cf-id-content -->

#### Secret Shadowing

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-secret-shadowing`

<!-- cpt-cf-id-content -->
When a tenant owns a secret with the same reference as an ancestor's shared secret, and that secret is **accessible** to the requester, the tenant's own secret **MUST** take precedence during hierarchical resolution. If the tenant's same-reference secret is **inaccessible** to the requester (e.g., another owner's `private` secret), resolution **MUST** continue to ancestors.

**Rationale**: Customers can override partner defaults with their own credentials while keeping hierarchical fallback when the local secret is not theirs. **Actors**: `cpt-cf-credstore-actor-oagw`
<!-- cpt-cf-id-content -->

#### Service-to-Service Retrieval

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-service-retrieve`

<!-- cpt-cf-id-content -->
The system **MUST** support retrieval on behalf of an arbitrary tenant by an authorized service account: the service constructs a SecurityContext for the target tenant and calls the standard `get` operation; the PDP decides whether that subject may read in that tenant's scope. The response includes the decrypted value. There is no separate service-to-service operation.

**Rationale**: OAGW operates as a service account and needs hierarchical retrieval for arbitrary tenants through the same audited, policy-checked path. **Actors**: `cpt-cf-credstore-actor-oagw`
<!-- cpt-cf-id-content -->

### 5.3 P1 — Authorization

#### PDP-Based Authorization

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-authz-pdp`

<!-- cpt-cf-id-content -->
Every operation **MUST** be authorized through the platform PDP: the gear evaluates an access scope for the operation's action (`read` for get, `write` for put/create, `delete` for delete) against the secret's resolved concrete GTS type (including `generic`), and **MUST** enforce the returned scope on every metadata query at the data layer, enabling per-type policies (e.g., a role that reads `api-key` but not `certificate` secrets). Enforcement is fail-closed: a PDP denial denies the operation; a PDP evaluation failure surfaces as unavailable; out-of-scope or type-denied secrets are indistinguishable from non-existent ones on read.

**Rationale**: Real tenant isolation enforced in SQL, consistent with the platform policy plane; least privilege per action. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-oagw`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

#### Gear-Level Enforcement

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-authz-gear`

<!-- cpt-cf-id-content -->
Authorization, sharing-mode enforcement, and hierarchy logic **MUST** live exclusively in the gear. Plugins are pure value stores and **MUST NOT** implement authorization or policy decisions.

**Rationale**: Prevents inconsistent authorization behavior across backends; keeps backends trivially simple. **Actors**: `cpt-cf-credstore-actor-platform-gear`, `cpt-cf-credstore-actor-backend`
<!-- cpt-cf-id-content -->

### 5.4 P1 — Reliability & Concurrency

#### Crash-Safe Write Lifecycle

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-write-lifecycle`

<!-- cpt-cf-id-content -->
A secret write that spans metadata and backend **MUST** be crash-safe: a new secret becomes readable only after its value is durably stored in the backend (`provisioning` → `active`), and a failed backend write on the create path rolls the metadata back; on an overwrite of an existing secret a failed or half-completed backend write instead leaves the secret unreadable (fail-closed) until a retried write lands both parts. A crash mid-write leaves a non-readable in-flight record that is swept by a periodic reaper within a configurable timeout. No failure mode may serve a readable secret without a matching value or permanently block the reference.

**Rationale**: Readers must never observe half-written secrets; writers must never permanently wedge a secret name. **Actors**: `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

#### Optimistic Concurrency

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-optimistic-concurrency`

<!-- cpt-cf-id-content -->
Each secret **MUST** carry a monotonic version, exposed on retrieval. Update and delete **MUST** require a caller-supplied precondition ("must exist", or "the specified generation must still be current"), enforced atomically with the metadata commit — every write states its concurrency stance, there are no unconditional overwrites; creation is the only preconditionless write. A failed precondition surfaces as a conflict (lost-update detection); a malformed precondition is a validation error; a missing precondition is a validation error with its own distinct reason.

**Rationale**: Lost-update detection for concurrent secret management. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

### 5.5 P1 — Secret Types

#### GTS-Based Secret Types

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-secret-types`

<!-- cpt-cf-id-content -->
Each secret **MUST** have a *secret type* chosen at creation (default: `generic`) and immutable thereafter. Secret types are GTS types derived from the credstore secret base type and registered in the types-registry. Each type declares machine-readable **traits** that the gear enforces uniformly; at minimum:

- `allow_sharing`: the set of sharing modes permitted for the type. A write requesting a disallowed mode **MUST** be rejected (e.g., `personal-token` secrets are `private`-only and can never be shared).
- `value_schema` (optional): structural validation of the value on write.
- `expirable` (+ optional expiry): expired secrets resolve as not-found.
- `max_size_bytes`, `utf8_only`: value-format constraints.

The initial type catalog covers `generic`, `api-key`, `personal-token`, `oauth2-client`, `basic-auth`, `bearer-token`, `certificate`, `ssh-key`, `webhook-hmac`, and `connection-string` (see DESIGN §5.3). Untyped existing secrets behave as `generic` with unchanged semantics. Expired secrets of expirable types resolve as not-found and are cleaned up by the reaper through the deprovisioning lifecycle.

**Rationale**: Different kinds of secrets have different safe-handling rules; encoding them as GTS type traits gives one enforcement point in the gear, platform-native discoverability/versioning, and per-type policy targeting (PDP) without per-secret ACLs. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

### 5.6 P1 — Deprovisioning Lifecycle

#### Crash-Safe Delete (Deprovisioning Saga)

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-deprovisioning`

<!-- cpt-cf-id-content -->
Secret deletion **MUST** be a crash-safe lifecycle symmetric to provisioning: the secret first enters a `deprovisioning` status — at which instant it atomically stops resolving — then the backend value is deleted, then the metadata record is removed. A failure or crash at any step leaves a non-readable `deprovisioning` record that (a) a client retry of the delete resumes idempotently, and (b) the reaper completes within a configurable timeout. While a reference is deprovisioning, re-creating it **MUST** fail with a retryable conflict (the name is released only after backend cleanup completes).

**Rationale**: A plain backend-first delete leaves metadata/backend divergence on partial failure with no self-healing owner; the status-driven saga plus reaper makes revocation reliable and observable, and closes the orphaned-backend-value debt of the write saga (the reaper reconciles backend values for all reaped records). **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

### 5.7 P2 — Planned

#### Production Value-Store Backend

- [ ] `p2` - **ID**: `cpt-cf-credstore-fr-production-backend`

<!-- cpt-cf-id-content -->
The system **MUST** provide at least one production-grade value-store plugin (external secret vault, KMS-backed store, or OS-protected storage for desktop/VM environments) implementing the same plugin contract as the development in-memory plugin. Backend selection remains a deployment-time configuration with no consumer-visible change.

**Rationale**: The in-memory static plugin is suitable for development and testing only (values do not survive process restart). **Actors**: `cpt-cf-credstore-actor-backend`
<!-- cpt-cf-id-content -->

## 6. Non-Functional Requirements

### 6.1 Gear-Specific NFRs

#### Secret Value Confidentiality

- [ ] `p1` - **ID**: `cpt-cf-credstore-nfr-confidentiality`

<!-- cpt-cf-id-content -->
Secret values **MUST NOT** appear in logs, error messages, or debug output at any level (gear, plugin, transport), **MUST NOT** be cacheable by intermediaries, and **MUST NOT** be silently corrupted (non-UTF-8 values are rejected on the string transport rather than lossily decoded). Secret memory is zeroized on drop.

**Threshold**: Zero plaintext secret values in any log output **Rationale**: Secrets are the most sensitive data in the platform. **Architecture Allocation**: See DESIGN.md §3.2 for the implementation approach
<!-- cpt-cf-id-content -->

#### Tenant Isolation

- [ ] `p1` - **ID**: `cpt-cf-credstore-nfr-tenant-isolation`

<!-- cpt-cf-id-content -->
No operation may read or modify secret metadata outside the caller's PDP-authorized tenant scope; enforcement happens at the data layer on every query. Inaccessible secrets are indistinguishable from non-existent ones (anti-enumeration).

**Threshold**: Zero cross-tenant reads/writes outside the authorized scope **Rationale**: Multi-tenant platform guarantee. **Architecture Allocation**: PDP scope + data-layer clamps; see DESIGN.md §3.1
<!-- cpt-cf-id-content -->

#### Observability

- [ ] `p1` - **ID**: `cpt-cf-credstore-nfr-observability`

<!-- cpt-cf-id-content -->
The gear **MUST** emit operational metrics sufficient to detect resolution anomalies and lifecycle divergence: walk-up depth, read outcome (own/inherited/miss), per-dependency latency and outcome (PDP, tenant-resolver, plugin), cross-tenant denials, saga rollback/reap counters, and per-status inventory gauges. Metric labels **MUST NOT** contain secret references or values.

**Rationale**: Sagas and hierarchical resolution fail in partial, quiet ways; operators need signals, not log archaeology. **Architecture Allocation**: See DESIGN.md §10 Observability
<!-- cpt-cf-id-content -->

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### CredStoreClientV1

- [ ] `p1` - **ID**: `cpt-cf-credstore-interface-client`

<!-- cpt-cf-id-content -->
**Type**: Rust trait (async) **Stability**: stable **Description**: Public API for platform gears. Registered in ClientHub without scope. Operations: `get` (hierarchical read returning value + metadata: owning tenant, sharing, inherited flag, version, secret type, expiry), `put`/`create` (precondition-guarded update / create-only) plus typed-options variants accepting write options (secret type, expiry), `delete` (precondition-guarded). Hierarchical resolution is internal to the gear. **Breaking Change Policy**: Major version bump required
<!-- cpt-cf-id-content -->

#### CredStorePluginClientV1

- [ ] `p1` - **ID**: `cpt-cf-credstore-interface-plugin-client`

<!-- cpt-cf-id-content -->
**Type**: Rust trait (async) **Stability**: unstable **Description**: Plugin SPI for backend value stores. Registered in ClientHub with GTS instance scope. Operations: `get`/`put`/`delete` keyed by `(tenant_id, key, owner_id: Option)` where `Some(owner)` addresses the owner's private key class and `None` the tenant key class. Returns the value only — no metadata, no policy. **Breaking Change Policy**: Minor version bump (unstable API)
<!-- cpt-cf-id-content -->

### 7.2 External Integration Contracts

#### REST API

- [ ] `p1` - **ID**: `cpt-cf-credstore-contract-rest-api`

<!-- cpt-cf-id-content -->
**Direction**: provided **Protocol/Format**: HTTP/REST, JSON, canonical `Problem` error envelope, under a versioned path served beneath the platform API prefix. Exposes create-only and precondition-guarded update writes, retrieval, and delete over the secret reference, with optional secret-type and expiry inputs on writes, a mandatory `If-Match` precondition on update/delete, and value-confidentiality response controls. See DESIGN.md for the concrete endpoints, methods, status codes, and headers. **Compatibility**: Backward-compatible within major version
<!-- cpt-cf-id-content -->

#### GTS Registration

- [ ] `p1` - **ID**: `cpt-cf-credstore-contract-gts`

<!-- cpt-cf-id-content -->
**Direction**: provided to types-registry **Protocol/Format**: GTS link-time inventory. Registered types: the plugin spec, the secret resource type used by the PDP (carrying the secret-type traits schema), and the derived secret-type family (traits mirrored as `x-gts-traits`). See DESIGN §5 for the concrete type ids. **Compatibility**: Type ids are stable identifiers; new versions are new ids
<!-- cpt-cf-id-content -->

## 8. Use Cases

#### UC-001: Partner Creates Shared Secret

- [ ] `p1` - **ID**: `cpt-cf-credstore-usecase-create-shared`

<!-- cpt-cf-id-content -->
**Actor**: `cpt-cf-credstore-actor-tenant-admin`

**Preconditions**:
- Tenant is authenticated; PDP authorizes `write` on secrets in the tenant's scope

**Main Flow**:
1. Partner tenant stores `partner-openai-key` with a value and sharing `shared`
2. Gear evaluates the PDP write scope and the own-tenant gate
3. Gear runs the write saga: provisioning row → backend value write → active
4. Secret is immediately resolvable by the partner and all descendant tenants

**Postconditions**:
- Secret is stored and accessible to partner and descendants

**Alternative Flows**:
- **Secret already exists (same class)**: value/sharing updated, version bumped
- **Create-only write**: fails with a conflict if the reference is taken in that sharing class
- **Backend write fails**: provisioning row rolled back; reference not wedged; caller retries
<!-- cpt-cf-id-content -->

#### UC-002: OAGW Retrieves Secret for Customer (Hierarchical Resolution)

- [ ] `p1` - **ID**: `cpt-cf-credstore-usecase-hierarchical-resolve`

<!-- cpt-cf-id-content -->
**Actor**: `cpt-cf-credstore-actor-oagw`

**Preconditions**:
- OAGW holds a service identity authorized to read secrets in the customer's scope
- Partner has a `shared` secret `partner-openai-key`; customer is a descendant of partner

**Main Flow**:
1. OAGW constructs a SecurityContext for `customer-123` and calls `get("partner-openai-key")`
2. Gear evaluates the PDP read scope for that context
3. Gear obtains the customer's full ancestor chain (cached)
4. Gear resolves the reference against the whole chain in one metadata query → partner's `shared` row wins (customer has none)
5. Gear reads the value from the plugin for the winning row only
6. OAGW receives the value plus metadata (owning tenant = partner, inherited = true, version)

**Postconditions**:
- OAGW has the decrypted secret; the customer never sees the value
- Resolution depth and inherited-read outcome are recorded as metrics

**Alternative Flows**:
- **Customer has own accessible secret**: it wins (shadowing); the parent row is not considered
- **No accessible secret in the chain**: not-found
<!-- cpt-cf-id-content -->

#### UC-003: Customer Overrides Parent Secret (Shadowing)

- [ ] `p1` - **ID**: `cpt-cf-credstore-usecase-shadowing`

<!-- cpt-cf-id-content -->
**Actor**: `cpt-cf-credstore-actor-tenant-admin`

**Preconditions**:
- Partner has shared secret `partner-openai-key`; customer is a descendant

**Main Flow**:
1. Customer creates own secret with the same reference (sharing `tenant`)
2. OAGW resolves `partner-openai-key` for the customer
3. The customer's row is closer in the chain → customer's value returned
4. Partner's secret remains available to other descendants

**Postconditions**:
- Customer uses its own key; partner's shared secret is unaffected

**Alternative Flows**:
- **Customer uses `private` mode**: the override applies only to the creating owner; other subjects in the customer tenant still resolve the partner's shared secret
<!-- cpt-cf-id-content -->

#### UC-004: Private Secret Access & Fallback

- [ ] `p1` - **ID**: `cpt-cf-credstore-usecase-private-denied`

<!-- cpt-cf-id-content -->
**Actor**: `cpt-cf-credstore-actor-oagw`

**Scenario A: Parent's private secret (no leak)**

**Preconditions**:
- Partner has `internal-admin-key` with sharing `private` (owned by PartnerAdmin); customer has no secret with this reference

**Main Flow**:
1. OAGW resolves `internal-admin-key` for the customer
2. The resolution query only matches private rows owned by the requesting subject; PartnerAdmin's row is invisible to OAGW
3. No row matches → not-found

**Postconditions**:
- A parent's private secret is never disclosed to descendants or other subjects

**Scenario B: Another user's private secret with fallback to parent's shared**

**Preconditions**:
- Customer has `api-key` (sharing `private`, owner User A); partner has `api-key` (sharing `shared`); User B in the customer tenant requests `api-key`

**Main Flow**:
1. User B calls `get("api-key")`
2. User A's private row is invisible to User B; the customer tenant has no tenant/shared row
3. The partner's `shared` row is the closest accessible match → returned

**Postconditions**:
- User B falls back to the partner's shared secret; User A's private secret stays invisible

**Rationale**: Private secrets are per-owner; inaccessible private rows never block fallback to ancestor shared secrets.
<!-- cpt-cf-id-content -->

#### UC-005: Tenant CRUD Own Secrets (with Concurrency Control)

- [ ] `p1` - **ID**: `cpt-cf-credstore-usecase-crud`

<!-- cpt-cf-id-content -->
**Actor**: `cpt-cf-credstore-actor-tenant-admin`

**Preconditions**:
- Tenant is authenticated with PDP-authorized read/write/delete scope

**Main Flow**:
1. Create a secret by reference (create-only)
2. Read the secret → value + metadata + current version
3. Guarded update with a version precondition (or an explicit must-exist overwrite) → success, or conflict on a stale version
4. Guarded delete with a version precondition (or an explicit must-exist form) → success

**Postconditions**:
- Secret lifecycle managed; descendants of shared secrets lose access on delete

**Alternative Flows**:
- **Get/delete non-existent secret**: not-found
- **Get another owner's private secret**: not-found (anti-enumeration)
- **Stale version precondition**: conflict, no changes applied
- **Missing precondition on update/delete**: validation error (distinct reason), no changes applied
- **Malformed version precondition**: validation error
<!-- cpt-cf-id-content -->

#### UC-006: Owner-Only Private Secret Access Control

- [ ] `p1` - **ID**: `cpt-cf-credstore-usecase-private-owner-only`

<!-- cpt-cf-id-content -->
**Actor**: `cpt-cf-credstore-actor-tenant-admin`

**Preconditions**:
- User A and User B are authenticated users in the same tenant with write access

**Main Flow**:
1. User A stores `my-personal-api-key` with sharing `private` → row keyed `(tenant, ref, ownerA)`
2. User B stores the same reference with sharing `private` → independent row `(tenant, ref, ownerB)`, no conflict
3. Each user's `get` resolves their own private secret

**Postconditions**:
- Independent per-owner private secrets under one reference; no cross-owner visibility

**Alternative Flows**:
- **User C (no private secret) reads the reference**: falls back to the tenant/shared secret or not-found
- **User B attempts to delete User A's private secret**: deletes address only the caller's own class → User A's secret is untouched (User B gets not-found if they have none)
<!-- cpt-cf-id-content -->

#### UC-007: Type-Restricted Sharing

- [ ] `p1` - **ID**: `cpt-cf-credstore-usecase-type-restricted-sharing`

<!-- cpt-cf-id-content -->
**Actor**: `cpt-cf-credstore-actor-tenant-admin`

**Preconditions**:
- The `personal-token` secret type is registered with trait `allow_sharing = [private]`

**Main Flow**:
1. User stores a secret with type `personal-token` and sharing `private` → accepted
2. User (or a later update) attempts sharing `tenant` or `shared` for the same type → rejected (`SHARING_NOT_ALLOWED_FOR_TYPE`)
3. Retrieval returns `type: personal-token` in metadata

**Postconditions**:
- Personal tokens can never be widened beyond their owner, regardless of caller permissions

**Alternative Flows**:
- **Type omitted**: defaults to `generic` (all sharing modes allowed — current behavior)
- **Attempt to change the type of an existing secret**: rejected as unsupported transition
<!-- cpt-cf-id-content -->

#### UC-008: Reliable Revocation via Deprovisioning

- [ ] `p1` - **ID**: `cpt-cf-credstore-usecase-deprovisioning`

<!-- cpt-cf-id-content -->
**Actor**: `cpt-cf-credstore-actor-tenant-admin`

**Preconditions**:
- Tenant owns an `active` secret consumed by descendants

**Main Flow**:
1. Tenant deletes the secret by reference
2. The secret enters `deprovisioning` — it instantly stops resolving for every consumer
3. The backend value is deleted; the metadata record is removed

**Postconditions**:
- Secret fully revoked; the reference becomes reusable

**Alternative Flows**:
- **Backend delete fails**: caller gets a retryable failure; the secret already does not resolve; a delete retry or the reaper completes cleanup
- **Create during deprovisioning**: retryable conflict until the backend value is cleaned up (bounded by the reaper cadence)
- **Crash mid-delete**: the reaper finishes the saga within the configured timeout
<!-- cpt-cf-id-content -->

## 9. Acceptance Criteria

- [ ] Tenant can store, retrieve, and delete secrets via both ClientHub and REST API
- [ ] Create-only writes conflict on a same-class duplicate; updates require a precondition and never create
- [ ] Private secrets are accessible only to the owner; multiple owners can hold private secrets under one reference; a private and a tenant/shared secret coexist under one reference
- [ ] Tenant secrets are accessible to all subjects within the owning tenant and never inherited; shared secrets are inherited by all descendants
- [ ] Shadowing: the closest accessible secret wins; inaccessible private rows do not block fallback
- [ ] OAGW can retrieve secrets on behalf of any tenant it is authorized for, through the standard API
- [ ] Every operation is PDP-authorized and scope-clamped at the data layer; inaccessible reads are not-found; operation-level denial is refused; a PDP outage fails closed
- [ ] Half-written secrets are never readable; failed writes roll back; stuck lifecycle rows are reaped within the configured timeout
- [ ] Retrieval exposes the current version and value-confidentiality controls; update/delete require a precondition, with a conflict on stale versions and a distinct validation error when it is missing
- [ ] Secret values never appear in log output or metric labels; non-UTF-8 values are rejected on the REST transport, not corrupted
- [ ] Secret types: a write violating the type's `allow_sharing`, `value_schema`, size/format, or expiry traits is rejected with a stable reason; the type is immutable, defaults to `generic`, and is returned in metadata; expired secrets resolve as not-found and are reaped
- [ ] Deprovisioning: a deleted secret stops resolving atomically at delete start; partial delete failures self-heal via retry or reaper; the reference conflicts (retryably) until cleanup completes

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| `authz-resolver` | PDP: per-operation access-scope evaluation (fail-closed) | `p1` |
| `tenant-resolver` | Tenant ancestor chains for hierarchical resolution | `p1` |
| `types-registry` | GTS plugin discovery; secret resource type + secret-type registrations | `p1` |
| Database (PostgreSQL / SQLite) | Gear-owned secret metadata (`credstore_secrets`) | `p1` |
| Value-store plugin | Per-tenant secret value persistence (`static-credstore-plugin` for dev/test; production vault plugin planned) | `p1` |
| OAGW | Primary consumer of hierarchical secret retrieval (uses the SDK client) | `p1` |

## 11. Assumptions

- The gear owns all secret metadata; backends store values only and provide per-tenant key-value CRUD without hierarchical or policy logic
- Exactly one value-store plugin is active per deployment (GTS vendor match)
- Tenant hierarchy is managed externally and served by `tenant-resolver`; short-TTL caching of ancestor chains is acceptable
- The PDP is the sole authorization authority; there is no local policy cache (policy freshness over availability)
- Consumers provisioning infrastructure from secrets at startup (e.g., mini-chat → OAGW upstreams) tolerate missing secrets by degrading per-provider rather than failing boot
- OAGW is a ToolKit gear that uses the standard CredStore SDK client (all access flows through Gear → Plugin)

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Secret values leaked through logs/caches | Critical security incident | NFR enforcement (redaction, zeroize, no-store responses), code review |
| Metadata/backend divergence on partial saga failure | Orphaned backend values, temporarily wedged references | Compensating rollback; deprovisioning saga; reaper backend reconciliation with configurable timeouts; saga metrics |
| PDP or tenant-resolver outage | Operations fail closed (unavailable) | Ancestor-chain cache absorbs blips; dependency metrics for fast diagnosis |
| Ancestor-chain cache staleness | Briefly stale hierarchy after re-parenting | Short TTL + LRU; PDP scope still clamps every query |
| In-memory static plugin in non-dev use | Secret values lost on restart | Production vault plugin (`cpt-cf-credstore-fr-production-backend`); deployment policy |
| Type-trait misconfiguration | Overly permissive or broken writes for a type | Compiled-in catalog pinned to registered GTS schemas by unit tests; catalog changes are code-reviewed SDK releases; `generic` keeps legacy behavior |

## 13. Open Questions

- **Batch retrieval**: should `get` support multiple references per call for OAGW efficiency? (Single-query resolution makes this cheap on the metadata side.)
- **P2/Future — Human vs service access**: should human users be restricted to metadata-only for inherited shared secrets while service accounts can read values?
- **P2/Future — Audit trails**: structured audit events (actor, tenant, outcome — never values) to a tamper-evident platform sink.
- **P2/Future — Metadata list endpoint**: a values-free list becomes cheap with gear-owned metadata; must be reconciled with the anti-enumeration stance and per-type authorization.

## 14. Traceability

- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: [ADR/](./ADR/) — [ADR-0001 stateful gear](./ADR/0001-cpt-cf-credstore-adr-stateful-gear.md), [ADR-0002 deprovisioning saga](./ADR/0002-cpt-cf-credstore-adr-deprovisioning-saga.md), [ADR-0003 value-fingerprint fence](./ADR/0003-cpt-cf-credstore-adr-value-fingerprint-fence.md)
- **Features**: features/ (planned)
