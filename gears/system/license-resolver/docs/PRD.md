<!-- cpt:
version: 1.0.0
status: draft
module: license-resolver
system: cf
-->

# PRD — License Resolver

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
- [4. Scope](#4-scope)
  - [4.1 In Scope](#41-in-scope)
  - [4.2 Out of Scope](#42-out-of-scope)
- [5. Functional Requirements](#5-functional-requirements)
  - [5.1 License Resolution](#51-license-resolution)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Module-Specific NFRs](#61-module-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
- [8. Use Cases](#8-use-cases)
  - [Gate Access to a Licensable Resource](#gate-access-to-a-licensable-resource)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

License Resolver is a read-only CF/Gears system module that answers a single question: *is a specific resource
licensed (granted) to a specific subject right now?* Callers ask `is_licensed(request)` — a single `LicenseCheckRequest`
carrying the subject and resource contract objects (each with schematized `metadata`) and the tenant context — and
receive a yes/no decision plus structured, non-authoritative diagnostics (debug information about how the decision was
reached). It is the authoritative point-in-time license check used by other modules to gate access.

License Resolver owns no grant data; it delegates the lookup to a pluggable backend selected at runtime, mirroring
authz-resolver and tenant-resolver. This keeps licensing storage, issuance, and billing concerns out of the resolver and
behind a stable contract.

### 1.2 Background / Problem Statement

Multiple CF modules need to check whether a subject may use a licensable resource (a feature, a content item, a
capability) before granting access. The subject is whoever the license is granted to — a tenant, a user, or any future
subject type — identified by its licensing contract type plus an optional id.
Today no such check exists: CF modules simply do not contain license-resolution logic, so there is no shared contract
for it, no common GTS-typed identity, and no fail-closed guarantee to rely on when gating access.

A dedicated resolver consolidates the check behind one contract with consistent GTS-typed subject and resource
identity and consistent deny semantics. Because grant facts live in heterogeneous backends owned by
different vendors, the resolver must delegate the lookup rather than own a store — matching the proven authz-resolver /
tenant-resolver delegation model. (Tenancy enters only as the isolation scope, carried in the request context that the
caller derives from its `SecurityContext`.)

Licensing is also split across levels: the Gear performing enforcement knows *where* a check belongs, while only the
platform vendor — who composes a concrete platform out of Gears and a licensing backend — knows *what* must be
licensed, and different platform vendors apply different licensing models to the same Gear (one licenses every LLM
model individually, another does not distinguish models at all). The check payload must therefore be a stable,
observable contract, not an ad-hoc payload that exists only in Gear source code: platform vendors need to see which
checks exist, which Subject/Resource pairs they involve, and which properties are available to author rules against.

### 1.3 Goals (Business Outcomes)

- Single check contract: one `is_licensed` operation reused by all callers, giving modules a license check they do not
  have today and preventing future per-module divergence.
- Fail-closed guarantee: a license is never granted by default when the backend is unavailable; deny is the safe outcome
  in 100% of unavailable-backend cases.
- Backend independence: licensing backends can be swapped or added via plugin discovery with zero caller code changes.
- Governable licensing surface: every check payload is a registered, versioned licensing contract that platform vendors
  can enumerate, review, and author rules against — without reading Gear source code.

### 1.4 Glossary

| Term                  | Definition                                                                                                                                                                                                                                                                                                                                           |
|-----------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `LicenseCheckRequest` | A single object bundling the inputs of one check: the subject and resource contract objects and the tenant context. The contract's growth surface (new inputs are added as request fields).                                                                                                                                                          |
| Licensing Contract    | A registered, versioned GTS type derived from the licensing base types, describing a Subject or Resource shape: identity fields plus the schema of its `metadata`; a Resource contract also declares which Subject types it admits. The set of a Gear's derived contract types is its published licensing surface.                                   |
| Subject               | The "someone" a license is checked for — an instance of a derived Subject contract type, carrying its licensing contract type (required), an optional id (well-known name or UUID), and `metadata` conforming to the contract schema. Polymorphic — e.g. a tenant, a user, or any future subject type (the licensee is not restricted to tenants). |
| Resource              | The licensable thing — an instance of a derived Resource contract type, carrying its licensing contract type (required), an optional instance id (well-known name or UUID), and `metadata` conforming to the contract schema. Without the id the check targets the whole resource type; with it, a specific resource.                             |
| Licensing Projection  | Copying the licensing-relevant slice of a Gear's domain objects into its licensing contract types when assembling a check. Domain types evolve freely; the projection changes only deliberately, under contract review.                                                                                                                              |
| Platform Vendor       | The party that composes a concrete platform out of Gears and a licensing backend and defines the licensing rules; reviews, approves, and authors rules against the licensing contracts Gears expose.                                                                                                                                                 |
| Grant                 | A backend fact that a resource is licensed to a subject.                                                                                                                                                                                                                                                                                             |
| Plugin                | A backend implementation discovered via the GTS types registry that answers the check.                                                                                                                                                                                                                                                               |
| Metadata              | Licensing-relevant properties on the Subject/Resource contract objects (e.g. region, model name, user category). Validated against the registered contract schema, semantically uninterpreted by the resolver, forwarded unchanged to the backend; the extension point for attribute/constraint-based licensing.                                     |

## 2. Actors

> **Note**: Stakeholder needs are managed at project/task level by steering committee. Document **actors** (users,
> systems) that interact with this module.

### 2.1 Human Actors

#### Platform Vendor

**ID**: `cpt-cf-license-resolver-actor-platform-vendor`

- **Role**: Composes a concrete platform out of Gears and a licensing backend, and defines the licensing rules. Does
  not call the check; reviews, approves, and authors rules against the licensing contracts that Gears expose,
  enumerating them via the types registry — without reading Gear source code.

### 2.2 System Actors

#### Consuming Module

**ID**: `cpt-cf-license-resolver-actor-consuming-module`

- **Role**: Any CF/Gears module that must gate access to a licensable resource. Registers its derived licensing
  contract types in the types registry, projects its domain objects into them, calls `is_licensed(request)`, and
  enforces the returned decision.

#### License Backend Plugin

**ID**: `cpt-cf-license-resolver-actor-backend-plugin`

- **Role**: A vendor-supplied backend implementation that holds grant facts and answers the delegated check. Discovered
  and selected at runtime via the GTS types registry by vendor + priority.

## 3. Operational Concept & Environment

This module introduces no environment constraints beyond project defaults. Runtime, OS, lifecycle, and integration
patterns are inherited from the project-wide architecture in
[docs/ARCHITECTURE_MANIFEST.md](../../../../docs/ARCHITECTURE_MANIFEST.md) and [guidelines/](../../../../guidelines/).

## 4. Scope

### 4.1 In Scope

- Point-in-time check of whether a resource — a specific instance or a whole resource type — is licensed to a single
  subject.
- Tenant-scoped resolution via the request's tenant context (derived from the caller's `SecurityContext`).
- Typed licensing contracts: base Subject/Resource GTS types owned by this module, derived contract types registered by
  consuming Gears, and validation of every check request against the registered contracts before delegation.
- GTS-typed identity inside the contracts (whole-type, named/well-known, and opaque).
- Plugin-delegated backend selection via the GTS types registry.

### 4.2 Out of Scope

- **Listing / enumeration of granted resources** — answering "everything licensed to a subject" is a catalog/query
  concern, not a resolver concern; no list operation and no pagination. Enumeration of licensing *contract types* is a
  different, in-scope concern served natively by the types registry — it requires no resolver API.
- License issuance and revocation — grant lifecycle is owned by issuing/management modules.
- Billing and usage metering — owned by the billing/usage domain.
- Grant storage and management — the resolver owns no grant store; backends do.
- Defining domain types — a licensing contract *projects* the licensing-relevant slice of a Gear's domain objects
  into its `metadata`; the domain types themselves are owned by their respective modules and are neither defined nor
  referenced here.

## 5. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e) targeting 90%+ code
> coverage unless otherwise specified.

### 5.1 License Resolution

#### License Check

- [ ] `p1` - **ID**: `cpt-cf-license-resolver-fr-is-licensed-check`

The system **MUST** provide a single check operation `is_licensed(request)` taking a `LicenseCheckRequest` — which
bundles the subject (whom) and resource (what) contract objects — each carrying its schematized `metadata` — and the
tenant context (the caller derives it from its `SecurityContext`) — and returning a decision indicating whether the
resource is licensed to the subject at the time of the call, together with structured, non-authoritative
**diagnostics** (a string-keyed map of debug information about how the decision was reached — e.g. which backend
answered, matched grant, denial cause).
Diagnostics are advisory only and **MUST NOT** be required for the caller to interpret the boolean outcome. The single
request object is the contract's growth surface — new inputs are added as request fields, not as new parameters or
method-signature changes. (See `cpt-cf-license-resolver-fr-evaluation-metadata` for the `metadata` field.)

- **Rationale**: A single shared check is the module's reason to exist: it provides license-resolution logic that no
  module has today, and gives one consistent contract instead of each module growing its own.
- **Actors**: `cpt-cf-license-resolver-actor-consuming-module`

#### Licensing Contract Registration

- [ ] `p1` - **ID**: `cpt-cf-license-resolver-fr-contract-registration`

Every Subject and Resource shape used in a license check **MUST** be a registered, versioned GTS type derived from the
licensing base types owned by this module (under `gts.cf.core.lic.*`) — never an ad-hoc payload assembled at the call
site. A module that wants license enforcement **MUST** register, before the check, both the Subject and Resource
contract types it checks — as derivatives of the base types — together with the exact schema of the properties it
supplies with each; a derived Resource type **MUST** declare which Subject types it admits. This module **MAY** provide
helpers other modules use to register the reusable, well-known Subject types sourced from the `SecurityContext` (e.g.
`user`, `tenant`).

- **Rationale**: Platform vendors author licensing rules against contracts, not against source code. A contract that
  exists only in a Gear's internals cannot be reviewed, approved, or relied upon.
- **Actors**: `cpt-cf-license-resolver-actor-consuming-module`, `cpt-cf-license-resolver-actor-platform-vendor`

#### Subject Identity

- [ ] `p1` - **ID**: `cpt-cf-license-resolver-fr-subject-identity`

The system **MUST** identify the subject of a check as an instance of a registered Subject licensing contract type
(see `cpt-cf-license-resolver-fr-contract-registration`). Within the contract object, the subject **MUST** carry the
**GTS type id of that contract** (always present) — from which the resolver resolves the contract schema — and **MAY**
carry an **id** — a well-known instance name or a UUID — plus `metadata` conforming to the contract schema. The set of
Subject contract types **MUST** be open-ended — a license may be granted to a tenant, a user, or any future subject
type — and the resolver **MUST NOT** assume the subject is a tenant.
This module owns the base Subject type. A module that wants license enforcement registers its derived Subject contract
type — alongside its Resource contract type (see `cpt-cf-license-resolver-fr-contract-registration`) — as a derivative
of the base type, projecting an externally-owned domain subject type. To ease reuse of the common subjects sourced from
the `SecurityContext` (e.g. `user`, `tenant`), this module **MAY** provide helpers other modules use to register those
well-known Subject types. The domain types themselves are owned by their modules and are neither defined nor
referenced here — a contract projects from them, it does not name them.

- **Rationale**: A GTS-typed, contract-carried subject identity is polymorphic and type-safe, supports licensing any
  subject kind, and makes the subject side of every check observable to platform vendors via the registered contract —
  while domain-type ownership stays with the modules that define those types.
- **Actors**: `cpt-cf-license-resolver-actor-consuming-module`

#### Resource Identity

- [ ] `p1` - **ID**: `cpt-cf-license-resolver-fr-resource-identity`

The system **MUST** identify the resource of a check as an instance of a registered Resource licensing contract type
(see `cpt-cf-license-resolver-fr-contract-registration`). Within the contract object, the resource **MUST** carry the
**GTS type id of that contract** (always present) — from which the resolver resolves the contract schema, validates
`metadata`, and reads `admitted_subjects` — and **MAY** carry an **instance id**, plus `metadata` conforming to the
contract schema:

- **Whole resource type** (no instance id) — the check asks whether the subject is entitled to resources of this type
  *as a class*, not to one concrete instance. The typical case is gating creation (e.g. a `POST`): the resource does
  not exist yet, so there is no instance id to pass. The contract only carries this type-level question; what a
  type-level grant means — and therefore how such a check is answered — is defined by the backend licensing service,
  not by the resolver.
- **Specific resource** (instance id present) — the instance id identifies one concrete resource: a stable well-known
  name (e.g. a named feature) or a UUID (e.g. a content item).

This module owns the base Resource type; the concrete (derived) Resource contract types — each projecting an
externally-owned domain resource type (e.g. a feature or content type) — are registered by the module performing
license enforcement for that resource. The domain types themselves are owned by their modules and are neither defined
nor referenced here — a contract projects from them, it does not name them. Which resource types are licensable, and
what a grant means for them, are owned by the backend licensing service that answers the check.

- **Rationale**: A required contract GTS type plus an optional instance id covers both check kinds — whole-class
  entitlement (e.g. on `POST`, before an id is known) and a specific resource — without sentinel values or flags. Both
  components are GTS concepts (a GTS type id; a well-known instance name or UUID), so the identity stays GTS-typed and
  validatable, appropriate for a cross-module contract.
- **Actors**: `cpt-cf-license-resolver-actor-consuming-module`

#### Contract Discoverability

- [ ] `p2` - **ID**: `cpt-cf-license-resolver-fr-contract-discoverability`

Platform vendors **MUST** be able to enumerate, for a given environment: which licensing contracts (Subject/Resource
type pairs) exist; which Subject types each Resource type admits; and the exact, versioned property schema of each
Subject and Resource type. This enumeration **MUST** be possible through the types registry alone, independently of
Gear source code, and licensing contracts **MUST** be identifiable as such (by derivation from the licensing base
types), so they can be reviewed in isolation from all other registered types.

- **Rationale**: Discoverability is what turns "the Gear sends some fields" into "the platform exposes a licensing
  surface" that can be controlled, reviewed, and approved.
- **Actors**: `cpt-cf-license-resolver-actor-platform-vendor`

#### Request Validation Against Registered Contracts

- [ ] `p1` - **ID**: `cpt-cf-license-resolver-fr-request-validation`

The system **MUST** validate every check request against the registered contracts *before* delegating to the backend
plugin, and **MUST** reject non-conforming requests with a validation error — fail-closed and distinct from a
not-granted decision. At minimum: Subject or Resource `metadata` not matching the registered schema of its declared
contract type → error; a missing contract type → error; a Subject contract type not admitted by the Resource contract
type → error. The resolver validates *shape and compatibility* only; it **MUST NOT** interpret what the properties mean
or decide licensability — those remain the backend's concern.

- **Rationale**: An invalid check silently evaluated is an unauditable check. Validation in the engine gives every
  plugin vendor the same guarantee — a request that reaches the plugin conforms to a published contract — instead of
  each backend re-implementing (or skipping) validation differently.
- **Actors**: `cpt-cf-license-resolver-actor-consuming-module`, `cpt-cf-license-resolver-actor-backend-plugin`

#### Contract Stability and Compatibility

- [ ] `p2` - **ID**: `cpt-cf-license-resolver-fr-contract-compatibility`

Licensing contracts **MUST** follow explicit compatibility rules: adding an **optional** property to a Subject/Resource
schema is non-breaking — plugins and platform vendors **MUST** ignore properties they do not use; removing or renaming
a property, changing its type, or narrowing the admitted Subject types is breaking and **MUST** be published as a new
contract version.

- **Rationale**: "Stable contract" must be a property, not an aspiration. The ignore-unknown-fields rule is also what
  keeps contracts flexible: a Gear may expose rich metadata, and each platform vendor consumes only the slice relevant
  to its licensing model.
- **Actors**: `cpt-cf-license-resolver-actor-consuming-module`, `cpt-cf-license-resolver-actor-platform-vendor`

#### Evaluation Metadata

- [ ] `p2` - **ID**: `cpt-cf-license-resolver-fr-evaluation-metadata`

The Subject and Resource contract objects **MAY** carry licensing-relevant properties (e.g. region, model name, user
category) in `metadata`, conforming to the registered schema of their contract type; the `metadata` object itself is
always present on the wire, possibly empty. The resolver **MUST** treat `metadata`
as semantically opaque — it **MUST NOT** interpret what any property means or require any particular property — but
**MUST** validate its shape against the registered contract schema (per
`cpt-cf-license-resolver-fr-request-validation`) and forward it unchanged to the selected backend plugin, which **MAY**
use it to express attribute/constraint-based licensing (e.g. "is this resource licensed to this subject in region
X?"). `metadata` is the contract's extension point: new properties arrive as new optional schema fields (non-breaking
per `cpt-cf-license-resolver-fr-contract-compatibility`) rather than undocumented keys.

- **Rationale**: Whether a resource is licensed can depend on contextual attributes, and which attributes matter is
  backend-specific and expected to grow. Schematizing them keeps the licensing surface observable and validatable while
  the resolver stays free of any business rules about what the attributes mean.
- **Actors**: `cpt-cf-license-resolver-actor-consuming-module`, `cpt-cf-license-resolver-actor-backend-plugin`

#### Plugin-Delegated Backend

- [ ] `p1` - **ID**: `cpt-cf-license-resolver-fr-plugin-delegation`

The system **MUST** delegate the grant lookup to a backend plugin discovered via the GTS types registry and selected by
vendor + priority; the resolver **MUST NOT** hold its own grant store.

- **Rationale**: Grant facts live in heterogeneous vendor backends; delegation keeps storage out of the resolver and
  allows backends to be added or swapped without caller changes.
- **Actors**: `cpt-cf-license-resolver-actor-backend-plugin`, `cpt-cf-license-resolver-actor-consuming-module`

#### Read-Only Contract

- [ ] `p1` - **ID**: `cpt-cf-license-resolver-fr-read-only`

The resolver and its public contract **MUST** be read-only: the contract **MUST** expose only the `is_licensed` check
and **MUST NOT** offer any operation to issue, revoke, bill, manage, or list/enumerate grants, and the resolver itself
**MUST NOT** hold a grant store. This constrains the resolver only — backend plugins **MAY** be backed by read-write
systems (e.g. issuance or billing); how a backend sources or maintains grants is outside the resolver's scope.

- **Rationale**: A read-only resolver contract keeps the module simple and authoritative as a check point and prevents
  scope creep into the issuance, billing, and catalog domains, while still allowing backends to be backed by mutable
  systems behind the delegation boundary.
- **Actors**: `cpt-cf-license-resolver-actor-consuming-module`

## 6. Non-Functional Requirements

> **Global baselines**: Project-wide NFRs defined
> in [docs/ARCHITECTURE_MANIFEST.md](../../../../docs/ARCHITECTURE_MANIFEST.md)
> and [guidelines/](../../../../guidelines/). Document only module-specific NFRs here.

### 6.1 Module-Specific NFRs

#### Read Latency

- [ ] `p1` - **ID**: `cpt-cf-license-resolver-nfr-read-latency`

The `is_licensed` check **MUST** complete within 50ms at p95, measured at the resolver boundary excluding backend plugin
processing time, under normal load.

- **Threshold**: 50ms p95 at the resolver boundary (excludes plugin compute), normal load.
- **Rationale**: The check sits on the access-granting path of consuming modules, so added latency directly impacts
  every gated request.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation for how this is realized.

#### Fail-Closed on No Plugin

- [ ] `p1` - **ID**: `cpt-cf-license-resolver-nfr-fail-closed`

When no backend plugin is available or the backend is unreachable, the resolver **MUST** fail closed — return a
non-granted decision or an error, and **MUST NOT** grant by default — in 100% of such cases.

- **Threshold**: 0 grant-by-default outcomes across all no-plugin / backend-unavailable conditions.
- **Rationale**: Granting access when the authority cannot be reached would be a license/security violation.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation for how this is realized.

#### Tenant Scoping

- [ ] `p1` - **ID**: `cpt-cf-license-resolver-nfr-tenant-scoping`

Every resolution **MUST** be scoped to the tenant carried by the request context (which the caller derives from its
`SecurityContext`), with 0 cross-tenant grant leaks tolerated. Regardless of subject type, the subject is treated as
bounded within that tenant (the current tenant-bounded model — see §11).

- **Threshold**: 0 cross-tenant resolutions; tenant scope derived solely from the request context.
- **Rationale**: Under the current model every license is tenant-bounded (a user belongs to a tenant), so the resolver
  enforces tenant isolation like other CF modules; a cross-tenant grant would expose another tenant's entitlements.
- **Architecture Allocation**: See DESIGN.md § NFR Allocation for how this is realized.

### 6.2 NFR Exclusions

- Horizontal write-scalability NFRs: N/A — the module performs no writes.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### License Resolver Client

- [ ] `p1` - **ID**: `cpt-cf-license-resolver-interface-client`

- **Type**: Rust trait (`LicenseResolverClient`)
- **Stability**: stable
- **Description**: The public client contract exposing the **single** method
  `is_licensed(request: LicenseCheckRequest) -> LicenseDecision` — the point-in-time check of whether a resource is
  licensed to a subject. `LicenseCheckRequest` bundles the Subject and Resource contract objects (instances of
  registered derived licensing types, each carrying its contract type + optional id + schematized `metadata`) and the
  tenant context (caller-derived from `SecurityContext`). A request that does not conform to its registered contracts is
  rejected with a validation error, distinct from a not-granted decision. There is no listing or enumeration method.
- **Breaking Change Policy**: Major version bump required to change the `LicenseCheckRequest`/`LicenseDecision` shape (a
  backward-compatible new optional request field is not breaking), the identity model (contract GTS type + optional
  instance id), or the decision/error semantics. Individual licensing contracts evolve under
  `cpt-cf-license-resolver-fr-contract-compatibility` (additive optional properties are non-breaking).

### 7.2 External Integration Contracts

#### Backend Plugin Contract

- [ ] `p2` - **ID**: `cpt-cf-license-resolver-contract-plugin`

- **Direction**: required from backend plugin
- **Protocol/Format**: Plugin trait (`LicenseResolverPluginClient`) mirroring the `is_licensed` signature, discovered
  via the GTS types registry plugin spec.
- **Compatibility**: Plugin spec is GTS-versioned; the plugin contract tracks the public client contract's major
  version.

## 8. Use Cases

### Gate Access to a Licensable Resource

- [ ] `p2` - **ID**: `cpt-cf-license-resolver-usecase-gate-access`

**Actor**: `cpt-cf-license-resolver-actor-consuming-module`

**Preconditions**:

- A `SecurityContext` with a tenant is available.
- The Gear's derived Subject/Resource licensing contract types are registered in the types registry.

**Main Flow**:

1. Consuming module builds the Subject and Resource contract objects as instances of the registered contract types,
   projecting the licensing-relevant fields from the data it has at hand (e.g. the resource from its domain objects,
   the subject from `SecurityContext`): each carries its contract GTS type (required), an optional id (a well-known
   name or UUID; omitted to check a whole resource type), and `metadata` conforming to the contract schema.
2. Module assembles a `LicenseCheckRequest` with the two contract objects and the tenant context derived from its
   `SecurityContext`, and calls `is_licensed(request)`.
3. Resolver validates the request against the registered contracts (schemas + admitted subject types).
4. Resolver selects the backend plugin via the GTS registry and delegates the check, forwarding the request unchanged.
5. Resolver returns a decision (granted true/false plus diagnostics).
6. Module enforces the decision.

**Postconditions**:

- The caller has an authoritative, tenant-scoped grant decision; no state was changed.

**Alternative Flows**:

- **Request does not conform to its registered contracts** (schema mismatch, missing contract type, subject type not
  admitted): Resolver rejects with a validation error before delegation — fail-closed, never evaluated; the module
  denies access.
- **No plugin available / backend unreachable**: Resolver fails closed — returns not-granted or an error; the module
  denies access.

## 9. Acceptance Criteria

- [ ] `is_licensed(request)` returns a correct granted/not-granted decision for whole-type, named, and opaque
  resources.
- [ ] Non-conforming requests (schema mismatch, missing contract type, inadmissible subject type) are rejected with a
  validation error before delegation — never silently evaluated, never granted.
- [ ] The platform's licensing surface is enumerable from the types registry alone (all types derived from the
  licensing base types), with per-contract property schemas and admitted subject types.
- [ ] No listing or enumeration capability exists in the public contract.
- [ ] When no backend plugin is available, the resolver never grants by default.
- [ ] All resolutions are tenant-scoped via the request context (derived from `SecurityContext`) with no cross-tenant
  leakage.
- [ ] Backend selection is performed by GTS registry discovery (vendor + priority) with no resolver-owned grant store.

## 10. Dependencies

| Dependency                            | Description                                                                                              | Criticality |
|---------------------------------------|----------------------------------------------------------------------------------------------------------|-------------|
| GTS types registry (`types-registry`) | Backend plugin discovery (by GTS plugin spec); registration and schema resolution of licensing contracts | p1          |
| `SecurityContext`                     | Source of the request's tenant context (built by the caller)                                             | p1          |
| Backend license plugin                | Holds grant facts and answers the delegated check                                                        | p1          |

## 11. Assumptions

- The domain types a licensing contract projects from (subject and resource) are owned and registered by their
  respective modules; the contract carries only the projected `metadata`, not the domain type itself. Linking a
  contract to its domain type is deferred (ADR-0001).
- The licensing base types are owned by this module under `gts.cf.core.lic.*`; a module that wants license enforcement
  registers its derived Subject and Resource contract types before checks are made.
- This module **MAY**, at some stage, provide a set of helpers that other modules use to register the reusable,
  well-known Subject contract types sourced from the `SecurityContext` (e.g. `user`, `tenant`), so common subjects need
  not be re-declared by each module.
- At least one backend plugin is registered in environments where license checks are expected to grant.
- Contract schemas are resolvable from the types registry and cacheable; the registry is available at bootstrap and its
  schema reads are not on the per-check hot path (cached).
- **Tenant-bounded grants (current model)**: at this stage, every license is assumed to be bounded within a single
  tenant — regardless of subject type, the subject (e.g. a user) belongs to a tenant — and the resolver enforces tenant
  isolation via the request's tenant context (derived from `SecurityContext`) as other CF modules do. Cross-tenant or
  tenant-independent licensing is not
  modeled yet; lifting this assumption would be a future, explicitly-versioned contract change.

## 12. Risks

| Risk                                            | Impact                                                  | Mitigation                                                                                                                                    |
|-------------------------------------------------|---------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------|
| No backend plugin registered in an environment  | All checks deny, blocking legitimate access             | Fail-closed by design; surface a clear `NoPluginAvailable` signal for operability.                                                            |
| Backend latency degrades the check              | Slows every gated request in consuming modules          | Boundary p95 NFR; consuming modules may apply their own timeouts/fallbacks.                                                                   |
| Misreferenced contract type path                | Check resolves against the wrong contract               | Engine validates the request against the registered contract; the backend licensing service validates licensability and denies unknown types. |
| Contract drift (Gear code vs registered schema) | Checks start failing validation after a Gear change     | Engine-side validation surfaces drift immediately and fail-closed; additive-optional evolution is non-breaking by rule.                       |
| Contract governance overhead                    | New licensable surfaces require registration and review | The review point is the feature; per-Gear contract count is small and bounded by its distinct licensable surfaces.                            |

## 13. Open Questions

- Should `LicenseDecision` carry minimal grant metadata (e.g. status/expiry) beyond the boolean? — Owner:
  license-resolver maintainers; target: DESIGN phase (2026-06-30).
- What diagnostics keys/conventions do consuming modules need (denial cause, matched grant, backend id, etc.)? — Owner:
  license-resolver maintainers; target: DESIGN phase (2026-06-30).
- How do callers source deployment-wide contract `metadata` properties (e.g. region, environment) that are fixed per
  deployment rather than known per call? Candidate: an SDK-level wrapper merging configuration-sourced properties into
  the contract objects (conforming to the registered schema) before delegation; merge precedence (caller-provided vs
  configuration-sourced) to be settled. — Owner: license-resolver maintainers; target: DESIGN phase (2026-06-30).

## 14. Traceability

Links to related specification artifacts.

- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: [ADR/](./ADR/)
- **Features**: [features/](./features/)
