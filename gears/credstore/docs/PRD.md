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
  - [5.8 P1 — Planned — Credential Records and Secret Values](#58-p1--planned--credential-records-and-secret-values)
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
| Secret status | Lifecycle state of a secret: `provisioning` (write in flight), `active` (readable), `deprovisioning` (delete in flight). **Superseded by ADR-0006 (planned)**: shrinks to two states, `active` and `declared` (value removed, record retained); `provisioning`/`deprovisioning` are retired — a write only ever produces a fully `active` row or none at all |
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

#### Integration Administrator

**ID**: `cpt-cf-credstore-actor-integrations-admin`

<!-- cpt-cf-id-content -->
**Role**: Configures a tenant's integrations (SMTP, provider keys, webhooks): creates and rotates credentials, retargets and disables them, and reads the catalogue. **Needs**: Read access to the credential catalogue and to a record's metadata; ability to create and replace credential records and to rotate their values under precondition control. Does not need the plaintext of the credentials being managed.
<!-- cpt-cf-id-content -->

#### Catalogue Auditor

**ID**: `cpt-cf-credstore-actor-catalogue-auditor`

<!-- cpt-cf-id-content -->
**Role**: Reviews what a tenant has configured — which credentials exist, their types, whether each is the tenant's own or inherited, when each expires — for compliance, support or migration planning. Changes nothing and reads no value. **Needs**: The credential catalogue and each record's metadata; nothing else.
<!-- cpt-cf-id-content -->

### 2.2 System Actors

#### Outbound API Gateway (OAGW)

**ID**: `cpt-cf-credstore-actor-oagw`

<!-- cpt-cf-id-content -->
**Role**: Service that proxies outbound API calls to external services. Retrieves secrets on behalf of tenants by constructing a SecurityContext for the target tenant. Primary consumer of hierarchical secret resolution.
<!-- cpt-cf-id-content -->

#### Integration Application

**ID**: `cpt-cf-credstore-actor-integration-app`

<!-- cpt-cf-id-content -->
**Role**: A platform service (mail sender, billing connector) that reads the values of the credentials assigned to it, one by one or as its whole set, in the tenant it acts for. Never enumerates the catalogue.
<!-- cpt-cf-id-content -->

#### Self-Rotating Application

**ID**: `cpt-cf-credstore-actor-self-rotating-app`

<!-- cpt-cf-id-content -->
**Role**: A service that both consumes and renews its own credential — refreshing an OAuth token, rotating an API key with its provider — and stores the new value back. **Needs**: To read the value of its credential and to rotate it under the validator that arrives with the value; no catalogue and no other record's metadata.
<!-- cpt-cf-id-content -->

#### Provisioning Injector

**ID**: `cpt-cf-credstore-actor-provisioner`

<!-- cpt-cf-id-content -->
**Role**: A pipeline or synchronization job (CI/CD, a sync from an external vault) that places values into records someone else declared, rotates them on schedule, and — shipped today, superseded by [ADR-0006](./ADR/0006-cpt-cf-credstore-adr-immutable-value-versions.md) (planned) — re-injects a known value to heal a fence-poisoned row; under ADR-0006 it instead writes a fresh version when a backend entry is found corrupted, since there is no in-place row to heal. Sees no value it did not itself supply and no catalogue. **Needs**: To write a value under a guarded or last-writer-wins precondition without holding any read action; optionally to create or edit records too, when it owns their definition.
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
- A background reaper task runs for the lifetime of the gear (lifecycle entry), sweeping stuck lifecycle rows and refreshing inventory metrics — shipped today. **Withdrawn, superseded by ADR-0006 (planned)**: no resident reaper; a periodic maintenance job, run on an operator-chosen schedule (daily by default, weekly acceptable) outside the gear's own lifecycle, performs the equivalent maintenance; no correctness property depends on it

## 4. Scope

### 4.1 In Scope

- Store, retrieve, and delete per-tenant secrets (ClientHub + REST)
- Sharing modes: private (owner-only), tenant (tenant-wide, default), shared (hierarchical)
- Owner-based access control for private secrets (`subject_id` from SecurityContext)
- Hierarchical secret resolution across tenant ancestry
- Secret shadowing (child overrides parent)
- Service-to-service retrieval on behalf of arbitrary tenants (OAGW pattern)
- PDP-based authorization with tenant-scope enforcement at the data layer
- Crash-safe write and delete lifecycles — shipped: provisioning/deprovisioning sagas + reaper; **superseded by ADR-0006 (planned)**: immutable value versions + intent log + a gc-draining periodic maintenance job, no resident reaper
- Optimistic concurrency: per-secret version with mandatory update/delete preconditions (creation is the only preconditionless write)
- Gear + plugin architecture with runtime backend selection; in-memory static plugin for development/testing
- GTS-based secret types with enforceable traits (`allow_sharing`, value schemas, size/format limits, expiry)
- Operational metrics (resolution depth/outcome, dependency health, saga health, per-status inventory gauges — shipped; **superseded by ADR-0006, planned**: the maintenance job's own counters instead of saga health; no equivalent inventory gauge — withdrawn under the platform's no-`COUNT` rule)

### 4.2 Out of Scope

- Secret value history or rollback (the version counter serves optimistic locking only)
- Automatic secret rotation (type-level rotation traits are advisory only)
- Cross-tenant secret transfer (secrets cannot change ownership)
- Unauthenticated or untrusted client access (all access requires platform authentication via SecurityContext)
- Full-text search over secret values or references (retrieval remains by known reference, or by the allowlisted metadata filter of the bulk value read)
- Secret values returned through the credential listing (the listing surface is metadata-only by construction, regardless of the caller's grants)
- Granular per-secret ACLs naming specific tenants (e.g., "share with tenants A, B, C only") or sharing outside the tenant hierarchy
- Hierarchical or policy logic in backend plugins (plugins are pure value stores)
- MySQL as a metadata database

## 5. Functional Requirements

### 5.1 P1 — Core Operations

#### Store Secret

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-put-secret`

<!-- cpt-cf-id-content -->
The system **MUST** allow a tenant to store a secret with a reference (key), a value, and a sharing mode. Two write operations exist: a create-only operation that fails with a conflict when a secret of the same sharing class already exists, and a precondition-guarded update of an existing secret (see the optimistic-concurrency requirement) that fails with a conflict when the target does not exist — an update never creates. For `tenant` and `shared` modes a write updates the single non-private secret for `(tenant, reference)`; for `private` mode each owner has an independent secret under `(tenant, reference, owner)`. A private secret and a tenant/shared secret with the same reference coexist; a write of one sharing class **MUST NOT** affect the other. Changing a secret between `private` and `tenant`/`shared` is rejected as an unsupported transition.

**Superseded in part** by [ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md): the capability — a guarded write of a value under a reference, with private and tenant/shared coexisting and an immutable transition rule — still holds, but the *shape* changes. The record address takes both a full replace and a partial update (`cpt-cf-credstore-fr-write-credential-record`, `cpt-cf-credstore-fr-write-secret`): a `PUT` creates or replaces record and value together in one request, guarded by `If-None-Match: *` or `If-Match`; a `PATCH` applies RFC 7396 merge-patch semantics and never creates, so "an update never creates" now describes the partial update rather than a separate value sub-resource.

**Superseded by [ADR-0006](./ADR/0006-cpt-cf-credstore-adr-immutable-value-versions.md)** (planned, `cpt-cf-credstore-fr-immutable-value-versions`): the write is no longer a saga over a shared, overwritten backend key. It is a protocol — write the value once under a fresh version identifier, then switch the record's pointer to it in one transaction — with no in-flight status and no in-place overwrite.

**Rationale**: Core capability — tenants manage their own credentials; the coexistence rule makes private and team secrets independent under common names. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

#### Retrieve Secret

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-get-secret`

<!-- cpt-cf-id-content -->
The system **MUST** allow a caller to retrieve the decrypted value of an accessible secret by reference, together with access metadata: owning tenant, sharing mode, whether the secret was inherited from an ancestor, and its version. Only fully provisioned (`active`) secrets are visible. Not-found and inaccessible are indistinguishable in the response (a single not-found surface).

**Superseded in part** by [ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md): the capability and the single not-found surface still hold, but the value moves to its own address (`cpt-cf-credstore-fr-read-secret`) and the metadata becomes independently readable without it (`cpt-cf-credstore-fr-get-credential`), so "value together with metadata" is no longer the only way to obtain either half. The metadata half also changes shape: it no longer names the owning tenant, and a three-state inheritance status (`cpt-cf-credstore-fr-inheritance-status`) replaces the inherited flag.

**Rationale**: Consumers need the value plus enough metadata to understand inheritance and support concurrency control. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`, `cpt-cf-credstore-actor-oagw`
<!-- cpt-cf-id-content -->

#### Delete Secret

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-delete-secret`

<!-- cpt-cf-id-content -->
The system **MUST** allow a tenant to delete their own secret by reference (own-tenant only; the private class targets the caller's own private secret). Descendants using a shared secret lose access immediately upon deletion. Deleting a missing backend value is not an error (idempotent delete).

**Superseded in part** by [ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md): revocation semantics are unchanged, but the deletion addresses the credential **record** (removing its value with it), and a tenant that wants to disable an inherited credential without deleting anything of its own uses suppression instead (`cpt-cf-credstore-fr-suppression`).

**Superseded by [ADR-0006](./ADR/0006-cpt-cf-credstore-adr-immutable-value-versions.md)** (planned): deletion is one row transaction plus garbage collection, not a saga; the deprovisioning status and the name-retention window it existed for are withdrawn — the reference is free to reuse the instant the delete transaction commits.

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

**Superseded in part** by `cpt-cf-credstore-fr-authz-action-split`: the mechanism — one PDP evaluation per operation against the resolved concrete type, enforced in SQL, fail-closed — holds unchanged, but the action set does not. `read`/`write`/`delete` describes what ships today; the six actions replace it, with no synonym for the old pair, so every policy granting `read` or `write` is re-issued. The use cases below likewise describe the shipped combined flow, not the split one.

**Rationale**: Real tenant isolation enforced in SQL, consistent with the platform policy plane; least privilege per action. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-oagw`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

#### Gear-Level Enforcement

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-authz-gear`

<!-- cpt-cf-id-content -->
Authorization, sharing-mode enforcement, and hierarchy logic **MUST** live exclusively in the gear. Plugins are pure value stores and **MUST NOT** implement authorization or policy decisions.

**Rationale**: Prevents inconsistent authorization behavior across backends; keeps backends trivially simple. **Actors**: `cpt-cf-credstore-actor-platform-gear`, `cpt-cf-credstore-actor-backend`
<!-- cpt-cf-id-content -->

#### Authorization Action Split

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-authz-action-split`

<!-- cpt-cf-id-content -->
Authorization **MUST** distinguish six actions on the credential resource type: `list`, `read`, `write` and `delete` on the record, and `read_secret` and `write_secret` on the value. The resource type **MUST** be the credential (`gts.cf.core.credstore.credential.v1~` and its derived types), renamed from the shipped `secret.v1~`, so that no shipped permission matches an operation on the new surface; policies granting the shipped actions **MUST** be re-issued against the new type rather than honoured as synonyms. On the record address, the required action set **MUST** be derived from the request body: `write` for metadata fields, `write_secret` for the `value` field, both when both are present. The purpose an application reads for is expressed by the credential **type** alone: a `read_secret` grant names a concrete type or a GTS wildcard of one, and a service that needs "its own" credentials **MUST** declare its own derived type rather than filing records under a separate label — because the type is immutable once set, a metadata edit can never change who may read a value.

**Rationale**: Enumerating entries, reading a record's metadata, and reading a secret value have different blast radius and must be separately grantable; an ambiguous grant would defeat that separation. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-integrations-admin`, `cpt-cf-credstore-actor-integration-app`
<!-- cpt-cf-id-content -->

### 5.4 P1 — Reliability & Concurrency

#### Crash-Safe Write Lifecycle

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-write-lifecycle`

<!-- cpt-cf-id-content -->
A secret write that spans metadata and backend **MUST** be crash-safe: a new secret becomes readable only after its value is durably stored in the backend (`provisioning` → `active`), and a failed backend write on the create path rolls the metadata back; on an overwrite of an existing secret a failed or half-completed backend write instead leaves the secret unreadable (fail-closed) until a retried write lands both parts. A crash mid-write leaves a non-readable in-flight record that is swept by a periodic reaper within a configurable timeout. No failure mode may serve a readable secret without a matching value or permanently block the reference.

**Superseded in part by [ADR-0006](./ADR/0006-cpt-cf-credstore-adr-immutable-value-versions.md)** (planned, `cpt-cf-credstore-fr-immutable-value-versions`): the crash-safety guarantee is unchanged, but there is no `provisioning` status and no half-written row to render unreadable — a write only ever produces a fully `active` row or none at all, since the record's pointer switches only after the backend write is durable. An overwrite's failure mode is likewise not "unreadable until retried": the old value keeps serving until the new one's pointer switch commits.

**Rationale**: Readers must never observe half-written secrets; writers must never permanently wedge a secret name. **Actors**: `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

#### Optimistic Concurrency

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-optimistic-concurrency`

<!-- cpt-cf-id-content -->
Each secret **MUST** carry a monotonic version, exposed on retrieval. Update and delete **MUST** require a caller-supplied precondition ("must exist", or "the specified generation must still be current"), enforced atomically with the metadata commit — every write states its concurrency stance, there are no unconditional overwrites; creation is the only preconditionless write. A failed precondition surfaces as a conflict (lost-update detection); a malformed precondition is a validation error; a missing precondition is a validation error with its own distinct reason.

**Superseded in part** by [ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md): the version is exposed only on the caller's own record; an inherited record carries a weak, opaque validator that changes when the ancestor writes but cannot be used in a write precondition. A record write that changes nothing does not advance the version; a value write always does. All failed preconditions surface as one conflict code.

**Rationale**: Lost-update detection for concurrent secret management. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

### 5.5 P1 — Secret Types

#### GTS-Based Secret Types

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-secret-types`

<!-- cpt-cf-id-content -->
Each secret **MUST** have a *secret type* chosen at creation (default: `generic`) and immutable thereafter. Secret types are GTS types derived from the credstore base type and registered in the types-registry. **Amended by [ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md)**: the base type is renamed from `gts.cf.core.credstore.secret.v1~` to `gts.cf.core.credstore.credential.v1~`, every derived type follows, and the type is also the PDP resource type of the new surface. Each type declares machine-readable **traits** that the gear enforces uniformly; at minimum:

- `allow_sharing`: the set of sharing modes permitted for the type. A write requesting a disallowed mode **MUST** be rejected (e.g., `personal-token` secrets are `private`-only and can never be shared).
- `value_schema` (optional): structural validation of the value on write.
- `expirable` (+ optional expiry): expired secrets resolve as not-found.
- `max_size_bytes`, `utf8_only`: value-format constraints.

The initial type catalog covers `generic`, `api-key`, `personal-token`, `oauth2-client`, `basic-auth`, `bearer-token`, `certificate`, `ssh-key`, `webhook-hmac`, and `connection-string` (see DESIGN §5.3). Untyped existing secrets behave as `generic` with unchanged semantics. Expired secrets of expirable types resolve as not-found and are cleaned up by the reaper through the deprovisioning lifecycle — shipped today; **superseded by ADR-0006 (planned)**: cleaned up by the periodic maintenance job exactly like an ordinary delete, one transaction plus garbage collection.

**Rationale**: Different kinds of secrets have different safe-handling rules; encoding them as GTS type traits gives one enforcement point in the gear, platform-native discoverability/versioning, and per-type policy targeting (PDP) without per-secret ACLs. **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

### 5.6 P1 — Deprovisioning Lifecycle

#### Crash-Safe Delete (Deprovisioning Saga)

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-deprovisioning`

<!-- cpt-cf-id-content -->
Secret deletion **MUST** be a crash-safe lifecycle symmetric to provisioning: the secret first enters a `deprovisioning` status — at which instant it atomically stops resolving — then the backend value is deleted, then the metadata record is removed. A failure or crash at any step leaves a non-readable `deprovisioning` record that (a) a client retry of the delete resumes idempotently, and (b) the reaper completes within a configurable timeout. While a reference is deprovisioning, re-creating it **MUST** fail with a retryable conflict (the name is released only after backend cleanup completes).

**Superseded by [ADR-0006](./ADR/0006-cpt-cf-credstore-adr-immutable-value-versions.md)** (planned): deletion is one row transaction plus garbage collection; the deprovisioning status and name retention are withdrawn. The reference is free to reuse the instant the delete transaction commits — there is nothing left to protect it *for*, since a successor write always mints its own version identifier and can never collide with a lagging backend cleanup of the old one. Crash-safety is unchanged in outcome (no failure leaves a readable secret or a permanently held reference); only the mechanism — one transaction plus a garbage-collection queue instead of a status-driven saga — differs. The queue is drained by a periodic maintenance job on an operator-chosen schedule, not by a resident reaper.

**Rationale**: A plain backend-first delete leaves metadata/backend divergence on partial failure with no self-healing owner; the status-driven saga plus reaper makes revocation reliable and observable, and closes the orphaned-backend-value debt of the write saga (the reaper reconciles backend values for all reaped records). **Actors**: `cpt-cf-credstore-actor-tenant-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

### 5.7 P2 — Planned

#### Production Value-Store Backend

- [ ] `p2` - **ID**: `cpt-cf-credstore-fr-production-backend`

<!-- cpt-cf-id-content -->
The system **MUST** provide at least one production-grade value-store plugin (external secret vault, KMS-backed store, or OS-protected storage for desktop/VM environments) implementing the same plugin contract as the development in-memory plugin. Backend selection remains a deployment-time configuration with no consumer-visible change.

**Rationale**: The in-memory static plugin is suitable for development and testing only (values do not survive process restart). **Actors**: `cpt-cf-credstore-actor-backend`
<!-- cpt-cf-id-content -->

### 5.8 P1 — Planned — Credential Records and Secret Values

> **Planned, not shipped.** Every requirement in this section is specified by [ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md) and [ADR-0005](./ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md), both of which are still `proposed`. What ships today is the combined secret-and-value contract of §5.1 through §5.6. The acceptance criteria in §9 that reference these IDs are planned on the same terms.

#### Credential Record and Secret Value Split

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-credential-record`

<!-- cpt-cf-id-content -->
The system **MUST** address a credential as a **record** whose representation contains metadata only — reference, sharing mode, type, fallback policy, lifecycle status of the caller's own record (none, declared or active), expiry, inheritance status, and, for the caller's own record only, version, last-update time and the creating subject (`owner_id`) — and never the secret value. The representation **MUST NOT** name the owning tenant, nor the creating subject of an inherited record: for an inherited credential that would disclose an ancestor's identifier the caller cannot obtain by any authorized route, and the inheritance status already answers whether the record is the caller's own ([ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md), "What a response says about tenants above"). The secret value **MUST** be a separate addressable sub-resource of that record.

**Rationale**: A metadata surface cannot leak a value it structurally does not contain; separating the two makes a value-blind administrator role possible. **Actors**: `cpt-cf-credstore-actor-integrations-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

#### List Credential Records

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-list-credentials`

<!-- cpt-cf-id-content -->
The system **MUST** allow an authorized caller to list the credential records effectively visible to its tenant, paginated, ordered deterministically, and **never carrying secret values regardless of the caller's grants**. The listing **MUST** follow the platform cursor-pagination contract: an opaque cursor, a `limit`, and OData `$filter`/`$orderby` restricted to an allowlist of indexed fields, with no total count. The listing **MUST** support OData `$select` for sparse projection, restricted to the `Credential` field allowlist; an unrecognized name **MUST** be a 400; without `$select` the item **MUST** be the full `Credential`. Of the filterable fields, only `reference` and `type` **MUST** be applied as SQL clamps — the invariants across a reference's chain — while `sharing`, `expires_at` and `fallback` **MUST** be filtered after the hierarchy is reduced in memory, since they vary along the chain and clamping them in SQL could change which row wins the reduction. Listing **MUST** require its own authorization action, distinct from reading a single record.

**Rationale**: Integration administrators need a catalogue view without turning it into a value-disclosure or enumeration primitive. **Actors**: `cpt-cf-credstore-actor-integrations-admin`
<!-- cpt-cf-id-content -->

#### Get Credential Record

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-get-credential`

<!-- cpt-cf-id-content -->
The system **MUST** allow an authorized caller to read one credential record by reference, applying hierarchical resolution, without disclosing the value. The response **MUST** carry the strong optimistic-concurrency validator for the caller's own record, so that a caller entitled to write but not to read values can still perform a guarded write. A record that does not resolve or is inaccessible **MUST** be indistinguishable in the response.

**Rationale**: A value-blind writer still needs a CAS validator to rotate or replace a record safely. **Actors**: `cpt-cf-credstore-actor-integrations-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

#### Write Credential Record

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-write-credential-record`

<!-- cpt-cf-id-content -->
The system **MUST** accept, at the credential's record address, a full replace — record and value together, in one request — guarded by a create-only or a replace precondition, and **MUST** accept a partial update at the same address following RFC 7396 merge-patch semantics: fields present in the body replace, fields absent are untouched, and a `value` of `null` removes the secret value while the record itself stays in place. A partial update **MUST NOT** create a record — a reference with no record of the caller's own is a not-found. The secret type **MUST** remain immutable under both forms.

**Rationale**: A value-blind administrator edits metadata (sharing, expiry, fallback) through a partial update that never carries a value; the same record address atomically creates a record together with its value in one request, so there is no window in which a record exists without one. **Actors**: `cpt-cf-credstore-actor-integrations-admin`
<!-- cpt-cf-id-content -->

#### Read Secret Value

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-read-secret`

<!-- cpt-cf-id-content -->
The system **MUST** allow an authorized caller to read the value of a credential resolved through the tenant hierarchy, at an address distinct from the record, with caching disabled and one audit record per returned value. The response **MUST** carry, besides the value, only what is needed to use it — the reference, the type and the expiry — plus the record's validator; administrative metadata (sharing mode, inheritance status, lifecycle status) **MUST NOT** accompany a value, so that reading a value and reading a record remain distinct privileges in fact.

**Rationale**: Value disclosure is its own privilege with its own auditable, throttleable path, separate from reading or listing metadata. **Actors**: `cpt-cf-credstore-actor-integration-app`, `cpt-cf-credstore-actor-oagw`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

#### Write Secret Value

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-write-secret`

<!-- cpt-cf-id-content -->
The system **MUST** write a credential's value only through the record address: as the `value` field of a full replace, or as the `value` member of a partial update, under a required precondition, without granting the ability to read that value. Writing a value **MUST** require the value-write action, and the record-write action **MUST** also be required when the same request carries metadata fields. In a partial update, a `value` of `null` **MUST** remove the value, returning the record to its value-less state with its metadata intact, where the record's fallback policy decides whether the reference then inherits or resolves as absent. A partial update **MUST NOT** create: a reference with no record of the caller's own is a not-found rather than a create. The write **MUST NOT** be permitted for a record owned by an ancestor: a tenant that wants its own value creates its own record first. A value write **MUST** always write and always advance the version, even when the submitted value equals the stored one, under the record's one validator.

**Rationale**: This is the requirement that makes the value-blind configurator possible — the persona who provisions and rotates an integration's credentials without ever being able to read one. It is also the half of the old combined write that the record write does not cover, and it was missing from this section while both the endpoint and the `write_secret` action already referenced it. **Actors**: `cpt-cf-credstore-actor-integrations-admin`, `cpt-cf-credstore-actor-platform-gear`
<!-- cpt-cf-id-content -->

#### Immutable Value Versions

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-immutable-value-versions`

<!-- cpt-cf-id-content -->
Every value write **MUST** create a new immutable backend entry under a fresh version identifier and switch the record's pointer to it in one database transaction, only after the bytes are fully written. A record **MUST NOT** ever point at bytes that were not written for it. The previous version **MUST** be removed only after the switch. A failure at any step **MUST** cost at most an orphaned backend entry that the gear collects — never a wrong value, a closed read, or a reserved name. A value-less record **MUST** be one whose pointer is null. Garbage and expired records are removed by a periodic maintenance job; no correctness property depends on it.

**Rationale**: Overwriting a backend value in place makes every partial-write failure a race between two states of the same bytes; giving each write its own version removes that race entirely — a failure can only leave garbage beside the truth, never inside it. This is the immutable-versions model every managed secret store uses (Vault KV v2, Google Cloud Secret Manager versions, AWS Secrets Manager staging labels, Azure Key Vault versions), and it is why the in-process reaper is withdrawn: with no intermediate state to repair, what remains is garbage collection, a periodic job rather than a resident loop (ADR-0006, "The pattern, and why the reaper goes with the saga"). **Actors**: `cpt-cf-credstore-actor-platform-gear`, `cpt-cf-credstore-actor-integrations-admin`, `cpt-cf-credstore-actor-backend`
<!-- cpt-cf-id-content -->

#### Bulk Read Secret Values Through the Collection

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-bulk-read-secrets`

<!-- cpt-cf-id-content -->
The system **MUST** allow an authorized caller to read the values of several credentials in one request through the credential collection, by selecting the value field alongside a projection of metadata fields, restricted to exactly the selectors below and no others: an explicit list of references (`reference in (...)`), or a scope over the credential type (`type eq ...` or `type in (...)`), with membership operators only (`eq`, `in`). No third selector, no ordered or prefix operator, and no `$orderby` is offered in this mode — a prefix scan over references is the enumeration primitive this surface exists to withhold. The selection **MUST NOT** be able to exceed what the caller may read one by one: each item **MUST** be authorized individually. The response **MUST NOT** be paginated and **MUST NOT** be continuable — `limit` and an incoming cursor **MUST** be rejected. The number of returned items **MUST** be capped; exceeding the cap **MUST** fail the request rather than truncate the result. A refused item **MUST** be omitted entirely rather than reported by name, because the filter found it, not the caller. One audit record **MUST** be produced per value returned.

**Rationale**: Applications commonly need their whole credential set in one round-trip; disclosure must stay bounded by the caller's own grant, a hard cap, and the absence of pagination, rather than becoming a walkable catalogue dump. Serving this through the same collection as the metadata listing means one filter grammar and one authorization path cover both. **Actors**: `cpt-cf-credstore-actor-integration-app`
<!-- cpt-cf-id-content -->

#### Inheritance Status

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-inheritance-status`

<!-- cpt-cf-id-content -->
Every record representation **MUST** state its relationship to the ancestor chain: owned by the requesting tenant, inherited from an ancestor, owning a record that overrides an ancestor's shared credential, or resolving to nothing because the nearest record holds no value and a fallback policy of none. The status is metadata, not secret material, and **MUST** be available to callers holding only metadata actions.

**Rationale**: An integration administrator must be able to tell "mine" from "inherited" from the catalogue alone, without ever reading a value. **Actors**: `cpt-cf-credstore-actor-integrations-admin`
<!-- cpt-cf-id-content -->

#### Tenant-Level Suppression

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-suppression`

<!-- cpt-cf-id-content -->
Each credential record **MUST** carry a fallback policy, `inherit` or `none`, governing resolution while the record holds no value; set with the record-write action. A record with `none` and no value **MUST** make the reference resolve as absent in its tenant and, per its sharing mode, its descendants, leaving the ancestor's credential untouched. Writing a value **MUST** make the record serve it. The policy **MUST** persist while a value is present so that removing the value later applies it. A tenant **MUST** be able to set the policy and remove the value in one request.

**Rationale**: Descendants need a way to opt out of an inherited credential without deleting or shadowing it at the ancestor's expense; the same field lets a tenant fail closed while it is still setting its own. **Actors**: `cpt-cf-credstore-actor-integrations-admin`
<!-- cpt-cf-id-content -->

#### Override Type Consistency

- [ ] `p1` - **ID**: `cpt-cf-credstore-fr-override-type-consistency`

<!-- cpt-cf-id-content -->
When a tenant creates a credential record for a reference that currently resolves to an ancestor's `shared` credential, the new record **MUST** carry the same secret type as the credential it overrides; a differing type **MUST** be rejected as a conflict. Consuming applications address a credential by reference and rely on its type to know the shape of the value, so a local override of a different type would break them without any change on their side. The rule applies only at creation: the type is immutable afterwards, and a reference that resolves to nothing may be created with any registered type.

**Rationale**: The secret type is the contract between the credential and the application that reads it; a tenant must not be able to break that contract unilaterally by shadowing a credential with an incompatible one. **Actors**: `cpt-cf-credstore-actor-integrations-admin`, `cpt-cf-credstore-actor-integration-app`
<!-- cpt-cf-id-content -->

## 6. Non-Functional Requirements

### 6.1 Gear-Specific NFRs

#### Secret Value Confidentiality

- [ ] `p1` - **ID**: `cpt-cf-credstore-nfr-confidentiality`

<!-- cpt-cf-id-content -->
Secret values **MUST NOT** appear in logs, error messages, or debug output at any level (gear, plugin, transport), **MUST NOT** be cacheable by intermediaries, and **MUST NOT** be silently corrupted (non-UTF-8 values are rejected on the string transport rather than lossily decoded). Secret memory is zeroized on drop. Metadata surfaces (the credential record, its listing, and any catalogue view) **MUST NOT** be able to carry a secret value; the restriction is a structural property of the resource, not a convention enforced by review. Every value returned to a caller **MUST** be attributable to a subject and an operation in the audit trail.

**Threshold**: Zero plaintext secret values in any log output **Rationale**: Secrets are the most sensitive data in the platform. **Architecture Allocation**: See DESIGN.md §3.2 for the implementation approach
<!-- cpt-cf-id-content -->

#### Tenant Isolation

- [ ] `p1` - **ID**: `cpt-cf-credstore-nfr-tenant-isolation`

<!-- cpt-cf-id-content -->
No operation may read or modify secret metadata outside the caller's PDP-authorized tenant scope; enforcement happens at the data layer on every query. Inaccessible secrets are indistinguishable from non-existent ones (anti-enumeration). This equivalence **MUST** hold per item inside a bulk response, not only for point reads.

**Threshold**: Zero cross-tenant reads/writes outside the authorized scope **Rationale**: Multi-tenant platform guarantee. **Architecture Allocation**: PDP scope + data-layer clamps; see DESIGN.md §3.1
<!-- cpt-cf-id-content -->

#### Observability

- [ ] `p1` - **ID**: `cpt-cf-credstore-nfr-observability`

<!-- cpt-cf-id-content -->
The gear **MUST** emit operational metrics sufficient to detect resolution anomalies and lifecycle divergence: walk-up depth, read outcome (own/inherited/miss), per-dependency latency and outcome (PDP, tenant-resolver, plugin), cross-tenant denials, saga rollback/reap counters, and per-status inventory gauges — shipped today. **Superseded by ADR-0006 (planned)**: the reaper and its counters/gauges are withdrawn; the periodic maintenance job emits its own counters instead (gc-drain and expiry-removal counts, plus run outcome and duration); no inventory gauge replaces the withdrawn one — a per-status or per-queue-reason gauge would itself require a `COUNT … GROUP BY`, forbidden by the platform's no-`COUNT` rule. Metric labels **MUST NOT** contain secret references or values.

**Rationale**: Sagas and hierarchical resolution — under ADR-0006 (planned), the write/delete protocol and the gc drain — fail in partial, quiet ways; operators need signals, not log archaeology. **Architecture Allocation**: See DESIGN.md §10 Observability
<!-- cpt-cf-id-content -->

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### CredStoreClientV1

- [ ] `p1` - **ID**: `cpt-cf-credstore-interface-client`

<!-- cpt-cf-id-content -->
**Type**: Rust trait (async) **Stability**: stable **Description**: Public API for platform gears. Registered in ClientHub without scope. Operations, as reshaped by ADR-0004: `get` (one credential record: sharing, inheritance status, version, type, expiry — never the value and never the owning tenant), `get_secret` (hierarchical read of the value, at its own address), `put` (precondition-guarded create-or-replace of the record together with its value, in one call), `patch` (precondition-guarded partial update following merge-patch semantics — metadata edit, value rotate, or value remove via a null value; never creates), `list` (an OData query — filter, select, orderby, limit, cursor — over records; `secret` is present on an item only when selected, and selecting it drops `limit`/`cursor` and applies the bulk cap) and `delete` (precondition-guarded). `put` with the create-only precondition is the only way to create a credential, and it always carries a value. Hierarchical resolution is internal to the gear. **Breaking Change Policy**: Major version bump required
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
**Direction**: provided **Protocol/Format**: HTTP/REST, JSON, canonical `Problem` error envelope, under a versioned path served beneath the platform API prefix. Exposes create-only and precondition-guarded update writes, retrieval, and delete over the secret reference, with optional secret-type and expiry inputs on writes, a mandatory `If-Match` precondition on update/delete, and value-confidentiality response controls. See DESIGN.md for the concrete endpoints, methods, status codes, and headers.

**Compatibility**: **not** backward-compatible under [ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md), which is the point of that decision rather than a side effect. The entity address stops returning the value and starts returning the credential record; the collection is renamed; the value read moves to its own address; collection `POST` is removed in favour of `PUT` with a create-only precondition, which stays atomic — record and value in one request; a merge-`PATCH` is added for partial updates (metadata edit, value rotate, value remove); and the PDP action set is replaced with no synonym for the old `read`/`write` pair, so every policy granting them is re-issued. What **does** hold across the change: the reference (`{ref}`) stays the caller-chosen name and never becomes the row id, the `Problem` envelope and the canonical status mapping are unchanged, the strong `ETag` keeps its `"<id>.<version>"` shape, and value-confidentiality controls (`Cache-Control: no-store`, per-value audit) apply to every address that discloses a value. Until ADR-0004 is accepted, the endpoints described in DESIGN §4.3.1 are the ones in service and remain backward-compatible within the major version; DESIGN §4.3.2 describes what replaces them.
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
3. Gear runs the write saga: provisioning row → backend value write → active — shipped today; **superseded by ADR-0006 (planned)**: intent logged → backend value written under a fresh version → row switched to it in one transaction
4. Secret is immediately resolvable by the partner and all descendant tenants

**Postconditions**:
- Secret is stored and accessible to partner and descendants

**Alternative Flows**:
- **Secret already exists (same class)**: value/sharing updated, version bumped
- **Create-only write**: fails with a conflict if the reference is taken in that sharing class
- **Backend write fails**: provisioning row rolled back; reference not wedged; caller retries — shipped today; **superseded by ADR-0006 (planned)**: no row was ever created, only an orphaned backend entry the periodic maintenance job collects; reference never wedged
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
6. OAGW receives the value plus the credential record (inheritance status = inherited, version; the owning tenant is not named — ADR-0004)

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

**Main Flow — shipped today, superseded by [ADR-0006](./ADR/0006-cpt-cf-credstore-adr-immutable-value-versions.md) (planned)**:
1. Tenant deletes the secret by reference
2. The secret enters `deprovisioning` — it instantly stops resolving for every consumer
3. The backend value is deleted; the metadata record is removed

**Postconditions**:
- Secret fully revoked; the reference becomes reusable

**Alternative Flows**:
- **Backend delete fails**: caller gets a retryable failure; the secret already does not resolve; a delete retry or the reaper completes cleanup
- **Create during deprovisioning**: retryable conflict until the backend value is cleaned up (bounded by the reaper cadence)
- **Crash mid-delete**: the reaper finishes the saga within the configured timeout

**Main Flow — planned, ADR-0006**:
1. Tenant deletes the secret by reference
2. One database transaction removes the metadata record and enqueues its version for garbage collection; the reference stops resolving and is free to reuse the instant this transaction commits — no intermediate status, no name-retention window
3. The gear best-effort deletes the backend value; a failure leaves it queued for the periodic maintenance job, never blocking or re-exposing the reference

**Postconditions**:
- Secret fully revoked; the reference is immediately reusable, and a reuse before backend cleanup finishes cannot collide with or be clobbered by that cleanup — the new secret gets its own version identifier

**Alternative Flows**:
- **Backend delete fails**: invisible to the caller — the record and the reference are already gone; the periodic maintenance job's garbage-collection drain reconciles the orphaned backend entry
- **Retry the delete**: idempotent — the record no longer exists, so a retried `DELETE` is an ordinary not-found, not a resumed saga
- **Crash mid-delete**: the transaction either committed (record gone, cleanup queued) or did not (record and reference untouched); there is no partial state to recover
<!-- cpt-cf-id-content -->

#### UC-009: Integration Administrator Configures a Tenant's SMTP Without Seeing Credentials

- [ ] `p1` - **ID**: `cpt-cf-credstore-usecase-admin-configure-without-value`

<!-- cpt-cf-id-content -->
**Actor**: `cpt-cf-credstore-actor-integrations-admin`

**Preconditions**:
- Administrator holds the `list`, `read` and `write` actions in their tenant, but not `read_secret`
- A partner ancestor publishes a `shared` SMTP credential

**Main Flow**:
1. Administrator lists the tenant's credential catalogue and sees the SMTP entry marked as inherited from the partner
2. Administrator creates their own record with its value in one create-only request (never reading the value)
3. Administrator reads the record for its validator, then rotates the value with a guarded partial update carrying only the value
4. At no point does the administrator call the value-read address

**Postconditions**:
- The tenant has its own SMTP credential, rotated under concurrency control, without the administrator ever seeing a plaintext value

**Alternative Flows**:
- **Administrator attempts to read the value**: refused; the response is identical to requesting a reference that does not exist
- **Administrator suppresses the partner's SMTP for this tenant, having already created an own record with a value**: sets fallback `none` and removes the value in one partial update; value reads in the tenant and, with `shared`, in its descendants resolve as absent while the partner's credential is untouched
- **Administrator suppresses without ever having created an own record**: creates a record with a value, then a second request sets fallback `none` removing the value — two requests, since a value-less create is not offered
<!-- cpt-cf-id-content -->

#### UC-010: Mail Service Fetches Its Whole Credential Set in One Call

- [ ] `p1` - **ID**: `cpt-cf-credstore-usecase-bulk-fetch-own-set`

<!-- cpt-cf-id-content -->
**Actor**: `cpt-cf-credstore-actor-integration-app`

**Preconditions**:
- The application is authorized to read values of type `gts.cf.core.credstore.credential.v1~cf.smtp_sender.creds.smtp.v1~` only, in the tenant it acts for

**Main Flow**:
1. Application issues one collection read, `$filter=type eq '<smtp type>'` and `$select=reference,type,expires_at,secret`
2. The system resolves and authorizes the matching type with a single `read_secret` decision, then returns every matching credential in one response, each item carrying the value with its type and expiry and nothing administrative
3. Credentials of other types are not evaluated and do not appear in the result

**Postconditions**:
- The application has its full SMTP credential set from a single round-trip, scoped exactly to its own grant

**Alternative Flows**:
- **Selection would match more items than the cap allows**: the request fails outright; the result is never truncated
<!-- cpt-cf-id-content -->

#### UC-011: Tenant Overrides, Suppresses, and Returns to an Inherited Credential

- [ ] `p1` - **ID**: `cpt-cf-credstore-usecase-override-suppress-return`

<!-- cpt-cf-id-content -->
**Actor**: `cpt-cf-credstore-actor-integrations-admin`

**Preconditions**:
- T1 (a partner tenant) publishes `smtp-default` as a `shared` credential with value V1
- T2 is T1's child tenant; T3 is T2's child tenant; neither holds a row under `smtp-default` at the start
- The administrator, acting in T2, holds `write`, `write_secret` and `delete` on the reference

**Main Flow**:
1. In T2, the administrator creates a record for `smtp-default` with value V2 in one create-only request; T2's record becomes its own, active credential, and T2 and T3 now resolve V2, overriding T1's V1
2. In T2, the administrator rotates the value to V3 with a guarded partial update; T2 and T3 resolve V3
3. In T2, the administrator sets the record's fallback to `none` and removes the value in one partial update; the reference now resolves to nothing in T2 and, because the record is `shared`, in T3 as well; T1's record and value, and T1's other descendants, are unaffected
4. In T2, the administrator deletes the record entirely; T2 and T3 fall back to resolving T1's value V1 again

**Postconditions**:
- The tenant hierarchy passes through override, suppression and a return to inheritance without ever exposing an intermediate state in which the wrong tenant's value is served, and without the partner's credential being touched at any step

**Alternative Flows**:
- **Soft return instead of step 4**: the administrator sets the record's fallback back to `inherit` without deleting the record; T2 and T3 resolve T1's value V1 again, and T2 keeps its own record (still value-less, still reserving the reference) for a future override
- **A tenant without an own record that wants to block T1's credential**: two requests — a create with a value, then a partial update that sets fallback `none` and removes the value — since a value-less create is not offered
<!-- cpt-cf-id-content -->

## 9. Acceptance Criteria

- [ ] Tenant can store, retrieve, and delete secrets via both ClientHub and REST API
- [ ] Create-only writes conflict on a same-class duplicate; updates require a precondition and never create
- [ ] Private secrets are accessible only to the owner; multiple owners can hold private secrets under one reference; a private and a tenant/shared secret coexist under one reference
- [ ] Tenant secrets are accessible to all subjects within the owning tenant and never inherited; shared secrets are inherited by all descendants
- [ ] Shadowing: the closest accessible secret wins; inaccessible private rows do not block fallback
- [ ] OAGW can retrieve secrets on behalf of any tenant it is authorized for, through the standard API
- [ ] Every operation is PDP-authorized and scope-clamped at the data layer; inaccessible reads are not-found; operation-level denial is refused; a PDP outage fails closed
- [ ] Half-written secrets are never readable; failed writes roll back; stuck lifecycle rows are reaped within the configured timeout — shipped today via a saga; **superseded by ADR-0006 (planned)**: no status 1 (`provisioning`) or 3 (`deprovisioning`) row can exist, because a write only ever produces a fully `active` row or none at all; **no background timer runs inside the gear** — the reaper is withdrawn, and the equivalent maintenance work is a periodic job invoked on an operator-chosen schedule, outside the gear's own lifecycle
- [ ] `p1` **Immutable value versions (planned, ADR-0006)**: every value write creates a new backend entry under a fresh version identifier and switches the record's pointer in one transaction, only after the bytes are fully written; a torn write (crash after the backend write, before the pointer switch) leaves the old value still served and the new, unreferenced entry collected by the periodic maintenance job — never a wrong value, a closed read, or a reserved name; no row can carry status `provisioning` (1) or `deprovisioning` (3)
- [ ] Retrieval exposes the current version and value-confidentiality controls; update/delete require a precondition, with a conflict on stale versions and a distinct validation error when it is missing
- [ ] Secret values never appear in log output or metric labels; non-UTF-8 values are rejected on the REST transport, not corrupted
- [ ] Secret types: a write violating the type's `allow_sharing`, `value_schema`, size/format, or expiry traits is rejected with a stable reason; the type is immutable, defaults to `generic`, and is returned in metadata; expired secrets resolve as not-found and are reaped — shipped today; **superseded by ADR-0006 (planned)**: removed by the periodic maintenance job, same as an ordinary delete
- [ ] Deprovisioning: a deleted secret stops resolving atomically at delete start; partial delete failures self-heal via retry or reaper; the reference conflicts (retryably) until cleanup completes — shipped today; **superseded by ADR-0006 (planned)**: delete is one transaction (the secret stops resolving and the reference is free to reuse the instant it commits, no intermediate status), followed by best-effort backend cleanup the periodic maintenance job guarantees; there is no conflict window, because a successor write never shares the deleted version's backend key
- [ ] Credential records never carry a secret value; the value is a separate sub-resource of the record
- [ ] Listing returns credential records only, paginated per the platform cursor contract, filterable and orderable only on allowlisted indexed fields, with no total count, and requires its own authorization action
- [ ] A single credential record can be read by reference without disclosing its value, carrying the current version validator; an inaccessible or non-resolving record is indistinguishable in the response
- [ ] A credential can be created or fully replaced in one request via `PUT` — record and value together — under a create-only, guarded-replace, or last-writer-wins precondition; the secret type never changes; a partial update via `PATCH` applies present fields as `PUT` would, leaves absent fields untouched, never creates, and a `null` value removes the secret value without deleting the record; a record without a value neither resolves for value reads nor hides an inherited value
- [ ] A credential's value can be read at its own address, hierarchically resolved, with caching disabled and one audit record per value returned
- [ ] Several credential values can be read in one request by selecting `secret` on the credential collection, scoped by an explicit `reference in (...)` list or a `type eq/in` filter; each item is authorized individually, `limit` and `cursor` are rejected, the response is never paginated or continuable, and exceeding the cap fails the request rather than truncating it; a refused item is omitted rather than reported
- [ ] Authorization distinguishes six actions — list records, read one record, write a record, read a value, write a value, delete — and no policy grants value access as a side effect of a metadata grant
- [ ] Every credential record states whether it is owned, inherited, or an override of an ancestor's shared credential, and this status is readable with metadata-only access
- [ ] `p2` A tenant can suppress an inherited credential so it resolves as absent locally and for its descendants, without altering the ancestor's credential
- [ ] A value-less record with fallback `none` makes its reference resolve as absent for its tenant and, when `shared`, its descendants; the ancestor's credential is untouched; a single partial update can set fallback `none` and remove the value together
- [ ] A partial update with a null value leaves a value-less record whose fallback decides resolution; a partial update that carries only metadata by a caller holding only the record-write action succeeds, and one that carries a value by that caller is refused

## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| `authz-resolver` | PDP: per-operation access-scope evaluation (fail-closed) | `p1` |
| `tenant-resolver` | Tenant ancestor chains for hierarchical resolution | `p1` |
| `types-registry` | GTS plugin discovery; secret resource type + secret-type registrations | `p1` |
| Database (PostgreSQL / SQLite) | Gear-owned secret metadata (`credstore_secrets`) | `p1` |
| Value-store plugin | Per-tenant secret value persistence (`static-credstore-plugin` for dev/test; production vault plugin planned) | `p1` |
| OAGW | Primary consumer of hierarchical secret retrieval (uses the SDK client) | `p1` |
| PDP policy re-issuance | Existing `read` grants must be re-issued under the six-action split (list records, read record, write record, read value, write value, delete) before the split ships | `p1` |
| Database indexes on secret type and reference | The bulk filtered secret read depends on indexes over the allowlisted metadata fields; without them the filter is a sequential scan | `p1` |

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
| Metadata/backend divergence on partial saga failure — shipped today, superseded by ADR-0006 (planned) | Orphaned backend values, temporarily wedged references | Compensating rollback; deprovisioning saga; reaper backend reconciliation with configurable timeouts; saga metrics |
| Garbage until the next maintenance-job run (planned, ADR-0006) | An immutable backend entry outlives the switch or delete that superseded it, until the periodic maintenance job's next run | No resident reaper — a scheduled job (daily by default, weekly acceptable) drains the gc queue each run in bounded batches (`gc.batch_size`); the entry is never referenced by any row, so it is a storage cost only, never a correctness risk; a tighter destruction requirement is met by running the job more often |
| Pending intent older than `gc.pending_max_age_secs` (planned, ADR-0006) | A write's intent was logged but the row CAS never landed (crash, or an aborted CAS-loser whose own cleanup failed) | The maintenance job reclaims an unreferenced `pending` entry once older than `gc.pending_max_age_secs`; a defensive check refuses to delete an entry a live row still references |
| Expired record lingers in the catalogue and holds its reference until the job (planned, ADR-0006) | An expired credential stops resolving at read time but its record, past `expires_at`, is not removed until the periodic maintenance job's next run or an owner action | 409 on a create-only `PUT` under that reference until then; the owner uses `PUT If-Match`/`PATCH`/`DELETE` to reclaim it immediately rather than waiting for the job |
| PDP or tenant-resolver outage | Operations fail closed (unavailable) | Ancestor-chain cache absorbs blips; dependency metrics for fast diagnosis |
| Ancestor-chain cache staleness | A re-parented tenant keeps inheriting its former parent's `shared` credentials for up to the cache TTL | Short TTL + LRU. The own-tenant gate is **not** a control here: it validates the caller's tenant, not the ancestors the cached chain names, which is precisely what lets an inherited read work. Closing the window needs a hierarchy version or change signal from the tenant resolver, which is work outside this gear; until then the TTL is the settling time for credential inheritance after a move |
| In-memory static plugin in non-dev use | Secret values lost on restart | Production vault plugin (`cpt-cf-credstore-fr-production-backend`); deployment policy |
| Type-trait misconfiguration | Overly permissive or broken writes for a type | Compiled-in catalog pinned to registered GTS schemas by unit tests; catalog changes are code-reviewed SDK releases; `generic` keeps legacy behavior |

## 13. Open Questions

- ~~**Batch retrieval**: should `get` support multiple references per call?~~ **Answered** by [ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md): the credential collection's `$select` gains a `secret` field, selectable by an explicit reference list or by a `type` filter, non-paginated, authorized per item and capped by cardinality (`cpt-cf-credstore-fr-bulk-read-secrets`).
- ~~**P2/Future — Human vs service access**: should human users be restricted to metadata-only?~~ **Answered** by [ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md) and `cpt-cf-credstore-fr-authz-action-split`: the restriction is expressed by granting metadata actions without the value-read action, and applies to any principal kind rather than being derived from whether the subject is human.
- **P2/Future — Audit trails**: structured audit events (actor, tenant, outcome — never values) to a tamper-evident platform sink.
- ~~**P2/Future — Metadata list endpoint**~~ **Answered** by [ADR-0004](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md), which fixes that a listing exists (`cpt-cf-credstore-fr-list-credentials`) and never carries a value unless the caller selects `secret` via `$select` — in which case the same collection also serves the bulk value read (`cpt-cf-credstore-fr-bulk-read-secrets`), under its own cap and without pagination — and by [ADR-0005](./ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md), which is the dedicated ADR this entry asked for: the collection is rooted at the caller's tenant and reads upward only, and the flat tenant predicate gates the request instead of clamping rows, which is what lets an inherited entry appear at all. Both are `proposed`, so the answer stands or falls with their acceptance.
- ~~**One item per reference**~~ **Answered** by [ADR-0005](./ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md) "Pagination over a reduced result": because the canonical order leads with `reference`, every row of one reference is contiguous, so a cursor always sits on a reference boundary and no reference can be split across a page. **Still open**: this is the platform's first row-reducing cursor pagination, so its page-boundary behaviour needs its own test suite before the endpoint ships.

## 14. Traceability

- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: [ADR/](./ADR/) — [ADR-0001 stateful gear](./ADR/0001-cpt-cf-credstore-adr-stateful-gear.md), [ADR-0002 deprovisioning saga](./ADR/0002-cpt-cf-credstore-adr-deprovisioning-saga.md) (superseded by ADR-0006), [ADR-0003 value-fingerprint fence](./ADR/0003-cpt-cf-credstore-adr-value-fingerprint-fence.md) (amended by ADR-0006), [ADR-0004 credential record and secret value as separate resources](./ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md) (`proposed`), [ADR-0005 upward-rooted collection read](./ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md) (`proposed`), [ADR-0006 immutable value versions](./ADR/0006-cpt-cf-credstore-adr-immutable-value-versions.md) (`proposed`)
- **Features**: features/ (planned)
