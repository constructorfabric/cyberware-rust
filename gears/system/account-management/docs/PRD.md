Created:  2026-03-30 by Virtuozzo International GmbH
Updated:  2026-08-17 by Virtuozzo International GmbH

# PRD - Account Management (AM)


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
  - [3.2 IdP Integration Boundary](#32-idp-integration-boundary)
  - [3.3 Barrier Tenant Isolation](#33-barrier-tenant-isolation)
  - [3.4 User Data Ownership](#34-user-data-ownership)
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
  - [4.3 Deferred Detailed Coverage](#43-deferred-detailed-coverage)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 Platform Bootstrap](#51-platform-bootstrap)
  - [5.2 Tenant Hierarchy Management](#52-tenant-hierarchy-management)
  - [5.3 Tenant Type Enforcement](#53-tenant-type-enforcement)
  - [5.4 Managed/Self-Managed Tenant Modes](#54-managedself-managed-tenant-modes)
  - [5.5 IdP Tenant & User Operations Contract](#55-idp-tenant--user-operations-contract)
  - [5.6 User Groups Management](#56-user-groups-management)
  - [5.7 Extensible Tenant Metadata](#57-extensible-tenant-metadata)
  - [5.8 Deterministic Error Semantics](#58-deterministic-error-semantics)
  - [5.9 Observability Metrics](#59-observability-metrics)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Tenant Context Validation Latency](#61-tenant-context-validation-latency)
  - [6.2 Authentication Context](#62-authentication-context)
  - [6.3 Tenant Isolation Integrity](#63-tenant-isolation-integrity)
  - [6.4 Audit Trail Completeness](#64-audit-trail-completeness)
  - [6.5 Barrier Enforcement](#65-barrier-enforcement)
  - [6.6 Tenant Model Versatility](#66-tenant-model-versatility)
  - [6.7 API and SDK Compatibility](#67-api-and-sdk-compatibility)
  - [6.8 Expected Production Scale](#68-expected-production-scale)
  - [6.9 Data Classification](#69-data-classification)
  - [6.10 Reliability](#610-reliability)
  - [6.11 Data Lifecycle](#611-data-lifecycle)
  - [6.12 Data Quality](#612-data-quality)
  - [NFR Exclusions](#nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
  - [8.1 Bootstrap](#81-bootstrap)
  - [8.2 Tenant Lifecycle](#82-tenant-lifecycle)
  - [8.3 Managed/Self-Managed Modes](#83-managedself-managed-modes)
  - [8.4 User Groups](#84-user-groups)
  - [8.5 IdP User Operations](#85-idp-user-operations)
  - [8.6 Extensible Tenant Metadata](#86-extensible-tenant-metadata)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Review Baseline Decisions](#13-review-baseline-decisions)
- [14. Open Questions](#14-open-questions)
- [15. Traceability](#15-traceability)

<!-- /toc -->

> **Abbreviations**: Account Management = **AM**; Global Type System = **GTS**. Used throughout this document.

## 1. Overview

### 1.1 Purpose

AM is the foundational multi-tenancy source-of-truth gear for the Gears platform. It provides hierarchical tenant management, tenant isolation metadata, delegated administration, and a pluggable Identity Provider (IdP) integration contract for administrative user lifecycle operations.

AM enables diverse organizational models — from cloud hosting (Provider / Reseller / Customer) to enterprise divisions and managed-service providers — within a single deployment, supporting both managed (delegated administration) and self-managed (barrier-isolated) tenant modes in the same hierarchy.

Here, a deployment means one installed/running Gears environment with a single AM root tenant and one configured tenant-type topology. Different organizational shapes are modeled as tenant hierarchies inside that deployment, not as separate platform installations.

### 1.2 Background / Problem Statement

Gears need a unified multi-tenancy model that supports diverse organizational structures and business models within a single deployment. Without a shared tenant hierarchy, each organizational model requires separate infrastructure or custom integration, increasing operational cost and limiting platform scalability.

Tenant hierarchy is needed because many customers operate as parent/child organizations rather than flat accounts. It lets the platform model delegated administration, governance boundaries, inherited defaults, and future billing/reporting flows in a way that matches real customer structure.

The platform needs a tenant model with unlimited hierarchy depth (configurable advisory threshold), visibility barriers for self-managed tenants, and consistent tenant context propagation across all services.

**Representative use-cases the gear is designed for:**

| Domain | Use-case | How AM is used |
|--------|----------|-----------------|
| **Cloud hosting** | Model a service-provider channel: *Provider → Reseller → Customer*. Provider onboards resellers who sell to end-customers. | Each level is a GTS-registered tenant type; individual organizations are tenant entities forming a tree. Provider manages resellers and customers through a single hierarchy view. |
| **Education** | Model a university consortium: *Consortium → University → College*. Consortium provides shared infrastructure; each university operates independently. | Consortium is root tenant; universities are child tenants with their own billing, security policy, and data isolation. A college (e.g., School of Medicine) may be self-managed to enforce stricter data-access rules than the parent university. |
| **Enterprise** | Model a corporation with divisions: *HQ → Region → Business Unit*. Each unit manages its own users and resources while HQ retains oversight. | HQ is root tenant; regions and business units are child tenants. `BarrierMode` controls whether HQ can traverse into a self-managed unit; a self-managed unit's metadata is independent (no inheritance from HQ). |
| **MSP / Managed Services** | A managed service provider operates customer environments on their behalf. | Parent-child managed relationship (no barrier). Parent remains eligible for delegated administration of child environments subject to platform authorization policy; the exact v1 access mechanism is an open question. |
| **Distributor / Reseller** | A distributor resells platform capacity but does not access customer data. | Self-managed child tenants create visibility barriers. Distributor sees billing metadata (`BarrierMode::Ignore`) but cannot access customer APIs or resources. |

### 1.3 Goals (Business Outcomes)

- Reduce tenant onboarding to one AM tenant-create call plus provider-executed provisioning, replacing the current multi-step hierarchy and IdP setup workflow with one stable contract for tenant hierarchy, type enforcement, barrier semantics, and user group coordination (via [Resource Group](../../resource-group/docs/PRD.md)).
- Run both managed (no barrier — delegated administration) and self-managed (with barrier — independent operation) tenant modes inside the same deployed tenant tree, eliminating the need for separate per-model deployment variants across MSP, reseller, and enterprise topologies.
- Support hierarchies beyond the default advisory threshold of 10 levels in the AM data model while keeping the default behavior warning-only, preserving a configurable strict rejection mode, and grounding production support in benchmark-backed deployment-profile limits.
- Hold end-to-end tenant-context validation to p95 ≤ 5ms on the approved deployment profile.
- Pass the full AM cross-tenant isolation test suite with zero observed data leaks.
- Prove the pluggable IdP integration contract against at least two conforming provider implementations for tenant and user lifecycle operations.

**Success criteria:**

| Metric | Baseline | Target | Timeframe |
|--------|----------|--------|-----------|
| Tenant onboarding effort | 5+ API calls and manual IdP configuration per tenant (*baseline estimate from the current pre-AM workflow: create tenant record, provision IdP realm/org, bind admin identity, create root user-group, set barrier/policy flags; no production measurement exists because AM itself replaces this workflow, so the baseline is a design-time estimate used for GA pass/fail rather than a measured SLO*) | Single API call — tenant create with type and mode | Gear GA |
| Separate deployments per tenant model | 2 separate deployment configurations required today (one per managed/self-managed pattern) | Both modes in one tenant tree — zero per-model deployments (pass/fail) | Gear GA |
| End-to-end tenant context validation latency (p95) | 0 approved AM-backed benchmarks against the target | ≥ 1 approved pre-GA benchmark on the deployment profile with p95 ≤ 5ms through the resolver path | Pre-GA load test gate |
| Cross-tenant data leaks | 0 AM-specific automated isolation scenarios wired into the gate today | Zero leaks observed; 100% of the planned AM isolation scenarios pass | Pre-GA security gate |
| Hierarchy depth coverage | Fixed shallow hierarchies (2–3 levels) | No hard AM structural depth cap; configurable advisory threshold (default: 10); benchmark-backed validation of at least 3 distinct type topologies, at least one of which reaches **≥ 15 levels** (50% above the default advisory threshold) on the approved deployment profile defined in §13 | Pre-GA performance + integration gate |
| Conforming IdP provider implementations | 0 provider implementations validated against the AM contract suite | ≥ 2 provider implementations pass the AM tenant/user lifecycle contract suite without breaking the public API contract | Pre-GA integration gate |

### 1.4 Non-goals

- User authentication flows (covered by IAM PRD).
- Authorization policy evaluation or SQL predicate generation (covered by AuthZ Resolver).
- Resource provisioning and lifecycle for non-identity platform resources. AM owns tenant hierarchy and tenant-scoped administrative metadata, not downstream resource CRUD or provisioning workflows.
- Being an IdP implementation — AM consumes IdP, not replaces it.
- User-tenant reassignment (moving a user between tenants) — deferred from v1; requires cross-platform coordination (Resource Group membership migration, resource ownership transfer, AuthZ cache invalidation, session revocation).
- Hierarchy reparenting (moving a tenant subtree to a new parent) — deferred from v1; requires explicit subtree-move semantics, pending conversion-request handling, depth recomputation for the full subtree, and restrictions on moves that would cross effective IdP provisioning boundaries.
- Partial metadata updates (HTTP `PATCH`) — v1 tenant metadata uses `PUT` full-replace semantics; field-level merge/patch is deferred and will be revisited once client workloads show need.
- Metadata change events / CloudEvents — no push notifications on metadata mutations in v1; deferred alongside the broader tenant lifecycle events until the Events and Audit Bus (EVT) is introduced. Consumers poll resolution APIs in the meantime.
- Secret storage via tenant metadata — AM metadata is **not** a secrets store. API keys, SSO client secrets, webhook signing keys, and any value requiring encryption-at-rest, per-access audit, or key rotation **MUST** use the platform secret manager; tenant metadata may hold only opaque references (IDs, URIs) to secrets stored elsewhere.

### 1.5 Glossary

> Platform-wide terms (Tenant, Subject Tenant, Context Tenant, Resource Tenant) are defined canonically in the [Authorization Core Terms](../../../../docs/arch/authorization/DESIGN.md#core-terms) and the [Tenant Model](../../../../docs/arch/authorization/TENANT_MODEL.md). This glossary re-states them briefly for self-containedness and adds AM-specific terms.

| Term | Definition |
|------|------------|
| GTS (Global Type System) | Platform registry for runtime-extensible type definitions, validation rules, and schema-based constraints. AM uses GTS to register and validate tenant types and metadata schemas. |
| Tenant | Logical organizational entity representing a company, division, team, or individual. The fundamental unit of multi-tenancy isolation. |
| Subject Tenant | Tenant the subject (user or API client) belongs to. Used for authorization context. |
| Context Tenant | Tenant scope root for an operation. May differ from Subject Tenant in platform-authorized cross-tenant administrative scenarios. |
| Resource Tenant | Actual tenant owning a specific resource. |
| Self-Managed Tenant | Child tenant that creates a visibility barrier for barrier-enforced subtree/resource reads. Barrier handling is resource-type dependent per the platform tenant model: parent-side reads of child tenant metadata may still be policy-authorized, while barrier-respecting subtree/resource traversal stops at the self-managed boundary. AM's only gear-specific v1 carve-out is the dedicated conversion-request discovery surface (`/child-conversions`), which exposes minimal request metadata required for the dual-consent workflow. |
| Managed Tenant | Child tenant where the parent is eligible for controlled access to child tenant APIs and resources per policy. No visibility barrier exists. |
| Tenant Barrier | Visibility and access boundary created by self-managed tenants for barrier-enforced reads and subtree traversal. Whether the barrier applies is resource-type dependent per the platform tenant model; AM itself adds no gear-specific bypass beyond the narrow conversion-request metadata carve-out described for `/child-conversions`. |
| Barrier Mode | Authorization parameter controlling barrier handling: `Respect` (default) stops traversal at self-managed boundaries; `Ignore` traverses through barriers for operations like billing queries. Values align with the `BarrierMode` SDK enum. Tenant metadata resolution does not use `BarrierMode::Ignore` — inheritance stops at self-managed boundaries instead (see §5.7). |
| Tenant Tree | Single rooted hierarchy of tenants. Has exactly one root tenant (`parent_id = NULL`) created at platform bootstrap. |
| Root Tenant | Top-level tenant in the tree (`parent_id = NULL`). Exactly one root tenant exists per deployment, created automatically during platform bootstrap. |
| Tenant Context | Tenant scope carried in the authorization request's `tenant_context` (separate from `SecurityContext`) to enforce isolation and scoping. |
| Tenant Status | Lifecycle state of a tenant: `active`, `suspended`, or `deleted`. |
| Conversion Request | A durable record representing a pending or resolved request to toggle a tenant's `self_managed` flag. Owns its own identifier, status lifecycle, approval window, and audit trail; addressable via the collection-based mode conversion REST API. |
| Conversion Request Status | Lifecycle state of a mode conversion request: `pending` (awaiting counterparty decision), `approved` (counterparty consented, conversion applied), `cancelled` (**initiator withdrew** the request from their own scope), `rejected` (**counterparty declined** the request from their own scope), or `expired` (configured approval window elapsed without resolution). `cancelled` and `rejected` are distinct terminal states — the distinction is carried in the status itself, not only in audit. |
| Tenant Type | Classification of a tenant node. Types are extensible at runtime via the GTS types registry. Deployments define their own type topology. |
| User | A human subject managed by AM via the IdP contract (provisioning, tenant binding, group membership). Corresponds to the platform-level [Subject](../../../../docs/arch/authorization/DESIGN.md#core-terms) term narrowed to human identities; API clients and service accounts are not AM-managed users. |
| IdP Contract | Abstract pluggable interface for Identity Provider operations (tenant provisioning, tenant deprovisioning, user provisioning, update, deprovisioning, tenant-scoped user query, and the service-account lifecycle). |
| Service Account | A tenant-owned, non-human identity used by a workload that must authenticate without human credentials: a confidential OAuth 2.0 `client_credentials` client whose issued tokens carry the owning tenant and a service-subject type. Managed as a resource type distinct from `User`, so a grant over users never confers machine-credential minting. |
| Client Credentials | OAuth 2.0 grant in which a confidential client authenticates with its client identifier and secret. The only grant a service account supports. |
| One-Time Disclosure | Exposure of a plaintext client secret through the service-account provision and rotate-secret responses only. No read-back path exists; recovery from a lost secret is a rotation. |
| Ambiguous Outcome | An IdP contract failure category in which the provider may have retained state, so the operation cannot be reported as either a success or a safely retryable failure. The caller reconciles by matching the name it submitted in the tenant's listing. |
| User Group | A [Resource Group](../../resource-group/docs/PRD.md) entity with a Resource Group type configured for user membership (`allowed_memberships` includes the user resource type). AM delegates group hierarchy, membership management, and cycle detection to the Resource Group gear. |
| Tenant Metadata Schema | A GTS-registered schema that defines a category of extensible tenant data (e.g., branding, contacts), its validation rules, and its `inheritance_policy` trait; identified by its chained `schema_id`. |
| Inheritance Policy | The `inheritance_policy` trait on a tenant metadata schema, controlling parent-to-child propagation: `override_only` (default; each tenant sets its own value, no inheritance) or `inherit` (child inherits parent value unless overridden). |

## 2. Actors

### 2.1 Human Actors

#### Platform Administrator

**ID**: `cpt-cf-account-management-actor-platform-admin`

- **Role**: Operator of the platform with full access to the tenant tree, platform configuration, and bootstrap operations.
- **Needs**: View and manage the full tenant tree, perform platform bootstrap, override barriers for billing and administrative operations, monitor tenant health and isolation integrity.

#### Tenant Administrator

**ID**: `cpt-cf-account-management-actor-tenant-admin`

- **Role**: Administrator of a specific tenant who manages sub-tenants, users, groups, extensible tenant metadata, and tenant configuration within their scope.
- **Needs**: Create and manage child tenants, configure tenant metadata (branding, contacts, etc.) via GTS-registered schemas, manage user groups and memberships (via Resource Group), and control tenant mode (managed/self-managed).

### 2.2 System Actors

#### Tenant Resolver Plugin

**ID**: `cpt-cf-account-management-actor-tenant-resolver`

- **Role**: Query facade over the AM-owned tenant hierarchy. Reads `tenants` and `tenant_closure` directly from the AM database over a read-only role provisioned by AM with `SELECT`-only grants, and serves barrier-aware subtree, ancestor, and existence queries on the authorization hot path. Holds no derived state of its own — every query reflects AM's currently committed state.

#### AuthZ Resolver Plugin

**ID**: `cpt-cf-account-management-actor-authz-resolver`

- **Role**: Evaluates authorization decisions using tenant context, barrier semantics, and subtree scoping constraints. Consumes the barrier-aware query facade exposed by [Tenant Resolver](../../tenant-resolver/) for query-level tenant isolation.

#### IdP Provider

**ID**: `cpt-cf-account-management-actor-idp`

- **Role**: Pluggable identity provider that manages user authentication, token issuance, and user-tenant binding. Provides tenant identity via user attributes in tokens. Conforms to the AM IdP contract for user lifecycle operations.

#### Machine Workload

**ID**: `cpt-cf-account-management-actor-machine-workload`

- **Role**: A service, job, agent, or automation process that authenticates without a human user. Never calls AM: it uses the credentials AM's service-account operations obtained for it to request access tokens through the OAuth 2.0 `client_credentials` grant, and loses the ability to authenticate once that credential is superseded by a rotation or removed by a revocation.

#### Billing System

**ID**: `cpt-cf-account-management-actor-billing`

- **Role**: Consumes tenant hierarchy metadata with barrier bypass (`BarrierMode::Ignore`) for billing aggregation and reporting across the tenant tree.

#### GTS Registry

**ID**: `cpt-cf-account-management-actor-gts-registry`

- **Role**: Provides runtime-extensible type definitions for tenant types, enabling new tenant classifications without code changes. Validates parent-child type constraints at tenant creation time.

**Downstream consumer resilience**: Billing System and AuthZ Resolver Plugin are downstream consumers of AM data. Their resilience to AM unavailability is their own concern. Tenant Resolver Plugin reads AM-owned storage directly via a read-only database role, so its availability tracks AM database availability and it holds no independent derived state to reconcile.

**Downstream SLA contract model**: AM does **not** issue per-consumer SLA contracts (availability, freshness, or convergence windows) to its downstream consumers. The architecture makes those guarantees unnecessary rather than negotiated:

| Consumer | Freshness / availability model | Basis |
|----------|-------------------------------|-------|
| Tenant Resolver Plugin | Atomic: every AM commit is immediately visible. Query latency tracks AM database read latency and is bounded by the tenant-context-validation target in §6.1 / §9 (p95 ≤ 5ms). Availability tracks AM database availability. | Stateless DB facade; no derived state, no projection window. |
| AuthZ Resolver Plugin | Consumer-owned: AuthZ decides its own cache-invalidation and convergence strategy over the AM-owned hierarchy and barrier state exposed by the Tenant Resolver. AM neither commits to a propagation deadline nor maintains an AM-side cache. | Downstream consumer; responsible for its own resilience (see above). |
| Billing System | Consumer-owned, batch-friendly: Billing aggregates tenant hierarchy on its own cadence using `BarrierMode::Ignore`. AM commits no tight convergence window; hours-scale lag is architecturally acceptable. | Downstream consumer; responsible for its own resilience (see above). |

The absence of AM-side SLA contracts is a deliberate architectural choice, not an omission: AM is an administrative source-of-truth gear (see §3), and any per-consumer freshness guarantee would either require AM-owned caching/replication (rejected in [ADR-0004](ADR/0004-cpt-cf-account-management-adr-resource-group-tenant-hierarchy-source.md)) or shift AM into the request-path enforcement role it explicitly refuses (see §3).

## 3. Operational Concept & Environment

AM is the control-plane source of truth for tenant hierarchy, tenant mode, tenant metadata, and delegated user lifecycle intent. It is an administrative gear, not a request-path enforcement gear. Authentication, authorization, token handling, and tenant-scoped runtime enforcement are inherited from the platform AuthN/AuthZ architecture in [docs/arch/authorization/DESIGN.md](../../../../docs/arch/authorization/DESIGN.md).

### 3.1 Core Boundary

AM:

- owns the tenant hierarchy and tenant metadata (source of truth).
- validates structural invariants such as type compatibility, single-root topology, root immutability, and barrier-mode transitions.
- exposes authoritative tenant data consumed by Tenant Resolver, AuthZ Resolver, Billing System, and other platform components.
- coordinates tenant and user lifecycle operations against the configured IdP provider.

AM does not:

- evaluate authorization decisions or emit query predicates.
- validate bearer tokens, manage sessions, or construct `SecurityContext`.
- own user credentials, user profiles, or user-group storage.
- provision non-identity resources outside its own tenant and metadata domain.

### 3.2 IdP Integration Boundary

AM consumes a pluggable outbound IdP contract for tenant provisioning and deprovisioning, tenant-scoped user provisioning, update, and deprovisioning, and user listing. AM defines the expected lifecycle outcomes and deterministic public error categories; the concrete provider protocol, provider credentials, federation setup, and session policy remain owned by the IdP implementation and the platform AuthN layer. The IdP contract is intentionally separate from the AuthN Resolver contract because AM performs low-frequency administrative operations, while AuthN Resolver handles request-path token validation.

### 3.3 Barrier Tenant Isolation

AM stores the tenant mode (`managed` or `self-managed`) and the hierarchy state that downstream [Tenant Resolver](../../tenant-resolver) and [AuthZ Resolver](../../authz-resolver) use to enforce visibility. In the normal platform mode:

- managed children remain eligible for delegated parent access per policy;
- self-managed children create a visibility barrier between parent and child subtrees;
- nested self-managed boundaries are allowed;
- authorized platform-owned consumers such as Billing System may use platform-level barrier-bypass modes outside AM itself.

AM may still perform internal hierarchy-owner reads for two non-policy purposes: validating structure-changing operations against the full tenant tree, and stopping metadata inheritance at self-managed boundaries. Parent-side visibility of child tenant metadata follows the platform tenant model and platform AuthZ policy rather than an AM-specific bypass. The only additional AM-owned structural-read carve-out is parent-side discovery of child conversion requests, and that exception returns only minimal request metadata needed for the dual-consent workflow.

### 3.4 User Data Ownership

AM coordinates user lifecycle operations but **does not own user identity data**. The ownership boundaries are:

| Data | System of record | AM role |
|------|-----------------|----------|
| User identity, credentials, authentication state, session policy | IdP / platform AuthN | Not stored by AM. |
| User-tenant binding | IdP | AM coordinates create/delete/query operations against the binding but does not become the canonical store. |
| User-group hierarchy and membership | Resource Group | AM depends on RG for structure and membership storage. |
| User identifiers in audit records | Platform audit infrastructure | AM emits IdP-issued UUID identity references for traceability only. |

**Key invariants**:

- IdP is the single source of truth for "user X exists and belongs to tenant Y."
- User IDs exposed through AM are IdP-issued UUIDs; AM passes them through for audit and Resource Group integration but does not mint a separate user identity model.
- AM stores no credentials, no profile cache, and no local user projection.
- Group membership cleanup after user deprovisioning is a future cross-gear lifecycle capability and is not solved in v1.

## 4. Scope

### 4.1 In Scope

- Platform bootstrap: initial root tenant auto-creation during install, idempotent (p1).
- Tenant type classification via GTS registry with configurable parent-child constraints (p1).
- Tenant hierarchy: single rooted tree with parent-child relationships and unlimited depth with a configurable advisory threshold (p1).
- Canonical transitive-ancestry storage: AM owns the `tenant_closure` table in the platform-canonical shape `(ancestor_id, descendant_id, barrier, descendant_status)` defined in TENANT_MODEL.md, maintains it transactionally with tenant writes, and exposes it to the Tenant Resolver Plugin through a read-only database role (p1).
- Managed tenant model: parent eligible for delegated child administration with no barrier, subject to platform authorization policy (p1).
- Self-managed tenant model: strict isolation via barrier; metadata inheritance stops at self-managed boundaries (p1).
- Tenant CRUD operations: create, read, update, soft-delete with configurable retention (p1).
- IdP user operations contract: pluggable contract for user provisioning, update, deprovisioning, and query (p1).
- Service accounts: tenant-scoped machine identities over the same pluggable IdP contract — provision, list, rotate-secret, revoke — with one-time secret disclosure and four independently grantable permissions on a resource type distinct from `User` (p1).
- User groups management (via Resource Group): create groups, manage membership, nested groups with cycle detection delegated to Resource Group (p1).
- Observability metrics: domain-specific metrics exported via platform observability conventions (OpenTelemetry) (p1).
- Extensible tenant metadata: GTS-registered schemas for tenant-specific data kinds (e.g., branding, contacts) with per-schema inheritance policy and validation (p2).
- Tenant mode conversion: any post-creation managed/self-managed transition uses the same dual-consent `ConversionRequest` flow in either direction, with inbound conversion request discovery for parent admins (p3).

### 4.2 Out of Scope

- User self-registration: users are provisioned via API within tenant security context (invite model only).
- User authentication flows: covered by IAM PRD.
- Tenant context propagation (SecurityContext population, cross-tenant rejection, service-to-service forwarding): framework and AuthZ Resolver responsibility.
- Barrier-aware tenant tree traversal (ancestor chains, descendant queries with `BarrierMode`): Tenant Resolver Plugin responsibility. AM owns the underlying `tenants` + `tenant_closure` storage and the canonical closure shape, and exposes it to the Plugin via a read-only database role; barrier-aware SDK queries are served by the Plugin over that shared storage. AM's own public API exposes tenant CRUD and direct-children discovery, not barrier-aware subtree traversal.
- AuthZ Resolver (PDP) implementation: covered by Gears DESIGN; this gear covers the tenant model consumed by PDP.
- Resource provisioning and lifecycle for non-identity platform resources: outside AM scope. AM provides tenant context and ownership boundaries but does not manage downstream resource CRUD or provisioning workflows.
- Tenant lifecycle events (CloudEvents): deferred until EVT (Events and Audit Bus) is introduced.
- Service-account credential storage, secret distribution, and workload injection: AM discloses a secret once and keeps nothing; long-term custody belongs to the caller and the platform secret-management plane.
- Retrieval of an existing service-account secret, account update, enable/disable, and paginated or filtered account listing: not exposed in v1 (see §5.5).
- Automatic reconciliation of an ambiguous IdP outcome (idempotency keys, operation keys, retry workers): AM surfaces the ambiguity and the caller reconciles by matching its submitted name; holding reconciliation state would contradict the no-local-storage constraint.
- Service-account name syntax, scope-allowlist membership, and per-tenant quota: enforced by the registered IdP adapter, which owns client-id derivation and its own limits. AM applies only boundary caps on payload size.

### 4.3 Deferred Detailed Coverage

Sections 1–5 scope the product and enumerate functional requirements at contract granularity. The following topics are intentionally treated in later sections of this PRD rather than inlined above, so reviewers can distinguish deliberate deferral from accidental omission:

| Topic | Covered in | Rationale for deferral |
|-------|-----------|------------------------|
| Non-functional thresholds (latency, audit completeness, data classification, reliability, data lifecycle, production scale) | §6 | Contract obligations separated from functional scope so they can be reviewed as a coherent NFR set. |
| Public library and integration interfaces (SDK surface, IdP contract, downstream consumer interfaces) | §7 | Interface shape depends on FRs + NFRs together; kept downstream of both. |
| Use-case walkthroughs (bootstrap, tenant lifecycle, mode conversion, user groups, IdP ops, metadata) | §8 | FRs define *what*, use cases illustrate *how the actors compose them*; kept separate to avoid FR-level narrative drift. |
| Acceptance criteria (review-ready GA gates) | §9 | Aggregated from FR + NFR + use-case claims; lives after all three. |
| External dependencies and consumer matrix | §10 | Enumerated once, after actors and FRs are settled. |
| Assumptions (IdP bootstrap, GTS availability, RG ownership graph) | §11 | Global to the PRD; not scoped per FR. |
| Risks and mitigations | §12 | Cross-cutting; scored against the whole scope rather than per section. |
| Review baseline decisions (approved deployment profile, barrier taxonomy, authoritative artifacts) | §13 | Point-in-time freeze for the v1 review; intentionally isolated so review baseline edits are auditable. |
| Open questions requiring follow-up resolution | §14 | Unresolved items with owner/disposition; not FRs or NFRs. |
| Traceability links (upstream requirements, downstream artifacts, canonical platform references) | §15 | Structural index; lives at the tail by convention. |

Integration test strategy, per-endpoint SLA tables, implementation-level DECOMPOSITION/FEATURE traceability, and CloudEvents for metadata/lifecycle mutations are **not** in this PRD at all — they are explicitly out of scope for this review-ready baseline (see §1.4 Non-goals and §15 Traceability).

## 5. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e) targeting 90%+ code coverage unless otherwise specified. Document verification method only for non-test approaches (analysis, inspection, demonstration).

### 5.1 Platform Bootstrap

#### Root Tenant Auto-Creation

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-root-tenant-creation`

**Actors**: `cpt-cf-account-management-actor-platform-admin`, `cpt-cf-account-management-actor-idp`

The system **MUST** automatically create the initial root tenant when the AM service starts for the first time during platform installation and complete bootstrap with the root tenant visible in status `active`. The root tenant type is determined by deployment configuration (typically the top-level type in the GTS tenant type hierarchy, e.g., `provider` or `root`).

- **Rationale**: The root tenant is the foundation of the tenant tree; without it, no other tenants or operations can exist.

#### Root Tenant IdP Linking

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-root-tenant-idp-link`

**Actors**: `cpt-cf-account-management-actor-platform-admin`, `cpt-cf-account-management-actor-idp`

During bootstrap, the system **MUST** invoke the tenant-provisioning operation for the root tenant — the same IdP integration contract used for every tenant creation — forwarding deployer-configured metadata so the IdP provider plugin can establish the tenant-to-IdP binding. The provider determines the appropriate action: adopting an existing IdP context (e.g., Keycloak master realm), creating a new one, or any other provider-specific behavior. If the provider returns provisioning metadata, AM persists it as tenant metadata; if the provider returns no metadata (binding established through external configuration or convention), AM proceeds normally. AM does not require identifier equality between its tenant UUID and the IdP's internal identifiers, nor does it validate binding sufficiency — that is the provider's responsibility. The initial Platform Administrator user identity is pre-provisioned in the IdP during infrastructure setup; AM does not create this user.

- **Rationale**: AM's obligation is to invoke the IdP contract at the right lifecycle moment and persist whatever the provider returns. Whether the binding is established through returned metadata, external IdP configuration, or convention is deployment-specific and provider-owned. AM owns the tenant model, not user identities — the IdP is the source of truth for admin credentials and authentication. The metadata pass-through keeps AM IdP-agnostic while giving the provider plugin enough context to determine deployment-specific behavior.

#### Bootstrap Idempotency

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-bootstrap-idempotency`

**Actors**: `cpt-cf-account-management-actor-platform-admin`

The system **MUST** detect an existing root tenant during platform upgrade or AM restart and preserve it without duplication; bootstrap **MUST** be a no-op when the root tenant already exists.

- **Rationale**: Platform upgrades and service restarts must not corrupt the tenant tree by creating a duplicate root tenant.

#### Bootstrap Ordering

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-bootstrap-ordering`

**Actors**: `cpt-cf-account-management-actor-platform-admin`, `cpt-cf-account-management-actor-idp`

The system **MUST** wait for the IdP to be available before completing bootstrap, retrying with backoff, and failing after a configurable timeout if the IdP is not ready.

- **Rationale**: The root tenant cannot be fully operational without its associated IdP; proceeding without it would leave the platform in an inconsistent state.

### 5.2 Tenant Hierarchy Management

**Cross-cutting: Concurrency semantics** — Hierarchy-mutating operations (create, delete, status change, mode conversion) on overlapping tenant scopes **MUST** produce deterministic, serializable outcomes. Concurrent mutations on the same tenant **MUST** resolve without data corruption; conflicting operations **MUST** fail with the appropriate deterministic error category rather than producing partial state.

#### Create Child Tenant

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-create-child-tenant`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** allow an authenticated parent tenant administrator to create a new child tenant with a parent reference, establishing the parent-child relationship immediately and making the tenant visible in status `active` once creation completes successfully.

- **Rationale**: Creating sub-tenants is the core operation that builds the organizational hierarchy for all tenant models.

#### Tenant Hierarchy Depth Limit

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-hierarchy-depth-limit`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** support a configurable hierarchy depth advisory threshold (default: 10 levels). When a child tenant creation would exceed the threshold, the system **MUST** emit an operator-visible warning signal and **MUST NOT** reject the operation. In v1, this warning signal MUST be observable by operators via platform monitoring infrastructure (metric increment and structured log entry); it is not a tenant lifecycle CloudEvent. Operators **MUST** be able to configure a strict mode that rejects creation above the threshold with a `tenant_depth_exceeded` error. The platform data model supports unlimited depth per the platform tenant model specification, but production support beyond the approved deployment profile **MUST** be treated as unsupported until representative performance benchmarks exist for the claimed hierarchy depth and data-volume envelope.

- **Rationale**: The platform architecture defines hierarchy depth as unlimited, but deep hierarchies impact query performance and operational complexity in AM and dependent consumers. A configurable advisory threshold provides operational visibility while preserving flexibility; strict mode is opt-in for deployments that need a hard cap, and benchmark-backed profiles define what is actually supported in production.

#### Tenant Status Change

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-tenant-status-change`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** allow an administrator to change a tenant's status between `active` and `suspended` and **MUST NOT** cascade suspension to child tenants; child tenants **MUST** remain active and fully operational when a parent is suspended. Transitioning to `deleted` is not permitted via status change — deletion **MUST** go through the dedicated soft-delete operation (`cpt-cf-account-management-fr-tenant-soft-delete`) which enforces child/resource-ownership preconditions.

**Operations on a suspended tenant:**

- Child tenant creation under a suspended parent **MUST** be rejected with a `validation` error.
- User provisioning within a suspended tenant **MUST** be rejected with a `validation` error.
- Metadata writes to a suspended tenant **MUST** be rejected with a `validation` error.
- Read operations (tenant details, children query, metadata resolution, user query) **MUST** remain available.
- Status change to `active` (unsuspend) and soft-delete **MUST** remain available.
- Mode conversion initiation from a suspended tenant **MUST** be rejected with a `validation` error.

- **Rationale**: Cascading suspension would disrupt downstream tenants (e.g., suspending a parent must not suspend its children). Each tenant's operational state must be independently controllable. Separating deletion from status updates ensures the child/resource-ownership guards cannot be bypassed. Blocking mutating operations on suspended tenants prevents inconsistent state while preserving read access and the ability to unsuspend or delete.

#### Tenant Soft Delete

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-tenant-soft-delete`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

The system **MUST** allow deletion of a **non-root** tenant only when it has no non-deleted child tenants and no remaining tenant-owned resource associations in the Resource Group ownership graph, transitioning the tenant to `deleted` status (soft delete). Attempts to delete the root tenant **MUST** be rejected as `InvalidArgument` (HTTP 400) with `reason=ROOT_TENANT_CANNOT_DELETE`. If non-deleted children exist, deletion **MUST** be rejected as `FailedPrecondition` (HTTP 400) with `reason=TENANT_HAS_CHILDREN`. If resource associations remain under the tenant's ownership scope, deletion **MUST** be rejected as `FailedPrecondition` (HTTP 400) with `reason=TENANT_HAS_RESOURCES`. Hard deletion **MUST** occur after a configurable retention period (default: 90 days). The hard-deletion process **MUST NOT** leave orphaned child tenant records. When a parent and child tenant share the same retention window, the hard-deletion background job **MUST** process leaf tenants before their parents (leaf-first ordering).

- **Rationale**: Preventing deletion of tenants with active children or remaining resource ownership links protects organizational integrity and prevents orphaned ownership mappings. The root tenant is undeletable so the deployment always retains exactly one hierarchy root. Soft delete with retention enables recovery and compliance. Ensuring no orphaned child records prevents referential integrity violations during retention cleanup.

#### Children Query with Pagination

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-children-query`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** return direct children of a given tenant with pagination support and optional status filtering.

- **Rationale**: Tenant administrators need a predictable way to browse and manage immediate children; deeper barrier-aware traversal is handled by Tenant Resolver.

#### Read Tenant Details

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-tenant-read`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

The system **MUST** return tenant details for a requested tenant identifier when the caller is authorized for that tenant scope. The response **MUST** include the tenant's identifier, parent reference, type, status, mode, and the timestamps required for administrative workflows and auditing.

- **Rationale**: Administrators need a reliable way to inspect current tenant state before making lifecycle, support, billing, and policy decisions.

#### Update Tenant Mutable Fields

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-tenant-update`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

The system **MUST** allow an authorized administrator to update only mutable tenant attributes through the general update operation: `name` and `status` (limited to `active` ↔ `suspended` transitions; `deleted` is handled exclusively by the soft-delete operation). The system **MUST** reject attempts to modify immutable hierarchy-defining fields such as `id`, `parent_id`, `tenant_type`, `self_managed`, and `depth`; mode changes remain handled by the dedicated conversion flow.

- **Rationale**: Administrative workflows require controlled edits to tenant presentation and lifecycle state without allowing accidental hierarchy or mode mutations through a generic update path. Restricting the update path to non-terminal status transitions ensures deletion guards (child/resource checks) cannot be bypassed.

#### Transitive Ancestry Storage

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-tenant-closure`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-tenant-resolver`, `cpt-cf-account-management-actor-authz-resolver`

The system **MUST** own a `tenant_closure` table with the platform-canonical shape `(ancestor_id, descendant_id, barrier, descendant_status)` defined in TENANT_MODEL.md. Closure rows **MUST** exist only for tenants whose `tenants.status` is SDK-visible (`active`, `suspended`, `deleted`): each such tenant has a self-row `(id, id)` plus one row per strict ancestor along the `parent_id` chain. Tenants in the transient `provisioning` state **MUST NOT** have any closure rows — they are inserted in a single transaction with the `provisioning → active` status transition, and removed in a single transaction with hard-deletion. Every status transition between SDK-visible states **MUST** update `descendant_status` transactionally for all rows where that tenant is `descendant_id`. The `barrier` column (`SMALLINT`, v1 uses bit 0 for self_managed per TENANT_MODEL.md) reflects whether any tenant on the path `(ancestor, descendant]` is `self_managed` (ancestor excluded, descendant included); self-rows `(id, id)` **MUST** carry `barrier = 0`. The `descendant_status` column domain is `{active, suspended, deleted}` only — the internal `provisioning` state is excluded by construction. The system **MUST** expose this storage to the Tenant Resolver Plugin through a read-only database role scoped to `tenants` and `tenant_closure`, and **MUST NOT** provide any sync, projection, revision-token, or change-feed capability on top of that storage.

- **Rationale**: Barrier-aware subtree, ancestor, and existence queries on the authorization hot path need a transitive representation of the tenant tree. Co-owning the canonical closure table inside AM, and co-maintaining it transactionally with tenant writes, is what allows the Tenant Resolver Plugin to be a stateless query facade with no sync pipeline, no projection freshness budget, and no drift-detection loop — barrier changes and hierarchy changes become visible to the Plugin atomically the moment AM commits them. Keeping `provisioning` tenants out of `tenant_closure` entirely (rather than carrying them as a filtered state) makes the closure a clean publication contract for future replication to business gears — replica consumers never observe transient AM saga state and have no `provisioning`-specific filtering obligation.

### 5.3 Tenant Type Enforcement

#### Tenant Type Validation via GTS

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-tenant-type-enforcement`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-gts-registry`

The system **MUST** enforce parent-child type constraints at creation time using the GTS types registry. Each tenant type defines `allowed_parent_types`. The system **MUST** reject creation when the child type is not permitted under the parent's type.

- **Rationale**: Enforcing type-based parent constraints ensures the business hierarchy remains well-formed and prevents invalid organizational structures.

Type topology is deployment-specific. Examples:

| Deployment model | Tenant types | Type rules |
|------------------|-------------|------------|
| **Flat** | `tenant` | `allowed_parent_types: []` — root type configured at bootstrap, no nesting |
| **Cloud hosting** | `provider`, `reseller`, `customer` | provider can parent reseller and customer; reseller can parent customer; customer is leaf |
| **Education** | `consortium`, `university`, `college` | consortium is root; university under consortium; college under university |
| **Enterprise** | `hq`, `region`, `unit` | hq is root; region under hq; unit under region |

#### Tenant Type Nesting

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-tenant-type-nesting`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** allow same-type nesting when the GTS type definition permits it (e.g., `region` under `region` for multi-level structures) while maintaining an acyclic hierarchy.

- **Rationale**: Some real tenant topologies require repeated organizational tiers, and forbidding valid same-type nesting would force artificial type taxonomies.

### 5.4 Managed/Self-Managed Tenant Modes

#### Managed Tenant Creation

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-managed-tenant-creation`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** allow a child tenant to be created directly in managed mode, establishing no visibility barrier between parent and child and leaving delegated parent access eligible for downstream policy evaluation.

- **Rationale**: The managed tenant model enables delegated administration where parent tenants directly manage child tenant environments.

#### Self-Managed Tenant Creation

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-self-managed-tenant-creation`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** allow a child tenant to be created directly in self-managed mode, establishing a visibility barrier so the parent has no default access to the child's APIs or resources.

- **Rationale**: The self-managed model enables autonomous operation where the child tenant operates independently with full isolation from the parent.

#### Mode Conversion Approval

- [ ] `p3` - **ID**: `cpt-cf-account-management-fr-mode-conversion-approval`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

Any post-creation change between managed and self-managed mode **MUST** use a durable dual-consent `ConversionRequest`: one side initiates, the counterparty approves, and each side acts only from its own tenant scope. Root tenants **MUST NOT** be convertible.

- **Rationale**: Both directions change the trust boundary between parent and child, so both directions need bilateral consent.

#### Mode Conversion Expiry

- [ ] `p3` - **ID**: `cpt-cf-account-management-fr-mode-conversion-expiry`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

Pending conversion requests **MUST** expire after the configured approval window if they remain unresolved, and expiry **MUST NOT** change the tenant's current mode.

- **Rationale**: Expiry prevents indefinite outstanding trust-boundary changes.

#### Mode Conversion Single Pending Invariant

- [ ] `p3` - **ID**: `cpt-cf-account-management-fr-mode-conversion-single-pending`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

At most one pending conversion request **MUST** exist for a tenant at any time.

- **Rationale**: Competing pending requests create ambiguous operator intent and unclear approval ownership.

#### Mode Conversion Consistent Apply

- [ ] `p3` - **ID**: `cpt-cf-account-management-fr-mode-conversion-consistent-apply`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

Approving a conversion request **MUST** produce a consistent outcome in which the request resolution and the tenant mode change become visible together, or neither does.

- **Rationale**: Review and enforcement cannot tolerate a state where approval history and barrier state disagree.

#### Creation-Time Self-Managed Declaration

- [ ] `p3` - **ID**: `cpt-cf-account-management-fr-conversion-creation-time-self-managed`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

Creating a child tenant directly in self-managed mode **MUST NOT** require a conversion request. Only post-creation mode changes require the dual-consent flow.

- **Rationale**: A child tenant must not become self-managed behind the parent's back at any moment. At creation time the parent is the sole party with admin authority over the new tenant; requiring a separate `ConversionRequest` would be ceremonial overhead with no protective value.

#### Inbound Conversion Requests Query

- [ ] `p3` - **ID**: `cpt-cf-account-management-fr-child-conversions-query`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** allow a parent tenant administrator to discover conversion requests targeting direct children, with filtering and pagination, while exposing only minimal conversion-request metadata and not the child's full tenant record.

- **Rationale**: Without a dedicated query endpoint, the parent admin has no way to discover pending conversion requests from self-managed children — the barrier blocks normal child visibility, creating a functional gap in the dual-consent flow. Exposing only conversion-request metadata (not child tenant data) preserves barrier semantics while enabling the approval workflow.

#### Conversion Request Cancellation by Initiator

- [ ] `p3` - **ID**: `cpt-cf-account-management-fr-conversion-cancel`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** allow only the initiator of a pending conversion request to withdraw it. Withdrawal **MUST** transition the request to `cancelled` and **MUST NOT** alter the tenant's current mode.

- **Rationale**: Without an explicit withdraw path, the initiator's only way to dismiss their own mistaken or outdated request is to wait for the approval window to expire. The semantic "I changed my mind" is distinct from "they declined" and is therefore expressed as a distinct state (`cancelled`) rather than collapsed with counterparty rejection.

#### Conversion Request Rejection by Counterparty

- [ ] `p3` - **ID**: `cpt-cf-account-management-fr-conversion-reject`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

The system **MUST** allow only the counterparty of a pending conversion request to decline it. Rejection **MUST** transition the request to `rejected` and **MUST NOT** alter the tenant's current mode.

- **Rationale**: Counterparty rejection is semantically distinct from initiator withdrawal — the former conveys active disagreement with the proposed mode change, the latter conveys that the initiator no longer wants it. Keeping them as distinct terminal states enables precise UX ("your request was rejected by the parent" vs. "you withdrew this request"), attribution of reject/cancel rates in metrics, and policy hooks that may later depend on which side resolved the request.

#### Resolved Conversion Request Retention

- [ ] `p3` - **ID**: `cpt-cf-account-management-fr-conversion-retention`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

Resolved conversion requests (`approved`, `cancelled`, `rejected`, `expired`) **MUST** remain queryable for a configurable review window and then leave the default API surface while remaining available to retained audit/history flows.

- **Rationale**: Historical resolutions carry audit and UX value — "my last conversion attempt was rejected two weeks ago" must remain answerable without forcing unbounded storage growth. A configurable retention window gives operators a single knob to balance audit needs, operational clarity, and storage cost.

Pending `ConversionRequest` rows transition to `expired` by an AM background reaper that scans for rows whose `expires_at` has passed. The reaper runs on a `cleanup_interval` cadence (default 60s). Expired rows remain queryable for `resolved_retention` (default 30 days) before the retention job stamps `deleted_at` and default API reads exclude them. Hard-delete follows AM's platform retention cadence. See `cpt-cf-account-management-fr-mode-conversion-expiry` and `cpt-cf-account-management-fr-conversion-retention`.

### 5.5 IdP Tenant & User Operations Contract

AM uses an outbound IdP integration contract. The public AM contract defines when those operations must occur and which failure classes are visible to consumers; the provider-specific transport and payload details belong to the IdP contract and the OpenAPI artifact.

#### Tenant IdP Provisioning

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-idp-tenant-provision`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`, `cpt-cf-account-management-actor-idp`

The system **MUST** invoke the IdP tenant-provisioning operation after every successful tenant creation, including bootstrap creation of the root tenant, and **MUST** pass the tenant identity plus deployment-supplied provisioning context required by the provider.

- **Rationale**: Providers need a lifecycle hook to establish tenant-scoped identity resources without making AM provider-specific.

#### Tenant IdP Provisioning Failure Contract

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-idp-tenant-provision-failure`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`, `cpt-cf-account-management-actor-idp`

The system **MUST** surface tenant-provisioning failures through deterministic AM error categories. When AM can prove that no IdP-side tenant state was retained, the failure **MUST** be exposed as a clean retryable failure. When the external outcome may already have been retained, the failure **MUST** be exposed as requiring reconciliation before retry rather than inviting blind automatic retry.

- **Rationale**: Tenant creation crosses a trust boundary into an external identity system and therefore needs an explicit reconciliation contract.

#### Tenant IdP Deprovisioning

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-idp-tenant-deprovision`

**Actors**: `cpt-cf-account-management-actor-platform-admin`, `cpt-cf-account-management-actor-idp`

The system **MUST** invoke tenant deprovisioning during hard deletion so the provider can clean up tenant-scoped identity resources before AM removes the tenant permanently.

- **Rationale**: Symmetric to tenant provisioning — providers that create per-tenant IdP resources need a lifecycle hook to clean them up. Running at hard-deletion time (not soft-delete) ensures the IdP resources remain available during the retention window in case the tenant is restored.

#### User Provisioning

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-idp-user-provision`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-idp`

The system **MUST** provision users within a tenant scope through the IdP integration contract and bind the resulting user identity to that tenant.

- **Rationale**: Tenant-scoped provisioning is the primary administrative bridge between AM and a pluggable IdP implementation.

#### User Deprovisioning

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-idp-user-deprovision`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-idp`

The system **MUST** deprovision users through the IdP integration contract and treat an already-absent IdP user as a successful no-op.

- **Rationale**: Deprovisioning through the shared contract keeps AM intent and IdP identity state aligned while closing access promptly.

#### User Update

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-idp-user-update`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-idp`

The system **MUST** update a tenant user's mutable attributes (`username`, `email`, `display_name`, `first_name`, `last_name`, `password`) through the IdP integration contract as a partial update (JSON Merge Patch), persisting no local user state. An update against a user the IdP reports absent **MUST** surface as `not_found` (not folded into success); the tenant-user binding attribute is out of scope (user reassignment remains a v1 non-goal).

- **Rationale**: Attribute update is the missing verb in the AM-mediated user lifecycle. Routing it through the shared contract keeps edits inside AM's tenant-scoped authorization, GTS validation, and audit envelope — the same posture as create and delete — while the IdP stays the single source of truth and AM persists no local user state.

#### User Tenant Query

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-idp-user-query`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-idp`

The system **MUST** support tenant-scoped user listing and point-existence checks through the IdP integration contract.

- **Rationale**: Tenant-scoped user queries are required for administration and support. User-ID filtering enables callers to verify user existence before adding them to a Resource Group.

#### Service Account Provisioning

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-service-account-provision`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-machine-workload`, `cpt-cf-account-management-actor-idp`

The system **MUST** provision, through the IdP integration contract, a confidential `client_credentials` service account owned by an explicit target tenant, and **MUST** disclose exactly once everything the workload needs to authenticate: the client identifier, the plaintext client secret, the token endpoint, and the subject identifier. The credential-bearing response **MUST NOT** be cached or stored by intermediaries. The system **MUST** reject a request whose name is already live in the target tenant and **MUST NOT** resume, reveal, or modify the existing account, so a tenant and a name together identify at most one live account.

- **Rationale**: Workloads need dedicated non-human credentials rather than borrowed human accounts, and a name that could match more than one live account would leave a caller recovering from an uncertain outcome unable to tell which identity its request produced.

#### Service Account Discovery

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-service-account-list`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-idp`

The system **MUST** list a target tenant's service accounts through the IdP integration contract, identifying each by its client identifier and by the name its creator supplied, reported unchanged. The listing **MUST NOT** carry a client secret in any field.

- **Rationale**: Administrators need inventory for audit, rotation, and revocation, and the verbatim name is the only provider-independent way to recognise an account when the identity provider assigns client identifiers the caller cannot predict. Inventory access must not become credential access.

#### Service Account Secret Rotation

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-service-account-rotate`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-machine-workload`, `cpt-cf-account-management-actor-idp`

The system **MUST** rotate a tenant-owned account's secret without changing its identity, disclosing the new secret exactly once in a response that **MUST NOT** be cached or stored by intermediaries. An address that does not resolve within the target tenant **MUST** surface as `not_found` rather than succeeding, and **MUST NOT** disclose whether that identifier exists under a different tenant.

- **Rationale**: Workloads need regular and incident-driven credential replacement without replacing their identity, and rotation must never mint a credential for a foreign or nonexistent one. Rotation is also the recovery path both for a lost secret and for an account left behind by an uncertain provisioning outcome.

#### Service Account Revocation

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-service-account-revoke`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-machine-workload`, `cpt-cf-account-management-actor-idp`

The system **MUST** revoke a tenant-owned service account through the IdP integration contract and **MUST** treat revocation of an already-absent account as indistinguishable from revocation of a present one, so the operation is idempotent and retry-safe. That indistinguishability **MUST** also cover an address owned by a different tenant, so revocation never becomes a probe for accounts owned elsewhere. A clean provider failure or an ambiguous outcome **MUST NOT** be reported as a successful revocation.

- **Rationale**: Administrators need an immediate, retry-safe way to terminate workload access, and reporting absence identically in every case denies a caller any signal about other tenants' accounts.

#### Service Account Secret Confidentiality

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-service-account-secret-confidentiality`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-idp`

The system **MUST NOT** expose any path that reads back an existing plaintext client secret, and **MUST NOT** persist one. Because a provider adapter may place a credential in the text it returns with a failure, the system **MUST NOT** surface that text — to a caller or to its own logs — in any form, whether whole, filtered, shortened, or digested; each failure category **MUST** be answered with a fixed message the system owns, and the record kept of the discarded text **MUST** be confined to its category, its length, and whether a field was attributed.

- **Rationale**: A leaked client secret grants workload identity until rotation or revocation. A credential embedded in provider text is indistinguishable from ordinary prose — `secret=abc123` is plain ASCII — so no filter or length cap can make relaying that text safe; only discarding it can. This is deliberately stricter than the tenant and user halves of the same contract, which forward a non-reversible digest for operator correlation, because a credential-minting call is where a vendor error is most likely to quote the secret it just issued.

### 5.6 User Groups Management

User groups are implemented as [Resource Group](../../resource-group/docs/PRD.md) entities. AM ensures the required Resource Group type exists and coordinates cleanup at tenant retirement, but consumers use Resource Group directly for group hierarchy and membership operations.

#### User Group Resource Group Type Registration

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-user-group-rg-type`

**Actors**: `cpt-cf-account-management-actor-platform-admin`

AM **MUST** register (or require via seeding) the chained Resource Group type schema `gts.cf.core.rg.type.v1~cf.core.am.user_group.v1~` for user groups. Its `allowed_memberships` **MUST** include the platform user resource type `gts.cf.core.am.user.v1~`, and its `allowed_parents` **MUST** include itself to support nested groups. Tenant-scoped placement is enforced by Resource Group's ownership-graph rules rather than encoded as a schema trait. Registration happens during AM gear initialization.

- **Rationale**: A dedicated Resource Group type ensures user group operations are governed by the same typed hierarchy, forest invariants, and tenant isolation rules as all other Resource Group entities, without reimplementing group infrastructure in AM.

#### User Group Lifecycle via Resource Group

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-user-group-lifecycle`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** allow a tenant administrator to create, update, and delete user groups within their tenant scope by using Resource Group directly. Group-identity uniqueness and hierarchy integrity are enforced by Resource Group; AM does not proxy those CRUD operations.

- **Rationale**: Resource Group already provides typed hierarchy, tenant scoping, and forest invariants. A separate AM proxy layer would add no domain logic beyond pass-through. AM's tenant-scoped user-query capability provides the valid user set that callers combine with Resource Group membership operations.

#### User Group Membership via Resource Group

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-user-group-membership`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** allow a tenant administrator to add and remove users from groups through Resource Group membership operations while using AM's user-query capability to verify that referenced users exist.

- **Rationale**: Administrators need direct control of membership; Resource Group's existing membership contract (composite key, tenant-scoping, conflict detection) satisfies this without duplication. User existence validation is a caller responsibility, not a structural invariant.

#### Nested User Groups

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-nested-user-groups`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** support nested user groups through Resource Group's parent-child hierarchy, with cycle prevention delegated to Resource Group.

- **Rationale**: Resource Group already provides strict forest enforcement (single parent, no cycles) and closure-table traversal — reusing it avoids duplicating cycle detection and hierarchy query logic.

### 5.7 Extensible Tenant Metadata

#### Tenant Metadata Schema Registration

- [ ] `p2` - **ID**: `cpt-cf-account-management-fr-tenant-metadata-schema`

**Actors**: `cpt-cf-account-management-actor-platform-admin`, `cpt-cf-account-management-actor-gts-registry`

The system **MUST** support extensible tenant metadata through GTS-registered schemas. Each schema defines validation rules and an inheritance policy, and new metadata categories **MUST** be introducible without AM code changes.

- **Rationale**: A generic metadata mechanism avoids feature-specific APIs for each new tenant data category. Branding, company contacts, billing addresses, and future tenant attributes all share the same storage, validation, and inheritance contract.

#### Tenant Metadata CRUD

- [ ] `p2` - **ID**: `cpt-cf-account-management-fr-tenant-metadata-crud`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** allow tenant-scoped create, read, update, and delete of metadata entries for registered schemas, with writes validated against the selected schema.

- **Rationale**: Tenant administrators need self-service control over tenant-specific data without platform-level intervention for each new metadata schema.

#### Tenant Metadata Resolution API

- [ ] `p2` - **ID**: `cpt-cf-account-management-fr-tenant-metadata-api`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** provide effective-value resolution for tenant metadata based on the schema's inheritance policy. For inheritable schemas, resolution **MUST** stop at self-managed boundaries.

- **Rationale**: A single resolution API with a per-schema `inheritance_policy` trait gives consumers one consistent contract instead of re-implementing resolution per metadata schema. Stopping inheritance at self-managed boundaries preserves the core isolation invariant — a self-managed tenant is fully independent, including its metadata — without requiring `BarrierMode::Ignore` for metadata operations.

#### Tenant Metadata Listing

- [ ] `p2` - **ID**: `cpt-cf-account-management-fr-tenant-metadata-list`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`

The system **MUST** expose a listing capability for metadata entries written directly on a tenant, separate from effective-value resolution, and that capability **MUST** respect the same tenant/barrier visibility model as other AM reads.

- **Rationale**: Tenant administration UIs and automation need to discover which metadata schemas a tenant has populated without probing schemas one by one. Discovery is required before any editor, export, or diagnostics surface can be built, and it is the minimal complement to direct per-schema metadata management.

#### Per-Schema Metadata Permissions

- [ ] `p2` - **ID**: `cpt-cf-account-management-fr-tenant-metadata-permissions`

**Actors**: `cpt-cf-account-management-actor-platform-admin`, `cpt-cf-account-management-actor-tenant-admin`

The metadata permission model **MUST** allow authorization policies to restrict access by `schema_id`, not only by tenant.

- **Rationale**: Real deployments separate duties between functions (marketing, finance, operations, legal) and storing all these categories behind a single coarse "tenant metadata" action would force ops teams to grant over-broad access. Carrying `schema_id` into the AuthZ request keeps AM's own contract simple while letting policy authors scope permissions to the categories they own.

### 5.8 Deterministic Error Semantics

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-deterministic-errors`

**Actors**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

The gear **MUST** map every failure to one of the AIP-193 canonical
categories surfaced by [`toolkit-canonical-errors`](../../../../libs/toolkit-canonical-errors/).
AM does not maintain a private error taxonomy — the SDK re-exports
`CanonicalError` (as `AccountManagementError`) verbatim, and the HTTP
status of every response is a property of the canonical category.

Categories AM uses today:

- `InvalidArgument` (HTTP 400) — input or precondition-on-input violations (`VALIDATION`, `ROOT_TENANT_CANNOT_DELETE`, `ROOT_TENANT_CANNOT_CONVERT`; `INVALID_TENANT_TYPE` is emitted unconditionally by the bootstrap-owned root-type preflight, and additionally by the **tenant-type checker path** (`GtsTenantTypeChecker` invoked on child create / mode conversion) only when `strict_barriers = true` and the UUID-keyed Types Registry lookup pipeline lands — see DESIGN §3.1, "Enforcement scope (PR1 vs strict mode)")
- `NotFound` (HTTP 404) — tenant, conversion request, metadata schema, or metadata entry missing
- `FailedPrecondition` (HTTP 400) — state precondition violations (`TENANT_HAS_CHILDREN`, `TENANT_HAS_RESOURCES`, `TENANT_DEPTH_EXCEEDED`, `PENDING_EXISTS`, `INVALID_ACTOR_FOR_TRANSITION`, `ALREADY_RESOLVED`; `TYPE_NOT_ALLOWED` is emitted unconditionally by the bootstrap-owned root-type preflight, and additionally by the tenant-type checker path on child create / mode conversion only when `strict_barriers = true` — outside that gate the per-pair compatibility check stub-admits)
- `PermissionDenied` (HTTP 403) — PDP-denied cross-tenant access (also fail-closed for unreachable PDP / unsupported constraint-compile per the ToolKit AuthZ invariant), non-platform-admin attempting root-tenant-scoped operations
- `Aborted` (HTTP 409) — concurrency conflict (currently only retry-exhausted SERIALIZABLE failures, with `reason = "SERIALIZATION_CONFLICT"`)
- `AlreadyExists` (HTTP 409) — unique-constraint violation on a tenant write
- `ResourceExhausted` (HTTP 429) — integrity-check single-flight refusal: the canonical envelope sets `quota_violations[0].subject = "integrity_check"`; there is no separate public reason field for clients to key off. The hierarchy-integrity check itself is a Phase-2 internal SDK capability (`TenantService::check_hierarchy_integrity()`) reachable today only via the in-process periodic job and the auto-repair tick — DESIGN §3.2 *Diagnostic Capabilities* defines its semantics, the `integrity_check_runs` PK gate, and the single-flight contract; public REST surfacing is tracked together with the `InTenantSubtree` predicate and not yet a v1 functional requirement (`§5` does not list it as a public-FR), so this 429 envelope is reserved for the internal-SDK / admin-tool driven path until that exposure lands.
- `ServiceUnavailable` (HTTP 503) — transient infrastructure outage; covers IdP outages (former `idp_unavailable`), DB outages, AuthZ PDP transport failure, and Types Registry reachability failure during a tenant-type-consulting flow. Carries `retry_after_seconds` when a defensible hint is available.
- `Unimplemented` (HTTP 501) — IdP plugin does not support the requested operation
- `Internal` (HTTP 500) — unclassified internal failure

Rationale: anchoring AM's error contract to the AIP-193 canonical model means clients and operators react consistently across Gears — every gear that adopts `toolkit-canonical-errors` shares the same envelope shape, the same HTTP semantics, and the same `errors[]` discriminator vocabulary. Fine-grained discriminators (`INVALID_TENANT_TYPE`, `TENANT_HAS_CHILDREN`, `PENDING_EXISTS`, `SERIALIZATION_CONFLICT`, …) live inside the canonical envelope as `reason` tokens on field/precondition/quota violations rather than as a private AM-side `code` field.

The authoritative HTTP mapping, the `errors[]` violation vocabulary, and the GTS resource-type tags are documented in [DESIGN §3.8](./DESIGN.md#38-error-codes-reference). Provider-specific diagnostics appear in audit trails or in the audit-only `diagnostic` field without changing the public envelope.

### 5.9 Observability Metrics

#### Domain-Specific Metrics Export

- [ ] `p1` - **ID**: `cpt-cf-account-management-fr-observability-metrics`

**Actors**: `cpt-cf-account-management-actor-platform-admin`

The gear **MUST** export domain-specific metrics for dependency health, metadata resolution, bootstrap lifecycle, tenant-retention work, conversion lifecycle, hierarchy-depth threshold exceedance, and cross-tenant denials. Metric naming and exposure **MUST** follow platform observability conventions.

- **Rationale**: Operators need visibility into AM-specific domain operations (external dependency health, internal sub-operation latencies, security event counts) that platform-level middleware cannot provide. The boundary between platform-provided and gear-internal metrics is an implementation concern owned by the DESIGN.

## 6. Non-Functional Requirements

> **Global baselines**: Project-wide NFRs (performance, security, reliability, scalability) defined in root PRD. Document only gear-specific NFRs here: **exclusions** from defaults or **standalone** requirements.
>
> **Testing strategy**: NFRs verified via automated benchmarks, security scans, and monitoring unless otherwise specified.

### 6.1 Tenant Context Validation Latency

- [ ] `p1` - **ID**: `cpt-cf-account-management-nfr-context-validation-latency`

AM data access and source-of-truth lookups **MUST** enable end-to-end tenant-context validation — including the Tenant Resolver Plugin's indexed read path over `tenants` + `tenant_closure` via the read-only database role and steady-state reverse-hydration of UUID-backed public GTS identifiers — to complete in p95 ≤ 5ms under normal load on the approved deployment profile.

- **Threshold**: 5ms at p95 across the full tenant-context validation path under the approved deployment profile.
- **Rationale**: AM is not the request-path enforcement point, but its source-of-truth query model must not block the platform SLO.

### 6.2 Authentication Context

- [ ] `p1` - **ID**: `cpt-cf-account-management-nfr-authentication-context`

AM API endpoints **MUST** require authenticated requests via platform `SecurityContext`.

- **Inherited ownership**: Token validation, session renewal, federation, credential policy, and MFA policy are inherited from [docs/arch/authorization/DESIGN.md](../../../../docs/arch/authorization/DESIGN.md) and the configured IdP provider.
- **AM-specific addendum**: AM defines only the extra administrative identity requirements unique to this gear, namely tenant-scoped administrative user operations and preservation of tenant identity context in audit records.

### 6.3 Tenant Isolation Integrity

- [ ] `p1` - **ID**: `cpt-cf-account-management-nfr-tenant-isolation`

Tenant A **MUST NOT** be able to access Tenant B data through any API or data access path, verified by automated security tests.

- **Threshold**: Zero cross-tenant data leaks in automated security and integration testing.
- **Verification Method**: Cross-tenant access attempts against both managed and self-managed hierarchies.

### 6.4 Audit Trail Completeness

- [ ] `p1` - **ID**: `cpt-cf-account-management-nfr-audit-completeness`

Every tenant configuration change **MUST** be recorded in the platform append-only audit infrastructure with actor identity, tenant identity, and change details. AM **MUST** emit `actor=system` audit records through that same platform sink for non-request transitions it owns, including bootstrap completion, conversion expiry, provisioning-reaper compensation, and hard-delete / tenant-deprovision cleanup. **v1 deferral**: the platform append-only audit sink (event-bus) is not yet available, so AM-owned non-request transitions emit a structured log on the `am.events` target with `actor=system` as the v1 stand-in for the audit envelope; full sink integration is tracked as a follow-up.

- **Threshold**: 100% of AM-owned state changes recorded; zero known audit gaps.
- **Inherited controls**: Audit retention, tamper resistance, and security-monitoring integration are inherited platform controls documented in [docs/security/SECURITY.md](../../../../docs/security/SECURITY.md). AM must emit the events those platform controls rely on.

### 6.5 Barrier Enforcement

- [ ] `p1` - **ID**: `cpt-cf-account-management-nfr-barrier-enforcement`

AM barrier state and tenant metadata **MUST** be sufficient for Tenant Resolver and AuthZ Resolver to enforce managed and self-managed access controls at the query level. AM **MUST** audit all barrier-state-changing operations it owns (mode conversions, `self_managed` flag writes) per `cpt-cf-account-management-nfr-audit-completeness`. Cross-tenant access auditing (barrier traversals, `BarrierMode::Ignore` usage) is a platform AuthZ concern — AM does not observe or audit downstream enforcement decisions.

- **Threshold**: Zero unauthorized cross-barrier accesses attributable to missing or stale AM source data; 100% of AM-owned barrier transitions audited.

### 6.6 Tenant Model Versatility

- [ ] `p2` - **ID**: `cpt-cf-account-management-nfr-tenant-model-versatility`

The tenant model **MUST** support both managed (no barrier) and self-managed (with barriers) patterns within the same tenant tree, with `BarrierMode` enabling selective barrier bypass for downstream billing and administrative operations.

- **Threshold**: Mixed-mode tenant hierarchies operate correctly under the same deployment profile and authorization model.
- **Rationale**: Supporting both models in one tenant tree is the core product differentiator for AM.

### 6.7 API and SDK Compatibility

- [ ] `p1` - **ID**: `cpt-cf-account-management-nfr-compatibility`

Published REST APIs **MUST** follow path-based versioning. SDK client and IdP integration contracts are stable interfaces — breaking changes **MUST** follow platform versioning policy and require a new contract version with a migration path for consumers.

- **Inherited runtime compatibility**: AM has no gear-specific runtime, OS, or deployment-topology deviations; it inherits standard ToolKit runtime, database, and rolling-upgrade compatibility from [docs/toolkit_unified_system/README.md](../../../../docs/toolkit_unified_system/README.md).
- **Consumer compatibility**: Changes to AM source-of-truth tenant data consumed by Tenant Resolver, AuthZ Resolver, or Billing **MUST** remain backward-compatible within a minor release or publish a coordinated migration path.

### 6.8 Expected Production Scale

- [ ] `p1` - **ID**: `cpt-cf-account-management-nfr-production-scale`

The AM deployment profile is defined authoritatively in §13 *Review Baseline Decisions* and frozen for the v1 review. The dimensions below are the **design targets** the architecture is validated against — not measured production workloads (AM is a new gear; no production AM traffic exists yet). Each bound is anchored to a specific architectural decision in the companion DESIGN, so enlarging any bound requires revisiting that decision rather than a documentation change.

| Dimension | v1 Design Target (from §13) | Anchored architectural decision |
|-----------|-----------------------------|--------------------------------|
| Tenants (hierarchy nodes) | 100K | Closure-table sizing and B-tree index layout on `tenant_closure (ancestor_id, barrier, descendant_status)` (DESIGN §3.1 *Domain Model* / §3.7 *Database schemas & tables*) |
| Typical hierarchy depth | 3–10 levels, with no hard cap; advisory threshold default 10; benchmark-gated validation up to ≥ 15 levels per §1.3 | Closure row count per tenant ≈ depth; recursive structural-validation cost (DESIGN §3.2 `TenantService`) |
| Users (across all tenants) | 300K (IdP-stored) | Out of AM's storage; affects only audit-trail identity references |
| User groups / memberships | 30K user groups, 300K memberships (Resource Group-stored) | Out of AM's storage; sets RG integration expectations only |
| Concurrent API requests (peak) | 1K rps | Per-endpoint latency table in §9 (Acceptance Criteria) and the Tenant Resolver p95 ≤ 5ms hot-path budget |

- **Threshold**: all dimensions locked to §13; changes require an amendment to §13 *and* the anchored architectural decision.
- **Additional throughput targets** (design targets, validated pre-GA):
  - Lifecycle reads sustain the 1K rps peak without violating NFR 6.1.
  - Administrative mutations sustain ≥ 25 writes/second for a 15-minute window under the profile.
  - Background expiry and retention work clear a backlog of 10K due rows within 60 minutes under the profile.
- **Rationale**: stating these as design targets anchored to concrete architectural decisions prevents the envelope from drifting independently of the architecture it was sized against. The wider planning ranges previously shown here (up to 1M users, 10K rps) were not architecture-validated and are removed to avoid implying production-supported limits that were never designed for. Evidence becomes measured only after GA load-test gates (§1.3 success criteria).

### 6.9 Data Classification

- [ ] `p2` - **ID**: `cpt-cf-account-management-nfr-data-classification`

AM persists tenant hierarchy data, tenant metadata, conversion-request state, and IdP-issued UUID identity references only where needed for traceability. User group structure is stored by Resource Group, and credentials/profile data remain owned by the IdP.

- **Classification baseline**:
  - Tenant hierarchy and tenant-mode data: Internal / Confidential.
  - Opaque identity references in audit records: PII-adjacent and platform-protected.
  - Extensible metadata: classification defined per registered GTS schema.
  - Service-account client secrets: restricted authentication credentials. Never persisted, wrapped in a redacting, zeroize-on-drop, non-serializable type in process, and disclosed only through the two responses that mint them.
  - Service-account client identifiers, subject identifiers, tenant ownership, and scopes: security-sensitive control-plane metadata, distinct from and never mixed with human profile attributes.
  - Text an IdP adapter supplies with a service-account failure: untrusted data of unknown classification — AM cannot inspect it to establish one — and therefore handled as the most sensitive class: discarded, with only its category, length, and whether a field was attributed retained.
- **Inherited legal/privacy ownership**: Data residency, DSAR orchestration, retention-policy administration, and privacy-by-default controls are inherited platform obligations. AM-specific obligations are to minimize persisted identity data, avoid storing credentials/profile data, and treat IdP-linked payloads as transient administrative data.
- **Threshold**: 100% of AM-owned persisted data categories mapped to a classification level; zero authentication credentials or IdP profile PII stored by AM outside the platform audit infrastructure; zero plaintext client secrets in debug-formatted output or in any serialized representation other than the two credential-bearing responses; zero occurrences of provider-supplied service-account failure text in any response or log record.

### 6.10 Reliability

- [ ] `p1` - **ID**: `cpt-cf-account-management-nfr-reliability`

AM inherits the platform core infrastructure SLA (target: 99.9% uptime). During IdP outages, AM **MUST** continue serving tenant reads, child listing, status reads, and metadata resolution from AM-owned data while failing only the IdP-dependent operations. Tenant creation remains intentionally non-idempotent across ambiguous external failures. Platform recovery targets remain RPO ≤ 1 hour and RTO ≤ 15 minutes.

### 6.11 Data Lifecycle

- [ ] `p1` - **ID**: `cpt-cf-account-management-nfr-data-lifecycle`

Tenant deprovisioning **MUST** remove tenant-scoped metadata, trigger Resource Group cleanup for tenant-scoped user groups, and invoke IdP tenant deprovisioning before final hard deletion. Soft-deleted tenants are hard-deleted after the configured retention period.

- **Threshold**: 100% of AM-owned tenant-scoped metadata removed during deprovisioning; tenant hard deletion occurs after the configured retention period.

### 6.12 Data Quality

- [ ] `p2` - **ID**: `cpt-cf-account-management-nfr-data-quality`

AM hierarchy changes **MUST** be committed transactionally across `tenants` and `tenant_closure` together and become immediately visible in the source-of-truth tenant tables. Mandatory fields **MUST** be validated before persistence. The Tenant Resolver Plugin reads those tables directly over a read-only database role and therefore observes every committed change the moment the transaction commits — there is no projection freshness window, no sync interval, and no barrier-propagation latency between AM and the Plugin.

#### Data Integrity Diagnostics

- [ ] `p2` - **ID**: `cpt-cf-account-management-nfr-data-integrity-diagnostics`

AM **MUST** provide diagnostic checks for hierarchy integrity anomalies that it can observe directly, including orphaned children, broken parent references, and depth mismatches.

#### Data Remediation Expectations

- [ ] `p2` - **ID**: `cpt-cf-account-management-nfr-data-remediation`

AM-owned integrity anomalies and compensation failures **MUST** produce operator-visible telemetry within 15 minutes of detection and a documented remediation path that can be triaged within one business day. Cross-gear cleanup gaps that AM cannot correct automatically **MUST** remain explicitly surfaced rather than silently ignored.

#### Operational Metrics Treatment

- [ ] `p2` - **ID**: `cpt-cf-account-management-nfr-ops-metrics-treatment`

AM domain metrics **MUST** be integrated into shared platform dashboards and alert routing. At minimum, operator treatment **MUST** exist for IdP failure rate, bootstrap not-ready state, provisioning reaper activity, integrity-check violations, and background cleanup failures.

### NFR Exclusions

- **Offline support**: Not applicable. AM is a server-side platform service and does not operate in offline mode.
- **Usability (UX)**: Not applicable at gear level — AM exposes REST API and SDK traits. Portal UI is a separate concern.
- **Compliance (COMPL)**: Regulatory programs and certifications remain platform-level obligations. AM's gear-specific contribution is captured in NFRs 6.4 and 6.9.
- **Safety (SAFE)**: Not applicable — AM is a pure information system with no physical interaction or safety-critical operations.
- **Operations (OPS)**: Deployment topology, on-call process, and runbook ownership are inherited platform concerns. AM-specific observability treatment is captured in NFR 6.12.
- **Maintainability / Documentation (MAINT)**: Consumer-facing contract publication and architectural sync rules are defined in DESIGN, not in this PRD.
- **Geographic distribution**: Not applicable at gear level — AM follows platform deployment topology. Data residency and cross-region replication are platform infrastructure concerns.
- **Rate limiting**: Not applicable at gear level — API rate limiting is enforced by the platform API gateway. AM does not implement gear-specific throttling.

## 7. Public Library Interfaces

### 7.1 Public API Surface

The authoritative wire contract for the public REST surface is [account-management-v1.yaml](./account-management-v1.yaml). The entries below describe the stable capability surfaces that the contract file elaborates.

#### Tenant Management API

- [ ] `p1` - **ID**: `cpt-cf-account-management-interface-tenant-mgmt-api`

- **Type**: REST API
- **Stability**: stable
- **Description**: API for tenant CRUD operations, direct-child discovery, mode-transition workflows, and the public tenant view consumed by downstream readers.
- **Breaking Change Policy**: Major version bump required for endpoint removal or incompatible request/response schema changes.

#### Tenant Metadata API

- [ ] `p2` - **ID**: `cpt-cf-account-management-interface-tenant-metadata-api`

- **Type**: REST API
- **Stability**: stable
- **Description**: API for CRUD, listing, and effective-value resolution of extensible tenant metadata defined by GTS-registered schemas.
- **Breaking Change Policy**: Major version bump required for endpoint removal or incompatible request/response schema changes.

#### User Operations API

- [ ] `p1` - **ID**: `cpt-cf-account-management-interface-user-ops-api`

- **Type**: REST API
- **Stability**: stable
- **Description**: API for tenant-scoped user provisioning, update, deprovisioning, and query operations delegated to the configured IdP provider contract.
- **Breaking Change Policy**: Major version bump required for endpoint removal or incompatible request/response schema changes.

#### Service Account API

- [ ] `p1` - **ID**: `cpt-cf-account-management-interface-service-account-api`

- **Type**: REST API + in-process Rust SDK (`AccountManagementClient` via ClientHub)
- **Stability**: stable
- **Description**: API for the tenant-scoped machine-identity lifecycle delegated to the configured IdP provider contract. Both public channels expose exactly four operations — provision/create, list, rotate-secret, revoke — addressed by owning tenant and, for rotate-secret and revoke, by client identifier; the SDK implementation delegates to the same authorized service layer as REST. There is deliberately no single-item read: the collection listing and the address returned by a provision are the only ways to learn an account's non-secret state, and no path returns an existing secret. HTTP credential responses are non-cacheable; SDK credential results retain the secret in a redacting, non-serializable wrapper for immediate transfer to the caller's credential custodian. Authorization uses four independently grantable permissions on a resource type distinct from the user type.
- **Breaking Change Policy**: Major version bump required for endpoint removal, incompatible request/response schema changes, or incompatible SDK signature changes. Resource and action identifiers cannot change within a major version.

### 7.2 External Integration Contracts

IdP implementations may align with standards such as SCIM 2.0 and OIDC where applicable, but AM defines stable behavior in terms of lifecycle outcomes rather than provider protocol details.

#### IdP Provider Contract

- [ ] `p1` - **ID**: `cpt-cf-account-management-contract-idp-provider`

- **Direction**: required from client (IdP implementation via pluggable IdP integration contract)
- **Protocol/Format**: Pluggable contract (in-process or remote)
- **Consumed / Provided Data**: tenant lifecycle intent, user lifecycle intent, service-account lifecycle intent, and provider-specific provisioning context.
- **Availability / Fallback**: AM tolerates IdP unavailability during bootstrap with retry/backoff. Read-only tenant and metadata operations remain available during IdP outages; IdP-dependent operations fail deterministically.
- **Compatibility**: Provider implementations are vendor-replaceable. Breaking changes require a versioned contract and migration path. Every operation ships a default implementation that declines as unsupported, so a partial adapter — tenant-only, or users without machine identities — is legal and declines explicitly rather than no-opping silently.
- **Service-account obligations**: an adapter that implements the machine-identity half MUST address `(tenant_id, client_id)` as the scoped resource and report an address that does not resolve within that tenant as absent; MUST reject a provision whose name is already live in the tenant, atomically with respect to concurrent calls, and never resume or reveal the existing account; MUST report the creator-supplied name verbatim on every listed account, since that is the only provider-independent correlation key when client identifiers are unpredictable; MUST return a secret only from provision and rotate-secret; MUST report transport uncertainty as ambiguous rather than as success; MUST delete a tenant's accounts when that tenant is deprovisioned; and MUST log its own diagnostics in-process, because AM relays and records none of the text the adapter returns with a failure.

#### GTS Registry Contract

- [ ] `p1` - **ID**: `cpt-cf-account-management-contract-gts-registry`

- **Direction**: required from client (schema/type resolution consumed by AM)
- **Protocol/Format**: SDK trait or equivalent registry API
- **Consumed / Provided Data**: tenant type definitions, allowed-parent rules, metadata-schema validation material, and metadata inheritance traits.
- **Availability / Fallback**: Existing AM reads that do not require fresh schema/type projection remain available if GTS is unavailable. Reverse-hydration of `tenant_type_uuid` / `schema_uuid` to chained identifiers depends on the planned UUID-keyed Types Registry lookup API (`get_by_uuid` / `resolve_traits_by_uuid`); until that ships, AM consumes the existing chained-identifier list endpoints via `TypesRegistryClient` and the reverse-hydration surface is **dependency-gated** on the SDK addition. `TypesRegistryClient` owns its own bounded TTL-aware cache; AM does not maintain a parallel cache and fails deterministically on registry-side cache miss rather than returning opaque UUIDs. Type-validating writes and metadata writes that require fresh schema lookup fail with `CanonicalError::ServiceUnavailable` (HTTP 503) when the registry is unreachable; per-pair tenant-type compatibility rejections (`INVALID_TENANT_TYPE` / `TYPE_NOT_ALLOWED`) are enforced only under `strict_barriers = true` once the UUID-keyed lookup lands — outside that gate the checker stub-admits after a registry reachability probe. AM does not invent or cache unverified schema changes locally.
- **Compatibility**: Registered schema identifiers remain the stable keys by which AM references tenant types and metadata kinds.

#### Tenant Resolver Plugin Data Contract

- [ ] `p1` - **ID**: `cpt-cf-account-management-contract-tenant-resolver`

- **Direction**: provided (AM exposes its own storage to the Tenant Resolver Plugin for direct reads)
- **Protocol/Format**: Read-only PostgreSQL role provisioned by AM with `SELECT`-only grants scoped to `tenants` and `tenant_closure`. The canonical closure shape `(ancestor_id, descendant_id, barrier, descendant_status)` defined in TENANT_MODEL.md is the on-the-wire contract. AM does not expose any sync, projection, revision-token, or change-feed surface.
- **Consumed / Provided Data**: rows of `tenants` (`id`, `parent_id`, `name`, `tenant_type_uuid`, `status`, `self_managed`, `depth`, timestamps, and any additional resolver-mapped metadata fields) and rows of `tenant_closure` (`ancestor_id`, `descendant_id`, `barrier`, `descendant_status`). Public chained `tenant_type` values are re-hydrated by the Tenant Resolver side through Types Registry; raw UUID keys are not the public contract.
- **Availability / Fallback**: AM commits tree and closure updates as one transaction, so the Tenant Resolver Plugin observes every committed hierarchy change the moment it becomes visible in the database — there is no projection lag and no cache freshness window for hierarchy state. AM persists internal `provisioning` rows on `tenants` during bootstrap/tenant-create sagas, but the `tenant_closure` contract excludes those rows entirely: closure entries are inserted atomically with the `provisioning → active` transition, so Tenant Resolver reads against the closure never observe provisioning tenants, by construction. Plugin read availability tracks AM database availability.
- **Compatibility**: Schema changes to `tenants` and `tenant_closure` are a coordinated contract change between AM and the Tenant Resolver Plugin. Migrations **MUST** be backward-compatible within a minor release so that rolling upgrades where AM and the Plugin temporarily run different versions remain safe against the shared storage.

#### AuthZ Resolver Integration

- [ ] `p1` - **ID**: `cpt-cf-account-management-contract-authz-resolver`

- **Direction**: provided by library (tenant context and hierarchy data for authorization decisions)
- **Protocol/Format**: SecurityContext propagation via Gears middleware
- **Consumed / Provided Data**: SecurityContext tenant fields, tenant hierarchy visibility, barrier state, and `schema_id`-scoped metadata authorization attributes.
- **Availability / Fallback**: AM does not provide an authorization fallback path; if AuthZ is unavailable, access decisions remain platform-owned failures.
- **Compatibility**: Changes to SecurityContext tenant fields require coordinated update with AuthZ Resolver Plugin.

#### Billing System Read Contract

- [ ] `p1` - **ID**: `cpt-cf-account-management-contract-billing`

- **Direction**: provided by library (read-only tenant and metadata views consumed by Billing System)
- **Protocol/Format**: AM public read APIs and/or SDK traits, subject to platform authorization policy
- **Consumed / Provided Data**: tenant identifiers, hierarchy relationships, tenant status and mode, and billing-relevant metadata resolved by registered schema id.
- **Availability / Fallback**: Billing must not invent hierarchy state when AM is unavailable. It may continue operating on previously synchronized billing snapshots where platform policy allows, but new hierarchy-dependent billing reads fail until AM data is reachable again.
- **Compatibility**: Billing-relevant read shapes follow the AM public versioning policy. Schema-id-based metadata categories remain the extensibility mechanism for new billing attributes.

## 8. Use Cases

### 8.1 Bootstrap

#### Scenario: Root Tenant Auto-Created on First Start

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-root-bootstrap`

**Actor**: `cpt-cf-account-management-actor-platform-admin`, `cpt-cf-account-management-actor-idp`

**Preconditions**:
- AM starts for the first time during platform installation
- IdP is available

**Main Flow**:
1. AM starts the bootstrap procedure.
2. System creates the initial root tenant through an internal provisioning flow and completes bootstrap with the tenant visible in status `active` and the configured root type.
3. System invokes the tenant-provisioning operation with the deployer-configured bootstrap metadata (e.g., `{ "adopt_realm": "master" }`), enabling the IdP provider plugin to adopt the pre-existing IdP context and configure tenant identity claim mapping.
4. System writes a bootstrap audit event via the platform audit infrastructure with a `system` actor.

**Postconditions**:
- Initial root tenant exists and is linked to IdP
- Tenant provisioning completed without error; any provider-returned metadata is persisted as tenant metadata
- Bootstrap completion state is persisted for subsequent restarts

**Alternative Flows**:
- **IdP unavailable**: See `cpt-cf-account-management-usecase-bootstrap-waits-idp`

#### Scenario: Bootstrap Is Idempotent

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-bootstrap-idempotent`

**Actor**: `cpt-cf-account-management-actor-platform-admin`

**Preconditions**:
- Initial root tenant already exists from a previous bootstrap

**Main Flow**:
1. AM restarts or the platform runs an upgrade.
2. Bootstrap checks whether the initial root tenant already exists.
3. System detects the existing tenant and performs no additional create operation.

**Postconditions**:
- No duplicate root tenant is created
- Existing root tenant remains unchanged

**Alternative Flows**:
- **None**: No additional alternative flows beyond standard restart handling

#### Scenario: Bootstrap Waits for IdP

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-bootstrap-waits-idp`

**Actor**: `cpt-cf-account-management-actor-platform-admin`, `cpt-cf-account-management-actor-idp`

**Preconditions**:
- AM bootstrap begins
- IdP is not yet available

**Main Flow**:
1. System starts bootstrap and attempts to provision the root tenant via `IdpPluginClient::provision_tenant` — `provision_tenant` itself is the readiness signal, there is no separate availability probe.
2. The IdP returns `IdpProvisionFailure::CleanFailure` indicating no IdP-side state was retained. The saga compensates the provisioning row.
3. System retries the saga with backoff until the IdP returns `Ok` or the configured timeout is reached.
4. If the IdP succeeds before the timeout, bootstrap finalizes the root.

**Postconditions**:
- Bootstrap reaches the active root only after a `provision_tenant` succeeds, or stops at the timeout boundary

**Alternative Flows**:
- **Timeout expires**: Bootstrap fails with `CanonicalError::ServiceUnavailable` (HTTP 503)

### 8.2 Tenant Lifecycle

#### Scenario: Create Child Tenant

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-create-child-tenant`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Parent tenant `T1` exists with status `active` and type `org`
- GTS registry allows type `division` under `org`
- Hierarchy depth is below the advisory threshold

**Main Flow**:
1. Tenant admin of `T1` submits a child tenant create request with type `division`, name `"East Division"`, and mode `self_managed=false`.
2. System validates parent status, type constraints, and current hierarchy depth.
3. System completes tenant creation; the new child becomes visible under parent `T1` in status `active` and managed mode. The `tenants` row and the matching `tenant_closure` entries (self-row plus one row per strict ancestor along the `parent_id` chain) are committed in the same transaction.
4. The Tenant Resolver Plugin's next barrier-aware query observes the new child immediately because it reads the same database.

**Postconditions**:
- Child tenant exists under `T1`
- Closure rows for the new child are committed and immediately queryable by the Tenant Resolver Plugin

**Alternative Flows**:
- **Type not allowed under parent**: See `cpt-cf-account-management-usecase-reject-type-not-allowed`
- **Depth threshold exceeded**: See `cpt-cf-account-management-usecase-warn-depth-exceeded`
- **Strict depth limit exceeded**: See `cpt-cf-account-management-usecase-reject-depth-exceeded`

#### Scenario: Read Tenant Details

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-read-tenant`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Tenant `T2` exists
- Caller is authorized to read `T2`

**Main Flow**:
1. Tenant administrator requests tenant details for `T2`.
2. System validates read access for the requested tenant scope.
3. System returns the current tenant details, including status, type, mode, parent reference, and audit timestamps.

**Postconditions**:
- Caller receives the current state of `T2`
- No tenant state is modified

**Alternative Flows**:
- **Tenant not found**: Request fails with `not_found`
- **Caller lacks access**: Request fails with `cross_tenant_denied`

#### Scenario: Update Tenant Mutable Fields

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-update-tenant`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Tenant `T2` exists and is not deleted
- Caller is authorized to update `T2`

**Main Flow**:
1. Tenant administrator submits an update request for `T2` with a new `name` and/or `status`.
2. System validates that only mutable fields are present in the request.
3. System applies the permitted changes.
4. System records the change via the platform audit infrastructure.

**Postconditions**:
- `T2` reflects the updated mutable fields
- Audit trail records the update operation

**Alternative Flows**:
- **Immutable field included**: Request fails with `validation`
- **Deleted tenant targeted**: Request fails because deleted tenants are immutable

#### Scenario: Reject Create — Type Not Allowed Under Parent

- [ ] `p2` - **ID**: `cpt-cf-account-management-usecase-reject-type-not-allowed`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Parent tenant `T1` has type `team`
- GTS registry defines `team` as a leaf type with no children allowed

**Main Flow**:
1. Tenant admin submits a child tenant create request under `T1`.
2. System validates the requested parent-child type relationship against GTS rules.
3. System rejects the request.

**Postconditions**:
- No child tenant is created under `T1`
- Caller receives `type_not_allowed`

**Alternative Flows**:
- **None**: No additional alternative flows beyond the validation failure

#### Scenario: Warn — Depth Advisory Threshold Exceeded

- [ ] `p2` - **ID**: `cpt-cf-account-management-usecase-warn-depth-exceeded`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Hierarchy depth is at the advisory threshold, for example 10 levels

**Main Flow**:
1. Tenant admin submits a request to create a child at depth 11.
2. System detects that the advisory threshold would be exceeded.
3. System creates the child tenant successfully.
4. System increments the `am_depth_threshold_exceeded_total` metric and writes a structured warning log entry for operators.

**Postconditions**:
- Child tenant exists beyond the advisory threshold
- Operator-visible warning signal is emitted for operators

**Alternative Flows**:
- **Strict mode enabled**: See `cpt-cf-account-management-usecase-reject-depth-exceeded`

#### Scenario: Reject Create — Depth Hard Limit (Strict Mode)

- [ ] `p2` - **ID**: `cpt-cf-account-management-usecase-reject-depth-exceeded`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Strict depth mode is enabled with a hard limit, for example 10 levels
- Hierarchy depth is already at the hard limit

**Main Flow**:
1. Tenant admin submits a request to create a child at depth 11.
2. System evaluates the request against the strict depth limit.
3. System rejects the request.

**Postconditions**:
- No child tenant is created
- Caller receives `tenant_depth_exceeded`

**Alternative Flows**:
- **None**: No additional alternative flows beyond the limit rejection

#### Scenario: Suspend Tenant Without Cascading

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-suspend-no-cascade`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

**Preconditions**:
- Tenant `T2` has child tenants `[T3, T4]` in `active` status

**Main Flow**:
1. Administrator submits a request to suspend `T2`.
2. System updates `T2` status to `suspended`.
3. System leaves child tenants `T3` and `T4` unchanged.

**Postconditions**:
- `T2` is `suspended`
- `T3` and `T4` remain `active` and fully operational

**Alternative Flows**:
- **None**: No additional alternative flows beyond the status update

#### Scenario: Reject Delete — Tenant Has Children

- [ ] `p2` - **ID**: `cpt-cf-account-management-usecase-reject-delete-has-children`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

**Preconditions**:
- Tenant `T2` has non-deleted child `T3`

**Main Flow**:
1. Administrator submits a request to delete `T2`.
2. System checks for non-deleted child tenants.
3. System rejects the delete operation.

**Postconditions**:
- `T2` remains unchanged
- Caller receives `tenant_has_children`

**Alternative Flows**:
- **None**: No additional alternative flows beyond the child-presence check

#### Scenario: Soft Delete Leaf Tenant

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-soft-delete-leaf`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

**Preconditions**:
- Tenant `T5` has no children
- Tenant `T5` has no remaining resource ownership associations in Resource Group

**Main Flow**:
1. Administrator submits a delete request for `T5`.
2. System validates that `T5` has no children and no remaining resource ownership associations.
3. System transitions `T5` to `deleted` status as a soft delete.
4. System schedules hard deletion after the configured retention period, by default 90 days.

**Postconditions**:
- `T5` is soft-deleted immediately
- Hard deletion is deferred until the retention period expires

**Alternative Flows**:
- **Active resources exist**: Delete request is rejected until resources are removed

#### Scenario: Reject Delete — Root Tenant

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-reject-delete-root`

**Actor**: `cpt-cf-account-management-actor-platform-admin`

**Preconditions**:
- Tenant `T0` is the root tenant (`parent_id = NULL`)

**Main Flow**:
1. Administrator submits a delete request for `T0`.
2. System detects that the target tenant is the root tenant.
3. System rejects the delete operation.

**Postconditions**:
- `T0` remains unchanged
- Caller receives `InvalidArgument` (HTTP 400) with `reason=ROOT_TENANT_CANNOT_DELETE`

**Alternative Flows**:
- **None**: No additional alternative flows beyond the root-invariant check

### 8.3 Managed/Self-Managed Modes

#### Scenario: Create Managed Child Tenant

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-create-managed-child`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Parent tenant `T1` is allowed to create child tenants
- Requested child is created with `self_managed=false`

**Main Flow**:
1. Tenant admin creates child tenant `T2` with `self_managed=false`.
2. System persists `T2` as a managed child tenant.
3. System exposes the parent-child relationship without a visibility barrier.

**Postconditions**:
- No visibility barrier exists between `T1` and `T2`
- `T1` is eligible for delegated access to `T2` per policy

**Alternative Flows**:
- **None**: No additional alternative flows beyond standard policy evaluation

#### Scenario: Create Self-Managed Child Tenant

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-create-self-managed-child`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Parent tenant `T1` is allowed to create child tenants
- Requested child is created with `self_managed=true`

**Main Flow**:
1. Tenant admin creates child tenant `T2` with `self_managed=true`.
2. System persists `T2` as a self-managed child tenant.
3. System establishes the visibility barrier for the new subtree.

**Postconditions**:
- Visibility barrier exists between `T1` and `T2`
- `T1` has no access to `T2` APIs or resources by default

**Alternative Flows**:
- **None**: No additional alternative flows beyond standard authorization behavior

#### Scenario: Convert Tenant Mode via Dual Consent

- [ ] `p3` - **ID**: `cpt-cf-account-management-usecase-convert-dual-consent`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

**Preconditions**:
- Tenant `T2` is a non-root direct child of `T1` with status `active`
- No pending conversion request exists for `T2`

**Main Flow**:
1. Admin of one side initiates a mode conversion request from their own tenant scope.
2. System derives the requested target mode from the tenant's current mode, records which side initiated the request, opens the approval window, and returns the created conversion request.
3. Admin of the counterparty resolves the request from their own scope by approving it within the approval window.
4. System makes the request resolution and the tenant mode change visible together and records the completed change via the platform audit infrastructure.

**Postconditions**:
- `tenants.self_managed` on `T2` reflects `target_mode`
- Conversion history shows both the initiating and approving sides with their respective identities
- `tenant_closure.barrier` is updated in the same transaction across every `(ancestor, descendant)` pair whose path `(ancestor, descendant]` contains `T2`, so barrier-aware queries served by the Tenant Resolver Plugin observe the new state the moment the approval commits

**Alternative Flows**:
- **Counterparty does not approve within the window**: See `cpt-cf-account-management-usecase-conversion-expires`
- **Initiator withdraws the request**: See `cpt-cf-account-management-usecase-cancel-conversion-by-initiator`
- **Counterparty declines the request**: See `cpt-cf-account-management-usecase-reject-conversion-by-counterparty`
- **Wrong side attempts a transition (e.g., initiator tries to approve)**: See `cpt-cf-account-management-usecase-invalid-actor-for-transition`
- **Target tenant is the root tenant**: Initiation is rejected with the deterministic validation error for root conversion.

#### Scenario: Conversion Approval Expires

- [ ] `p3` - **ID**: `cpt-cf-account-management-usecase-conversion-expires`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Pending conversion request exists for `T2`
- Neither side has resolved the request

**Main Flow**:
1. System tracks the approval window for the pending conversion request.
2. The configured approval window elapses without `approved`, `cancelled`, or `rejected` transitions.
3. Background expiry job transitions the row to `expired` and writes an audit entry with a `system` actor.
4. System preserves the existing tenant mode.

**Postconditions**:
- Conversion request status is `expired`
- `tenants.self_managed` on `T2` is unchanged
- A new conversion request can be initiated after expiry

**Alternative Flows**:
- **None**: No additional alternative flows beyond expiration handling

#### Scenario: Parent Discovers Pending Child Conversions

- [ ] `p3` - **ID**: `cpt-cf-account-management-usecase-discover-child-conversions`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Parent tenant `T1` exists with status `active`
- Self-managed child tenant `T2` exists under `T1` with a visibility barrier
- Child admin of `T2` has initiated a mode conversion (pending request exists)

**Main Flow**:
1. Parent admin asks to see pending conversion requests that target direct children of `T1` from the parent-scope discovery surface.
2. System returns a paginated list of conversion requests targeting direct children of `T1`.
3. Each entry includes the child tenant identity, initiating side, requested target mode, lifecycle status, and review timestamps needed to act on the request.
4. Parent admin reviews the pending request and either approves, rejects, or leaves it for expiry.

**Postconditions**:
- Parent admin has visibility into pending requests from barrier-isolated children without any barrier bypass
- No child tenant data beyond conversion-request metadata is exposed

**Alternative Flows**:
- **No pending requests**: Empty result set is returned
- **Operator wants historical view**: System can return recently resolved requests that remain inside the configured retention window through an operator-authorized history workflow

#### Scenario: Initiator Withdraws a Pending Conversion Request

- [ ] `p3` - **ID**: `cpt-cf-account-management-usecase-cancel-conversion-by-initiator`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- A pending conversion request exists for tenant `T2`
- The caller represents the same side that initiated the request

**Main Flow**:
1. Initiator admin requests withdrawal of the pending conversion from their own tenant scope.
2. System validates that the request is pending and that the caller's side matches the initiator side.
3. System transitions the row to `cancelled`, stamps `cancelled_by = caller`, and writes an audit entry.

**Postconditions**:
- Conversion request status is `cancelled`
- `tenants.self_managed` on `T2` is unchanged
- A new conversion request can be initiated after cancellation

**Alternative Flows**:
- **Counterparty attempts to cancel**: See `cpt-cf-account-management-usecase-invalid-actor-for-transition`

#### Scenario: Counterparty Rejects a Pending Conversion Request

- [ ] `p3` - **ID**: `cpt-cf-account-management-usecase-reject-conversion-by-counterparty`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-platform-admin`

**Preconditions**:
- A pending conversion request exists for tenant `T2`
- The caller represents the counterparty side of the initiator

**Main Flow**:
1. Counterparty admin declines the pending conversion from their own tenant scope.
2. System validates that the request is pending and that the caller's side is the counterparty.
3. System transitions the row to `rejected`, stamps `rejected_by = caller`, and writes an audit entry.

**Postconditions**:
- Conversion request status is `rejected`
- `tenants.self_managed` on `T2` is unchanged
- The initiator sees in the list that their request was declined by the counterparty (distinct from their own `cancelled`)

**Alternative Flows**:
- **Initiator attempts to reject their own request**: See `cpt-cf-account-management-usecase-invalid-actor-for-transition`

#### Scenario: Invalid Actor for Transition

- [ ] `p3` - **ID**: `cpt-cf-account-management-usecase-invalid-actor-for-transition`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- A pending conversion request exists for tenant `T2`

**Main Flow**:
1. An actor from the wrong side attempts a transition that their side is not allowed to drive, such as an initiator trying to approve or reject, or a counterparty trying to cancel.
2. AM detects the role/status mismatch before any state change is applied.
3. System returns the deterministic "invalid actor for transition" conflict response with enough detail for the caller to understand which transition was rejected.

**Postconditions**:
- Conversion request state is unchanged (still `pending`)
- Caller can disambiguate which role/status combination was rejected from the deterministic error details

**Alternative Flows**:
- **Request is already resolved (approved/cancelled/rejected/expired)**: System returns `FailedPrecondition` (HTTP 400) with `reason=ALREADY_RESOLVED` instead of `INVALID_ACTOR_FOR_TRANSITION`

#### Scenario: Retention of Resolved Conversion Requests

- [ ] `p3` - **ID**: `cpt-cf-account-management-usecase-retention-of-resolved-conversion`

**Actor**: `cpt-cf-account-management-actor-platform-admin`

**Preconditions**:
- Conversion requests in status `approved`, `cancelled`, `rejected`, or `expired` exist beyond the configured resolved-retention window (default 30 days)

**Main Flow**:
1. The AM retention background job identifies resolved requests that have aged past the configured review window.
2. Job removes those requests from the default day-to-day discovery surface while preserving them temporarily for retained history and audit workflows.
3. Subsequent hard-delete cadence permanently removes the retained history records.

**Postconditions**:
- Day-to-day API listings stay focused on recent activity
- Operator tooling can still inspect retained history until hard deletion
- Pending-request safeguards continue to apply only to active review items, not to already resolved history

**Alternative Flows**:
- **Operator queries all retained history**: A privileged operator workflow may inspect retained history that is no longer part of the standard tenant-admin surface

### 8.4 User Groups

> User group operations are performed by consumers directly via the [Resource Group gear](../../resource-group/docs/PRD.md). AM's role is limited to registering the user-group RG type at gear initialization and triggering tenant-scoped group cleanup during hard-deletion. Structural invariants (cycle detection, forest enforcement, tenant scoping) are enforced by Resource Group; see [Resource Group use cases](../../resource-group/docs/PRD.md#8-use-cases).

#### Scenario: Create User Group via Resource Group

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-create-user-group`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Tenant `T1` exists with status `active`
- User-group Resource Group type `gts.cf.core.rg.type.v1~cf.core.am.user_group.v1~` is registered with `allowed_memberships` including `gts.cf.core.am.user.v1~`

**Main Flow**:
1. Tenant admin calls the Resource Group API directly to create a Resource Group entity of the user-group type within tenant scope `T1`.
2. Resource Group validates type compatibility, tenant scope, and forest invariants; persists the group.

**Postconditions**:
- Group exists as a Resource Group entity within tenant `T1`
- Group identifier is unique within the tenant scope (enforced by Resource Group)

> AM is not in the call path — the consumer interacts with Resource Group directly.

#### Scenario: Manage Group Membership via Resource Group

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-manage-group-membership`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- User-group Resource Group entity `G1` exists in tenant `T1`
- User `U1` belongs to tenant `T1` (verified via AM's tenant-scoped user-query capability)

**Main Flow**:
1. Admin adds `U1` to `G1` through the Resource Group membership API.
2. Resource Group validates tenant compatibility and stores the membership link.
3. Admin removes `U1` from `G1` through the same Resource Group membership API.
4. Resource Group removes the membership link.

**Postconditions**:
- Group membership reflects the most recent update
- `U1` is no longer a member of `G1`

> AM is not in the call path — the consumer interacts with Resource Group directly. User existence verification is the caller's responsibility (via AM's tenant-scoped user-query capability).

#### Scenario: Reject Circular Group Nesting (Resource Group Invariant)

- [ ] `p2` - **ID**: `cpt-cf-account-management-usecase-reject-circular-nesting`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Nested user-group Resource Group entities `G1 → G2 → G3` already exist

**Main Flow**:
1. Admin requests Resource Group to move `G1` under `G3`.
2. Resource Group evaluates forest invariants and detects a cycle.
3. Resource Group rejects the operation with `CycleDetected`.

**Postconditions**:
- Existing nesting structure is preserved
- Caller receives `CycleDetected` error directly from Resource Group

> AM is not in the call path — cycle detection is a Resource Group invariant. `CycleDetected` is an RG-owned error, not part of AM's error contract.

### 8.5 IdP User Operations

#### Scenario: Provision User in Tenant

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-provision-user`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-idp`

**Preconditions**:
- Tenant `T1` exists with status `active`

**Main Flow**:
1. Tenant admin submits a request to provision user `U1` via the AM API.
2. AM invokes the IdP integration contract's user-provisioning operation with tenant scope `T1`.
3. IdP creates user `U1` and binds the user to tenant `T1`.

**Postconditions**:
- User `U1` exists in the IdP
- User `U1` is bound to tenant `T1` via the tenant identity attribute

**Alternative Flows**:
- **IdP unavailable**: IdP contract call fails or times out. AM returns `CanonicalError::ServiceUnavailable` (HTTP 503) to the caller. No user record is created or modified. (See `cpt-cf-account-management-fr-deterministic-errors`.)

#### Scenario: Deprovision User

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-deprovision-user`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-idp`

**Preconditions**:
- User `U1` exists in tenant `T1`

**Main Flow**:
1. Tenant admin submits a request to deprovision `U1`.
2. AM invokes the IdP integration contract's user-deprovisioning operation.
3. System revokes active sessions for `U1`.

**Postconditions**:
- User `U1` is removed or deactivated according to IdP behavior
- Active sessions for `U1` are revoked

**Alternative Flows**:
- **IdP unavailable**: IdP contract call fails or times out. AM returns `CanonicalError::ServiceUnavailable` (HTTP 503) to the caller. User `U1` remains in its current state. (See `cpt-cf-account-management-fr-deterministic-errors`.)

#### Scenario: Update User in Tenant

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-update-user`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-idp`

**Preconditions**:
- Tenant `T1` exists with status `active`
- User `U1` exists in tenant `T1`

**Main Flow**:
1. Tenant admin submits a JSON Merge Patch of `U1`'s mutable attributes via the AM API.
2. AM authorizes the caller, resolves `T1`, and validates the changed fields against `gts.cf.core.am.user.v1~`.
3. AM invokes the IdP integration contract's user-update operation with tenant scope `T1`.
4. IdP applies the patch and returns the updated user projection.

**Postconditions**:
- User `U1`'s attributes are updated in the IdP
- No AM-side user record is created or modified (AM owns none)

**Alternative Flows**:
- **User absent**: the IdP reports `U1` absent in `T1`. AM returns `CanonicalError::NotFound` (HTTP 404) — the absence is NOT folded into success (contrast: deprovision). (See `cpt-cf-account-management-fr-idp-user-update`.)
- **Username collision**: a `username` rename collides with an existing login. AM returns `CanonicalError::AlreadyExists` (HTTP 409).
- **IdP unavailable**: the IdP contract call fails or times out (both collapse to `IdpUserOperationFailure::Unavailable`). AM returns `CanonicalError::ServiceUnavailable` (HTTP 503) and persists no local state (it owns none). On a timeout the outcome is **ambiguous** — the IdP may have applied the patch before the response was lost — so AM does NOT assert the attributes are unchanged. Because a JSON Merge Patch is idempotent, the caller may re-read `U1` to observe the actual state or safely retry the same patch once the IdP recovers. (See `cpt-cf-account-management-fr-deterministic-errors`.)

#### Scenario: Query Users by Tenant

- [ ] `p1` - **ID**: `cpt-cf-account-management-usecase-query-users-by-tenant`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-idp`

**Preconditions**:
- Tenant `T1` has users `[U1, U2, U3]`

**Main Flow**:
1. Tenant admin queries users of `T1`.
2. AM invokes the IdP integration contract's tenant-scoped user-query operation with a tenant filter for `T1`.
3. IdP returns the matching users.

**Postconditions**:
- Response contains `[U1, U2, U3]`
- Returned users are scoped to tenant `T1`

**Alternative Flows**:
- **IdP unavailable**: IdP contract call fails or times out. AM returns `CanonicalError::ServiceUnavailable` (HTTP 503) to the caller. (See `cpt-cf-account-management-fr-deterministic-errors`.)

### 8.6 Extensible Tenant Metadata

#### Scenario: Register and Write Tenant Metadata

- [ ] `p2` - **ID**: `cpt-cf-account-management-usecase-write-tenant-metadata`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`, `cpt-cf-account-management-actor-gts-registry`

**Preconditions**:
- Tenant `T1` exists with status `active`
- A branding metadata schema is registered with `inheritance_policy: inherit`

**Main Flow**:
1. Tenant admin submits branding metadata for tenant `T1`.
2. System validates the payload against the registered branding schema.
3. System stores the metadata entry scoped to `T1`.

**Postconditions**:
- Branding metadata is stored for `T1`
- Child tenants without overrides inherit `T1` branding via the resolution API (`inheritance_policy: inherit`)

**Alternative Flows**:
- **Schema validation fails**: Write is rejected with `validation` error
- **Metadata schema not registered**: Write is rejected as `NotFound` (HTTP 404) with `resource_type=gts.cf.core.am.tenant_metadata.v1~` and `resource_name` carrying the missing `schema_id`

#### Scenario: Resolve Inherited Metadata

- [ ] `p2` - **ID**: `cpt-cf-account-management-usecase-resolve-inherited-metadata`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Tenant `T1` has branding metadata stored under a registered inheritable schema
- Child tenant `T2` has no branding metadata of its own

**Main Flow**:
1. Consumer requests the effective branding metadata for tenant `T2`.
2. System finds no `T2`-level entry for that metadata kind.
3. System walks up the hierarchy and finds `T1`'s entry.
4. System returns `T1`'s branding as the effective value for `T2`.

**Postconditions**:
- `T2` receives `T1`'s branding metadata via inheritance
- No metadata entry is created for `T2`

**Alternative Flows**:
- **`T2` has its own entry**: `T2`'s value takes precedence (override)
- **`override_only` schema**: Resolution returns empty — no hierarchy walk

#### Scenario: Write Override-Only Metadata

- [ ] `p2` - **ID**: `cpt-cf-account-management-usecase-write-override_only-metadata`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- A billing-address metadata schema is registered with `inheritance_policy: override_only`
- Tenant `T2` exists with status `active`

**Main Flow**:
1. Tenant admin writes billing-address metadata for `T2`.
2. System validates the payload against the registered billing-address schema.
3. System stores the entry scoped to `T2`.

**Postconditions**:
- `T2` has its own billing-address metadata
- Child tenants of `T2` do not inherit this value (`override_only` policy)

**Alternative Flows**:
- **None**: No additional alternative flows beyond standard validation

#### Scenario: Resolve Metadata Across Multiple Self-Managed Boundaries

- [ ] `p2` - **ID**: `cpt-cf-account-management-usecase-resolve-metadata-multi-barrier`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Hierarchy: `Root → T1 (self-managed) → T2 → T3 (self-managed) → T4`
- `Root` has branding metadata stored under a registered inheritable schema
- `T3` has a metadata entry for the same branding schema
- `T4` has no branding metadata of its own

**Main Flow**:
1. Consumer requests the effective branding metadata for tenant `T4`.
2. System walks up the hierarchy from `T4` and encounters `T3`'s self-managed barrier.
3. System stops the walk at `T3` (barrier boundary) and returns `T3`'s branding as the effective value.

**Postconditions**:
- `T4` receives `T3`'s branding metadata (nearest ancestor within the same barrier boundary)
- `Root`'s branding is not considered because `T3`'s self-managed barrier stops traversal

**Alternative Flows**:
- **`T3` has no metadata either**: Resolution returns empty — the walk stops at the barrier and does not cross into `T1`'s scope

#### Scenario: List Metadata Entries of a Tenant

- [ ] `p2` - **ID**: `cpt-cf-account-management-usecase-list-tenant-metadata`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Tenant `T1` exists with status `active`
- `T1` has stored branding and billing-address metadata entries
- No entry exists for any other schema on `T1`

**Main Flow**:
1. Tenant admin requests the list of metadata entries written directly on `T1`.
2. System returns the two stored entries, each with its metadata kind, payload, and audit timestamps.
3. Pagination metadata is included consistent with other AM list APIs.

**Postconditions**:
- Response contains exactly the two entries actually stored on `T1`
- Inherited values (from ancestors) are **not** included — inheritance is observable only through the metadata-resolution capability

**Alternative Flows**:
- **Tenant has no metadata yet**: Response is an empty page, not an error
- **Caller lacks the `list` action on resource type `Metadata`**: Request is rejected with `cross_tenant_denied` / authorization error per the deterministic error contract

#### Scenario: Distinguish Unregistered Schema From Missing Entry

- [ ] `p2` - **ID**: `cpt-cf-account-management-usecase-metadata-schema-vs-entry-not-found`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Tenant `T1` exists with status `active`
- The intended branding metadata schema is registered
- `T1` has no entry for that schema yet
- A misspelled metadata schema identifier is not registered in GTS

**Main Flow**:
1. Client requests a metadata entry using the misspelled schema identifier.
2. System returns `NotFound` with `resource_type=gts.cf.core.am.tenant_metadata.v1~` and the missing `schema_id` as `resource_name`.
3. Client corrects the identifier to the registered branding schema and retries the same lookup.
4. System returns `NotFound` with `resource_type=gts.cf.core.am.tenant_metadata.v1~` and the missing `(tenant_id, schema_id)` pair as `resource_name`.

**Postconditions**:
- Both paths return the same canonical category (`CanonicalError::NotFound`); clients branch on the `resource_name` payload (and request context) to distinguish them — the typo case is a client/configuration bug surfaced with the unregistered `schema_id` in `resource_name`, while the second case is a normal "unset" state surfaced with the `(tenant_id, schema_id)` pair and triggers a write to populate the entry.

**Alternative Flows**:
- **Client uses the resolution capability**: The same distinction applies there as well — an unregistered schema returns `CanonicalError::NotFound` (HTTP 404) with `resource_type=gts.cf.core.am.tenant_metadata.v1~` and `resource_name` identifying the missing schema, while a registered but empty chain returns the standard empty-resolution response, not `NotFound`.

#### Scenario: Per-Schema Permission Denial

- [ ] `p2` - **ID**: `cpt-cf-account-management-usecase-metadata-permission-denied-per-schema`

**Actor**: `cpt-cf-account-management-actor-tenant-admin`

**Preconditions**:
- Tenant `T1` exists with status `active`
- Both branding and billing-address metadata schemas are registered
- Policy grants actor the `write` action on resource type `Metadata` scoped to `T1` only for the branding schema kind

**Main Flow**:
1. Actor writes a branding payload to `T1` — request is authorized and stored.
2. Actor writes a billing-address payload to `T1` — request is rejected with the authorization error, not a validation error, because policy does not grant `Metadata.write` for that metadata kind.

**Postconditions**:
- Only the branding entry exists on `T1`
- Auditable trail shows the billing-address attempt was denied at the authorization layer with the metadata kind carried in the decision context

**Alternative Flows**:
- **Read variant**: The same policy shape restricts `Metadata.read` / `Metadata.list` per-`schema_id`; list responses omit entries the actor is not permitted to read.

## 9. Acceptance Criteria

- [ ] Initial root tenant is automatically created during platform installation and linked to IdP; bootstrap is idempotent and does not create a duplicate root on restart or upgrade.
- [ ] Bootstrap waits for IdP readiness before completing the initial root-tenant creation path, retries with backoff while the IdP is unavailable, and fails cleanly after the configured timeout.
- [ ] Root tenant is undeletable; any deletion attempt against the root tenant fails with the deterministic root-protection validation error, preserving exactly one root tenant per deployment.
- [ ] Child tenants can be created with GTS-enforced type constraints; depth advisory threshold emits an operator-visible warning signal, strict mode rejects when enabled.
- [ ] Authorized administrators can read tenant details and update mutable tenant fields; immutable hierarchy-defining fields are rejected by the general update operation.
- [ ] Managed tenants do not create visibility barriers and remain eligible for delegated parent administration under platform authorization policy; self-managed tenants block default parent access via visibility barriers.
- [ ] Any post-creation toggle of a tenant's `self_managed` flag (in either direction) goes through a dual-consent `ConversionRequest`: one side initiates from its own authorized scope, the counterparty resolves from its own scope within the configured approval window (default 72h). The target mode is always derived by AM from current tenant state.
- [ ] Creation-time declaration of self-managed mode does not require a `ConversionRequest`; the parent actor's explicit act of creating the tenant in that mode is the required consent.
- [ ] Tenant creation is intentionally non-idempotent: a clean compensated IdP-availability failure (`CanonicalError::ServiceUnavailable`) may be retried, while ambiguous transport failures, timeouts, and generic platform faults require reconciliation before retry.
- [ ] Direct children queries return paginated results with status filtering.
- [ ] IdP user operations (provision, deprovision, query) work through pluggable IdP integration contract.
- [ ] User deprovisioning is idempotent: deleting an already-absent IdP user returns success while a missing tenant still returns `not_found`.
- [ ] AM gear initialization registers or verifies the chained Resource Group user-group type schema `gts.cf.core.rg.type.v1~cf.core.am.user_group.v1~`, including `allowed_memberships = [gts.cf.core.am.user.v1~]` and self-referential `allowed_parents`, before user-group operations are relied upon.
- [ ] User groups (delegated to Resource Group) support creation, membership management, and nested groups with cycle detection via Resource Group forest invariants.
- [ ] Extensible tenant metadata (e.g., branding, contacts, billing-address) is configurable per tenant via GTS-registered schemas, with per-schema inheritance policy, exposed via tenant metadata resolution API.
- [ ] Tenant metadata entries written directly on a tenant are discoverable via a dedicated listing endpoint with pagination; listing honors self-managed barriers the same way other tenant-scoped reads do.
- [ ] Authorization policy can restrict `read`, `write`, `delete`, and `list` actions on resource type `Metadata` at `schema_id` granularity; `PolicyEnforcer` receives `schema_id` as a resource attribute on every metadata operation.
- [ ] `CanonicalError::NotFound` errors on metadata endpoints use canonical envelope semantics with stable `resource_type` (`gts.cf.core.am.tenant_metadata.v1~`) and `resource_name` identifiers that distinguish a missing schema (the chained `schema_id`) from a missing entry (the `(tenant_id, schema_id)` pair).
- [ ] Tenant isolation is verified by automated security tests: Tenant A cannot access Tenant B data through any path.
- [ ] Tenant context validation completes in p95 ≤ 5ms (hot path, served by Tenant Resolver Plugin over AM-owned storage).
- [ ] Control-plane (administrative) endpoints meet the following latency targets on the §13 approved deployment profile (1K rps peak). These are admin-plane targets, not request-path enforcement; the hot-path budget above is the binding p95 for read-path tenant context:

    | Endpoint class | p95 | p99 |
    |----------------|-----|-----|
    | `GET` single tenant / `GET` single metadata entry | ≤ 50 ms | ≤ 150 ms |
    | `GET` list (direct children, metadata listing, conversion-request discovery) | ≤ 150 ms | ≤ 400 ms |
    | `POST` create tenant (includes IdP provisioning saga) | ≤ 300 ms | ≤ 1000 ms |
    | `PATCH` / `PUT` update (tenant mutable fields, metadata full-replace) | ≤ 200 ms | ≤ 500 ms |
    | `DELETE` (soft-delete, with child/resource precondition checks) | ≤ 150 ms | ≤ 500 ms |
    | Mode conversion request create / resolve | ≤ 200 ms | ≤ 500 ms |

    IdP-dependent endpoints (tenant create, user provisioning, user deprovisioning) inherit IdP latency and may exceed these p99 bounds when the IdP itself is slow; in that case the AM-side error contract (`CanonicalError::ServiceUnavailable`, HTTP 503) applies rather than a latency-violation SLO.
- [ ] All tenant configuration changes are recorded in the platform append-only audit infrastructure with actor identity, tenant identity, and change details. AM-owned non-request transitions (bootstrap completion, conversion expiry, provisioning-reaper compensation, hard-delete / tenant-deprovision cleanup) carry `actor=system` with the same change-details payload. **v1 acceptance exception** (per §6.4): until the platform append-only audit sink lands, AM-owned non-request transitions emit a structured log on the `am.events` target with `actor=system` and full change details as the v1 stand-in; full sink integration is tracked as a follow-up.
- [ ] All failures map to deterministic error categories.
- [ ] AM exports the documented domain-specific metrics for dependency health, metadata resolution, bootstrap lifecycle, tenant-retention work, conversion lifecycle, hierarchy-depth threshold exceedance, and cross-tenant denials.
- [ ] Concurrent mode conversion requests targeting the same tenant produce deterministic outcomes: duplicate initiation is rejected with the deterministic pending-request conflict response, and AM preserves the invariant that only one pending request can exist per tenant at a time.
- [ ] Conversion request lifecycle has four distinct resolved statuses: `approved` (counterparty consents), `cancelled` (initiator withdraws), `rejected` (counterparty declines), `expired` (approval window elapsed). Each is a terminal state carried in `status` — not collapsed into audit metadata.
- [ ] Wrong-side attempts on a pending conversion request (e.g., initiator tries to approve or reject; counterparty tries to cancel) fail with the deterministic invalid-actor conflict response, and the returned error detail is sufficient for clients to disambiguate the rejected transition.
- [ ] Resolved conversion requests (`approved`/`cancelled`/`rejected`/`expired`) are retained for the configured window (default 30 days) and then soft-deleted; default queries exclude soft-deleted rows.
- [ ] Metadata resolution returns the correct value when multiple self-managed boundaries exist in the ancestor chain.
- [ ] Hard-deletion background job correctly processes leaf-first ordering when parent and child share the same retention window.
- [ ] IdP timeout during user provisioning results in deterministic rollback with `CanonicalError::ServiceUnavailable` (HTTP 503).
- [ ] Parent Tenant Administrator can list conversion requests for direct children through the dedicated parent-scope discovery capability without barrier bypass; only conversion-request metadata needed for review and resolution is exposed.
- [ ] Initiator can withdraw a pending conversion request from their own scope, counterparty can decline it from their own scope, and neither side can drive the other's transition.

## 10. Dependencies

**AM depends on:**

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| IdP Provider (via IdP integration contract) | User authentication, token issuance, user-tenant binding. IdP must be available before AM bootstrap completes. | p1 |
| GTS Types Registry | Provides runtime-extensible tenant type definitions and parent-child constraint validation at tenant creation time. | p1 |
| [Resource Group](../../resource-group/docs/PRD.md) | User group hierarchy, membership storage, cycle detection, tenant-scoped isolation, and the ownership-graph data AM reads to verify that no tenant-owned resource associations remain before soft deletion. | p1 |

**Depend on AM (consumers):**

| Consumer | What it consumes |
|----------|-----------------|
| Tenant Resolver Plugin | Reads AM-owned `tenants` and `tenant_closure` directly via a read-only database role and serves barrier-aware query APIs over that data. |
| AuthZ Resolver Plugin | Consumes tenant hierarchy and barrier state for authorization decisions. |
| RBAC Engine | Consumes user group structure and membership data from Resource Group for group-to-role binding; uses AM tenant hierarchy and barrier context separately where needed. |
| Billing System | Consumes tenant hierarchy metadata with barrier bypass for billing aggregation. |

## 11. Assumptions

- IdP is a pluggable component accessed via the IdP integration contract. The platform ships a default implementation; deployments can substitute vendor-specific providers behind the same contract.
- Initial root tenant is created during platform install; IdP is bootstrapped by infrastructure before AM starts. The initial Platform Administrator identity is provisioned in the IdP as part of infrastructure setup — AM links the root tenant to this pre-existing identity but does not create the admin user itself.
- User provisioning follows an API-only invite model; no self-registration is supported.
- Parent-child tenant creation is governed by GTS type constraints (allowed-parents rules); extensible by registering new GTS type schemas without code changes.
- The v1 tenant-mode contract is binary: `managed` and `self-managed`. Richer barrier taxonomies are explicitly out of scope for this review baseline.
- RBAC Engine handles role definitions, role assignments, and group-to-role binding; Resource Group provides group structure and membership data, while AM provides tenant hierarchy, barrier context, and IdP-backed user lookup when needed.
- Resource ownership is represented in the Resource Group ownership graph; tenant deletion validation relies on the absence of resource associations under the tenant's ownership scope.
- Authorization enforcement (AuthZ Resolver) and barrier-aware traversal (Tenant Resolver Plugin) are downstream consumers of AM data. The Tenant Resolver Plugin is a stateless query facade over the same database that AM owns, consuming `tenants` and `tenant_closure` directly through a read-only database role; the Plugin holds no derived state of its own, so consistency with AM follows from transactional co-maintenance of those tables inside AM.

## 12. Risks

| Risk | Likelihood | Severity | Impact | Mitigation |
|------|-----------|----------|--------|------------|
| Cross-tenant data leak due to query-level isolation bypass | Low | High | Tenant data exposure, contractual and legal liability | Automated security test suite with cross-tenant access attempts; barrier enforcement at query level; continuous monitoring |
| Tenant hierarchy depth exceeding practical limits at scale | Low | Medium | Performance degradation of hierarchy queries and projections | Configurable advisory threshold (default: 10) with opt-in strict mode; monitoring of hierarchy depth distribution; closure-table sizing and indexing anchored to the §13 approved deployment profile (100K tenants, 1K rps) per §6.8 |
| Circular nesting in user groups | Low | Low | Infinite loops in permission resolution | Enforced by Resource Group forest invariants — cycle detection at group move/create time; consumers receive `CycleDetected` directly from Resource Group before persistence. AM is not in the call path. |
| IdP provider unavailability during operations | Medium | High | Tenant creation and user lifecycle operations become temporarily unavailable | Clear deterministic error mapping (`CanonicalError::ServiceUnavailable`, HTTP 503), bootstrap retry/backoff, and operator alerting via observability metrics |
| Ambiguous retry after tenant-create timeout or generic platform fault | Medium | Medium | Blind retry of tenant creation can trigger duplicate tenant or IdP-side provisioning attempts because the initial outcome may already have crossed the IdP boundary | Document tenant creation as intentionally non-idempotent; require reconciliation before retry; use platform audit records, compensation metrics, and reaper cleanup for investigation |

## 13. Review Baseline Decisions

The following decisions are fixed for the review-ready v1 baseline:

- **Barrier model**: v1 standardizes a binary `self_managed` barrier contract across AM, Tenant Resolver, and AuthZ Resolver.
- **User reassignment**: moving a user between tenants remains a v1 non-goal because it requires cross-platform coordination beyond the IdP contract.
- **Approved deployment profile**: 100K tenants, 300K users (IdP-stored), 30K user groups / 300K memberships (RG-stored), and 1K rps peak.
- **Authoritative detailed artifacts**: REST wire contract authority is [account-management-v1.yaml](./account-management-v1.yaml); reference storage schema authority is [migration.sql](./migration.sql).

## 14. Open Questions

_None open for the v1 baseline._

**Resolved for v1:**

- **Managed-tenant impersonation — out of scope for v1.** AM does not issue impersonation tokens and exposes no impersonation endpoint. Delegated parent administration of managed children is handled exclusively through platform AuthZ policy evaluation over normal `SecurityContext`, not through a separate impersonation flow. If future versions require impersonation, the change is coordinated across PRD, DESIGN, OpenAPI, AuthZ vocabulary, IdP contract, and security/test scope and will be tracked under a new ADR at that time; it is **not** a deferred v1 commitment.

## 15. Traceability

- **Upstream requirements**: No UPSTREAM_REQS document exists for account-management. AM requirements are derived directly from platform architecture needs and business use cases documented in this PRD.
- **Downstream artifacts**: AM-specific [DESIGN](./DESIGN.md) exists. `DECOMPOSITION` and `FEATURE` artifacts are intentionally not part of this review-readiness pass, so downstream implementation traceability remains an accepted open gap.
- **Canonical platform references**:
  - [Authorization DESIGN](../../../../docs/arch/authorization/DESIGN.md) — authoritative source for `SecurityContext`, AuthN/AuthZ separation, and request-path enforcement
  - [Security Overview](../../../../docs/security/SECURITY.md) — inherited platform security, audit, and defense-in-depth controls
  - [ToolKit Unified System](../../../../docs/toolkit_unified_system/README.md) — inherited runtime, migration, and rolling-upgrade conventions
  - [Tenant Model](../../../../docs/arch/authorization/TENANT_MODEL.md) — platform-wide tenant terminology and ownership semantics
  - [Tenant Resolver README](../../tenant-resolver/README.md) — current resolver traversal and barrier behavior consumed by downstream authorization components
