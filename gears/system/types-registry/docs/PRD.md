# PRD - Types Registry

> Checklist `p1`/`p2` values are specification-item priorities inherited from the PRD template, not product delivery phases. Product P1 comprises every capability not explicitly assigned to P2 or post-P1 in the requirement prose; Product P2 adds the capabilities explicitly assigned there.

## Table of Contents

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
  - [5.1 Registry Core](#51-registry-core)
  - [5.2 References, Aliases, And Queries](#52-references-aliases-and-queries)
  - [5.3 Ownership, Lifecycle, And Caching](#53-ownership-lifecycle-and-caching)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Gear-Specific NFRs](#61-gear-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
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
- [15. References](#15-references)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

Types Registry is the central platform registry for type contracts used by gears to communicate, exchange typed data, discover capabilities, and extend platform functionality. It gives gears one shared authority for type identity, schema validation, derivation compatibility, lifecycle, discovery, resolving between user-facing type identifiers and machine-readable registry references, and — from P2 — type casting/conversion and Aliases.

Types Registry governs contract admission and lifecycle metadata, while owning gears remain responsible for runtime object storage and business behavior.

### 1.2 Background / Problem Statement

The platform currently needs shared type contracts for gear contracts, configuration, plugin discovery, and typed references between domain objects. Without a central registry, each gear would need to duplicate schema management, version compatibility, type derivation compatibility checks, type casting/conversion, future Alias resolution, tenant/global ownership, lifecycle rules, and cache invalidation.

Some vendors may already have an existing type registry or contract catalog that remains the source of truth for their contracts. Types Registry must still provide one platform-facing control plane for gears, while allowing selected registry entities to be resolved and queried live through vendor Registry Source Plugins without replicating those entities into Types Registry storage.

Industry systems solve adjacent parts of this problem separately. Kubernetes CRDs, Azure Resource Providers, and AWS CloudFormation Registry cover controlled resource-type registration. Confluent Schema Registry, AWS Glue Schema Registry, Azure Event Hubs Schema Registry, and Google Pub/Sub Schemas cover schema compatibility and client lookup. Dataverse metadata covers tenant-facing metadata customization. Types Registry combines these patterns for the platform's type-contract control plane.

The canonical representation of registry contracts is based on [Global Type System](https://github.com/globaltypesystem/gts-spec) (GTS) Types, GTS Type Schemas, and registered GTS Instances.

### 1.3 Goals (Business Outcomes)

- Provide one governed registry for platform type contracts instead of bespoke per-gear type-registration mechanisms.
- Allow gears to use stable machine-readable type references while preserving user-facing GTS Identifiers and, in P2, Aliases.
- Enable safe type evolution through compatibility checks, lifecycle state, dependency awareness, and P2 casting.
- Support global platform types and tenant-owned custom types with predictable ownership and visibility rules.
- Federate local and external registry sources behind one platform-facing registry contract.
- Make registry lookups cacheable for SDK clients without sacrificing correctness in multi-pod deployments.

### 1.4 Glossary

| Term | Definition |
|------|------------|
| GTS | Global Type System, the specification for globally unique versioned type identities and JSON Schema-based definitions. |
| GTS Type | Entity identified by a GTS Type Identifier and defined by a GTS Type Schema. |
| GTS Type Identifier | Canonical GTS Identifier ending with `~`. |
| GTS Type Schema | JSON Schema document annotated with GTS keywords and defining a GTS Type's shape, traits, and derivation. |
| JSON Schema Dialect | Draft declared by a Type Schema's top-level `$schema`; the managed profile is defined by `cpt-cf-types-registry-fr-gts-validation`. |
| Resolution Closure | Documents inlined into a Type Schema's effective form: its base chain and reachable `$ref` targets, including those in `x-gts-traits-schema`. It excludes `x-gts-ref`, whose target is never inlined. |
| Availability Closure | Managed Entities reachable from a subject through outgoing availability-blocking relationships, including the subject itself. An `x-gts-ref` is not such a relationship. |
| GTS Instance | Concrete value or document conforming to a GTS Type. |
| GTS Instance Identifier | Canonical GTS Identifier without a trailing `~`, naming a well-known Instance. |
| GTS Identifier | Canonical user-facing identifier of a GTS Type or Instance. |
| GTS Identifier Region | Set of identifiers matched by one GTS pattern, used by registration policy, grants, and Source Claims; because the wildcard is trailing, any two matching regions are nested or disjoint. |
| Type Schema Evolution Compatibility | Accepted-instance-set relation between successive definitions within one Type Schema major; rules: `cpt-cf-types-registry-fr-validate-schema-compat`. |
| Type Derivation Compatibility | Requirement that a derived Type Schema accept only instances valid against every base in its chain. |
| Version Family | Logical entities related by version succession, named by removing the last segment's complete version from the canonical identifier; succession never crosses derivation chains. |
| Version Successor | Distinct logical entity with a higher version in the same Version Family, not a content revision of one entity. |
| Minor-Bearing Major | Major whose logical members carry minor versions and are immutable; rules: `cpt-cf-types-registry-fr-minor-version-profile`. |
| Unstable Type Schema | Managed Type Schema whose own last segment has major 0; evolution compatibility is unenforced and quarantine rules apply. |
| Registry Reference | Client-opaque, platform-deterministic UUID (`gts_uuid`) returned for one exact GTS Identifier and persisted by domain gears as its type reference. |
| Concrete Reference Set | Deduplicated, bounded Registry Reference set obtained by exhausting one traversal of a type filter; it is traversal-complete, not an atomic snapshot. |
| Alias | P2 Managed Entity providing an alternate GTS Identifier for a Managed Type Schema or registered Instance. |
| Owning Gear | Gear responsible for runtime data and behavior using a registered type. |
| Validation Hook | P2 owning-gear contract for semantic validation of managed admission, revision, or deletion. |
| Admission Candidate | Proposed initial definition or content update being validated; it is not an admitted logical entity or revision. |
| Admission Status | Per-candidate state: `pending`, `running`, `succeeded`, `unchanged`, or `failed`; it has no separate resource. Operation Status reports only `pending`, `running`, or `completed`, where `completed` means every candidate is terminal. |
| Dry Run | Mode executing a mutation's checks and diagnostics without committing its effects. |
| Registry Federation | One Types Registry contract backed by managed storage and External Registry Sources. |
| Registry Source | Authoritative provider of registry entities: managed storage or an External Registry Source. |
| External Registry Source | Registry or catalog outside Types Registry that remains authoritative for its entities. |
| Registry Source Plugin | Governed read-only plugin through which Types Registry queries an External Registry Source. |
| Source Claim | Rooted single-segment GTS wildcard declaring the non-overlapping identifier space served by one plugin. |
| External Revision | Opaque source freshness token; equal revisions identify equal canonical content and content hash. |
| Managed Entity | Entity for which Types Registry is the source of truth. |
| Externally Managed Entity | Entity obtained live from an External Registry Source while Types Registry applies platform visibility and usage semantics. |
| Tenant Subtree | Tenant and all of its descendants in the platform hierarchy. |
| Context Tenant | Tenant scope root used for availability and caller-relative ownership evaluation; it may differ from the requesting subject, whose context still governs visibility. |
| Lifecycle Status | Entity state: `ACTIVE` or terminal `DELETED` in P1. |
| Resource Version | Monotonic logical-entity state token used for optimistic mutation preconditions; distinct from a content revision and from a read validator. |
| Tenant Enablement State | Tenant policy input: `NOT_INITIALIZED`, `ENABLED`, or `DISABLED`, with no reason or expiry. |
| Tenant Availability State | Consumer-facing `AVAILABLE` or reasoned `UNAVAILABLE` verdict for an entity and Context Tenant. |
## 2. Actors

### 2.1 Human Actors

#### XaaS Vendor Architect

**ID**: `cpt-cf-types-registry-actor-xaas-vendor-architect`

- **Role**: Chooses how Gears are composed into a vendor product and defines derived GTS Types for existing platform and domain Constructor Fabric Gears.
- **Needs**: Governed registration and lifecycle management for product-level derived Types without forked per-gear mechanisms.

#### Gears Developer

**ID**: `cpt-cf-types-registry-actor-gears-developer`

- **Role**: Develops platform and domain Gears; defines their base GTS Types, Type Schemas, and registered Instances, and may define derived Types from Types registered by other Gears.
- **Needs**: Safe registration, compatibility checks, dependency awareness, lifecycle management, and predictable startup behavior.

#### XaaS Vendor Developer

**ID**: `cpt-cf-types-registry-actor-xaas-vendor-developer`

- **Role**: Develops vendor-specific Gears and defines their base GTS Types, Type Schemas, and registered Instances.
- **Needs**: Safe registration, compatibility checks, dependency awareness, lifecycle management, and predictable startup behavior for vendor-specific Gears.

#### Tenant Administrator

**ID**: `cpt-cf-types-registry-actor-tenant-admin`

- **Role**: Manages tenant-owned custom types and, in P2, Aliases exposed through authenticated platform APIs.
- **Needs**: Tenant-scoped type management, discovery of global and tenant-visible types, and protection from cross-tenant changes.

### 2.2 System Actors

#### Platform Gear

**ID**: `cpt-cf-types-registry-actor-platform-gear`

- **Role**: Registers platform Type Schemas and Instances during initialization and resolves registry references at runtime.

#### Domain Gear

**ID**: `cpt-cf-types-registry-actor-domain-gear`

- **Role**: Owns runtime domain objects that refer to registered types and uses Types Registry for resolving, discovery, and query assistance.

#### Registry Source Plugin

**ID**: `cpt-cf-types-registry-actor-registry-source-plugin`

- **Role**: Provides live forward/reverse resolution, querying, caching, revision metadata, lifecycle assertions, and tenant state for an External Registry Source through a platform-governed plugin contract. The contract is read-only with respect to Types Registry state.

#### CI Pipeline

**ID**: `cpt-cf-types-registry-actor-ci-pipeline`

- **Role**: Validates type compatibility, dependency impact, and registry changes before deployment.

## 3. Operational Concept & Environment

Runtime, gear architecture, and project-wide quality baselines follow the repository foundations:

- [docs/ARCHITECTURE_MANIFEST.md](../../../../docs/ARCHITECTURE_MANIFEST.md)
- [guidelines/README.md](../../../../guidelines/README.md)
- [docs/toolkit_unified_system/README.md](../../../../docs/toolkit_unified_system/README.md)

### 3.1 Gear-Specific Environment Constraints

- Managed registry state and Registry Source Plugin configuration must be persistent and consistent across multi-pod deployments. External registry state remains plugin-owned; process-local state and client caches are allowed only as derived cache state.
- Admitted revisions are retained without a time limit. The only operation that physically removes them also releases the GTS Identifier and is disabled by default in every deployment; while it remains disabled in production, admitted content there is effectively unremovable (ADR-0013).
- Data classification, and any resulting limit on what may be placed in a registered Type Schema or Instance value, is platform-wide policy. Types Registry applies no content policy of its own.

## 4. Scope

### 4.1 In Scope

- GTS Type Schema registration, retrieval, search, lifecycle, Type Schema Evolution Compatibility checks, and Type Derivation Compatibility checks.
- GTS Instance registration, retrieval, search, lifecycle, and validation, plus P2 casting.
- P2 owning-gear semantic validation hooks for initial admission, content revisions, and deletion of Managed Type Schemas and registered Instances.
- Registry federation and live support for externally managed entities through ordered Registry Source Plugins, including platform-owned federation boundary enforcement, forward/reverse resolving, querying, source-owned caching, revision metadata, lifecycle assertions, and tenant state.
- P2 Alias management and alias-aware resolving.
- Stable registry reference support for domain gears.
- Tenant/global ownership, visibility, and management boundaries.
- Lifecycle status, post-P1 tenant enablement state, and computed tenant availability state for registry entities.
- Dependency tracking for GTS and JSON Schema references.
- `gts-rust` integration for GTS parsing, validation, reference derivation, wildcard matching, compatibility, casting, and schema generation/conversion capabilities required by registry workflows.
- SDK and REST contracts for registry management, resolving, validation, discovery, and P2 casting.
- Client-side cache correctness protocol.

### 4.2 Out of Scope

- Runtime domain-object storage and business behavior owned by other Gears, except explicitly registered well-known GTS Instances.
- Read and query policy for existing runtime domain objects whose referenced registry entity becomes unavailable; this policy is owned by the respective Domain Gear.
- Authoritative management of external registry sources that remain outside the platform's ownership boundary.
- GTS namespace governance outside registration-time validation and conflict detection.
- A general-purpose business audit product. Types Registry retains admitted content revisions and emits operation/audit records for registry mutations as required by its revision and lifecycle model; it does not provide platform-wide audit query, retention, or export capabilities.
- Local projection, synchronization, indexing, revision history, or caching of Externally Managed Entity content inside Types Registry.

## 5. Functional Requirements

> **Testing strategy**: Functional requirements are verified through automated unit, integration, and end-to-end tests in accordance with the repository testing architecture, targeting 90%+ code coverage unless a requirement specifies another verification method.

Functional requirements define what Types Registry must provide. Design details such as DB tables, route paths, cache transport, and exact SDK or REST DTOs are intentionally outside this PRD and will be specified in the Types Registry DESIGN document and, where appropriate, ADRs.

### 5.1 Registry Core

#### Type Schema Management

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-register-schemas`

The system **MUST** allow authorized actors to register, retrieve, search, update lifecycle state for, and delete GTS Type Schemas, subject to validation, content-profile, ownership, dependency, and compatibility rules. The content profile of a Managed Type Schema includes its JSON Schema Dialect, restricted by `cpt-cf-types-registry-fr-gts-validation`.

- **Rationale**: Gears need one authoritative registry for type contracts.
- **Actors**: `cpt-cf-types-registry-actor-platform-gear`, `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-xaas-vendor-architect`, `cpt-cf-types-registry-actor-xaas-vendor-developer`, `cpt-cf-types-registry-actor-tenant-admin`

#### Instance Management

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-register-instances`

The system **MUST** allow authorized actors to register, retrieve, search, update lifecycle state for, and delete named GTS Instances that conform to registered Type Schemas. Admission **MUST** validate an Instance against the current revision of a visible, `ACTIVE`, tenant-available Type Schema and retain the exact schema revision that validated it.

Each admitted Instance revision **MUST** retain its admission-time validating Type Schema revision as immutable provenance. When a later Type Schema revision revalidates the unchanged current Instance value, the registry **MUST** record that current revalidation separately and **MUST NOT** rewrite the provenance of any admitted Instance revision.

A registered Instance identifier **MUST NOT** carry a minor version in its last segment, even where the Type Schema it conforms to carries one. Nothing is lost by that: an Instance of a minor-versioned Type Schema carries the minor in a preceding segment, and only its own last segment is constrained.

A registered Instance **MUST NOT** conform to an unstable Type Schema. ADR-0006 forbids a schema revision from becoming current while an affected registered Instance would cease to be valid; applied to an unstable schema that rule would restore exactly the block the profile exists to remove, and waived it would leave admitted Instances failing validation against their own current schema while the registry records a revalidation that no longer holds. Refusing the combination is what keeps both records truthful, and its cost — a control-plane type and its Instances cannot be developed together under the profile — is accepted rather than worked around.

- **Rationale**: Platform gears need registered well-known instances for configuration and discovery metadata.
- **Actors**: `cpt-cf-types-registry-actor-platform-gear`, `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-xaas-vendor-developer`, `cpt-cf-types-registry-actor-tenant-admin`

#### GTS Validation

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-gts-validation`

For Managed Entities and explicit platform validation operations, the system **MUST** validate GTS Identifiers, Type Schemas, Instances, references, wildcard patterns, and version semantics using the platform-approved GTS implementation. For Externally Managed Entities this applies only to identifier and response-envelope conformance; Types Registry **MUST NOT** reproduce source-owned entity validation.

The managed identifier profile **MUST** enforce these unconditional restrictions:

| Restriction | Applies to | Governing decision |
|---|---|---|
| No explicit UUID tail | every managed identifier | ADR-0001 |
| Minor version admissible under every prefix | managed Type Schema | `cpt-cf-types-registry-fr-minor-version-profile`, ADR-0004 |
| No minor version in the last segment | managed registered Instance | ADR-0004 |
| No major version 0 in the last segment | managed registered Instance | ADR-0015 |

An Instance of a minor-versioned Type Schema may carry the minor in a preceding segment. A managed Type Schema may carry major 0 in its last segment, subject to ADR-0015 and every non-exempt admission check. No configuration, grant, or payload field relaxes these identity rules; registration policy may govern who may register a valid identifier, but not whether it is valid. They do not apply to source-owned external identifiers.

A managed Type Schema **MUST** declare top-level `$schema` as an accepted Draft-07 spelling; nested `$schema` values **MUST** be absent or normalize to the same dialect. The dialect is pinned at initial admission and **MUST NOT** change across revisions of the logical entity. Types Registry **MUST NOT** infer a missing dialect from validator defaults. Any post-P1 widening **MUST** preserve dialect uniformity across the Resolution Closure; `x-gts-ref` targets are excluded because they are not inlined. Types Registry **MUST NOT** inspect `$schema` in external content.

- **Rationale**: Registry behavior must match the GTS specification and avoid divergent local interpretations. Where the specification leaves a question open, the platform narrows its own managed profile instead of inventing an answer. ADR-0014 (dialect), ADR-0001 (UUID tail), ADR-0004 and ADR-0015 (version markers).
- **Actors**: `cpt-cf-types-registry-actor-platform-gear`, `cpt-cf-types-registry-actor-ci-pipeline`

#### Minor Version Profile

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-minor-version-profile`

The system **MUST** admit a minor version in the last segment of a managed Type Schema identifier under every prefix. Eligibility follows from the candidate identifier alone; no configuration, grant, payload field, or GTS Identifier Region changes it.

Each major takes exactly one shape, fixed by its first admitted member:

| Shape | Content evolution |
|---|---|
| Major-only, for example `v1~` | one mutable logical entity with revisions |
| Minor-bearing, for example `v2.0~` | immutable logical entities; publish change as the next minor |

The choice is per major, not per Version Family, because compatibility chains do not cross majors; both shapes may therefore coexist in different majors of one family. An admitted minor's authored content **MUST NOT** change, although its effective form may change through floating dependencies and the entity may be deleted; the system **MUST NOT** describe this as closure pinning.

The minors of one major **MUST** be the contiguous set `{0..k}`. Admission opens at `M.0`; `M.n` for `n > 0` requires admitted predecessor `M.(n-1)`, `ACTIVE` or `DELETED`. If that predecessor is absent at commit, admission **MUST** fail retryably. Purge **MUST** remove only a suffix of the sequence and refuse while a higher minor remains, naming it. Consequently the comparison baseline is fixed by the candidate identifier and cannot be superseded by a concurrent admission.

The system **MUST** support a per-candidate `force` waiver for the cross-minor compatibility check of `cpt-cf-types-registry-fr-validate-schema-compat`, and for no other check. It **MUST NOT** waive derivation compatibility, dialect and identifier profiles, unstable quarantine, contiguity, reference resolvability, or any other check. It **MUST** be refused, not ignored, for a major-only candidate, `M.0`, or a major-0 candidate; these cases are determined from the identifier alone. No equivalent waiver exists for a revision of a major-only entity: a new minor is unreferenced and withdraws only an upgrade statement, while revising a floating identity could break existing consumers.

The waiver **MUST** be disabled by default and governed by one deployment-wide, non-region-scoped configuration value. Disabled `force` requests **MUST** fail equally in Dry Run and real admission, with a reason identifying the configuration; disabling it later does not retract admitted waivers. Because replicas read the value at process start, rolling restarts may temporarily yield different decisions.

A forced admission **MUST** record the waiver and expose it on read. The flag describes the edge entering that minor: an upgrade from `s` to `t` is compatibility-established only if none of `s+1 … t` carries it.

`$ref` and derivation-base references **MUST NOT** cross a minor boundary. Admitting one minor **MUST NOT** revalidate, recompute, or invalidate entities of another minor, and resolving a major-only identifier **MUST NOT** select its highest minor. `x-gts-ref` is not resolution-bearing and imposes no minor-boundary restriction on the payload identifiers it accepts.

Platform-declared schemas and Instances under `gts.cf.*` **MUST** be major-only. An architecture lint over their declaring source, not Types Registry admission, enforces that rule; the registry **MUST NOT** reserve any prefix against minor-bearing schemas.

- **Rationale**: A major-only identifier gives an owner no way to publish a compatible successor without applying it to every dependent at once, while a new major expresses non-adoption only by discarding the compatibility statement; a minor supplies both. Major-only stays the recommendation and is the rule for platform contracts, kept by a lint over their source rather than by the registry. ADR-0004 records the alternatives, the concurrency argument behind contiguity, and the deployment configuration this requirement deliberately does not have.
- **Actors**: `cpt-cf-types-registry-actor-xaas-vendor-architect`, `cpt-cf-types-registry-actor-xaas-vendor-developer`, `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-platform-gear`

#### Type Schema Evolution Compatibility Checks

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-validate-schema-compat`

The system **MUST** check a managed Type Schema candidate against its baseline under the platform-enforced backward-compatibility mode and fail closed when compatibility is violated or cannot be established. The candidate identifier determines the baseline and waiver eligibility:

| Candidate | Baseline | Waivable |
|---|---|---|
| First admission of a major-only entity | none | n/a |
| Content revision of a major-only entity | that entity's own current revision | **never** |
| `M.0`, opening a Minor-Bearing Major | none | n/a |
| `M.n`, `n > 0` | the definition of `M.(n-1)`, `ACTIVE` or `DELETED` | by `force` |
| Candidate whose own last segment carries major 0 | none | n/a |

Where no baseline exists, no comparison or pass verdict exists. A revision of a major-only entity is never waivable; only the cross-minor check may be waived through `force` as defined by `cpt-cf-types-registry-fr-minor-version-profile`.

For a stable, unforced chain evaluated under one compatibility semantics, the highest minor of a major **MUST** accept every instance accepted anywhere earlier in that major. A major-0 Type Schema is exempt only from this evolution check: major-only `v0~` revisions and the next contiguous `v0.n~` **MUST** be admitted without a compatibility verdict, while derivation compatibility, dependent revalidation, dialect, reference, lifecycle, ownership, and authority rules remain in force.

**Quarantine.** A managed Type Schema whose own last segment carries major 1 or higher **MUST NOT** reference or derive from a major-0 entity through `$ref` or its immediate derivation base. The reverse direction is allowed.

`x-gts-ref` is outside the quarantine because it validates an Instance value without resolving or inlining the named entity. It creates no dependency or guarantee over that entity, as defined by `cpt-cf-types-registry-fr-ref-tracking`.

Only rejection reports compatibility: it **MUST** carry structured diagnostics naming the cause and offending schema location. Successful admission and ordinary reads **MUST NOT** expose a compatibility verdict, mode, or per-level evolvability. Forward-direction results may appear only as `p3` advisory diagnostics, and operational claims about producers, readers, casting, or default materialization **MUST NOT** be presented as schema compatibility.

- **Rationale**: In-place evolution must not silently break producers, consumers, or historical payload processing. A contract still being designed is the exception, and marking it in the identifier makes the risk legible while the quarantine rule keeps it with the owners who accepted it. ADR-0003 and ADR-0015 record the alternatives.
- **Actors**: `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-xaas-vendor-architect`, `cpt-cf-types-registry-actor-xaas-vendor-developer`, `cpt-cf-types-registry-actor-ci-pipeline`

#### Type Derivation Compatibility Checks

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-validate-type-derivation`

The system **MUST** check every derived GTS Type Schema against its immediate base Type Schema and the complete transitive base-type chain. Every instance valid against the derived Type Schema **MUST** remain valid against every base Type Schema in that chain. Admission **MUST** reject derivations that violate base constraints or applicable GTS derivation, finality, and inherited-trait rules.

- **Rationale**: A derived GTS Type must remain safely substitutable for every base Type declared by its GTS identifier chain, independently of compatibility between revisions of any one Type Schema.
- **Actors**: `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-xaas-vendor-architect`, `cpt-cf-types-registry-actor-xaas-vendor-developer`, `cpt-cf-types-registry-actor-ci-pipeline`

#### Dependency Awareness

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-ref-tracking`

The system **MUST** track dependencies between Managed Entities: `$ref` targets, an entity's immediate derivation base, and an Instance's conforming Type Schema. An `x-gts-ref` **MUST NOT** create a dependency, however concretely it names an entity.

Before a managed Type Schema revision becomes current, the system **MUST** revalidate every affected registered dependent in its transitive reverse dependency closure, including current registered Instances, and reject the candidate if any would cease to satisfy its conformance, derivation, or reference rules. It **MUST NOT** rewrite dependent references or publish replacement dependents automatically.

Consequently, the system **MUST** permit deletion of an entity named only by `x-gts-ref` and **MUST NOT** revalidate the schema when that entity changes or is deleted. For the managed–external boundary, admission **MUST** classify each value as follows without storing a relation:

| `x-gts-ref` value | Identifier used for authority classification |
|---|---|
| a literal whole identifier | the exact identifier |
| a literal prefix or wildcard | the longest prefix of itself that is a valid identifier |
| `gts.*`, or a relative JSON pointer such as `/$id` | none |

The open match set of a pattern **MUST NOT** be treated as named or re-expanded when a new entity is admitted.

Under ADR-0011 every tracked dependency has a Managed Entity at both ends, so the tracked set is authoritative for deletion safety and that decision is reached from local state without plugin availability, plugin cooperation, or plugin-supplied data. No plugin operation contributes to that set, and none is asked to.

Types Registry **MUST NOT** expose a client-facing operation for enumerating dependents. What a caller needs — whether a deletion or a revision would be refused, and by what — is answered by the Dry Run of that same mutation.

Any visible and tenant-available entity **MUST** remain a valid target for existing and new `$ref` and derivation references. Deletion and the quarantine rule remove it from that set. `x-gts-ref` is not resolution-bearing and makes no target-validity promise. P1 has no lifecycle status between `ACTIVE` and `DELETED`.

- **Rationale**: Platform teams need predictable blast-radius analysis for type changes.
- **Actors**: `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-xaas-vendor-architect`, `cpt-cf-types-registry-actor-xaas-vendor-developer`, `cpt-cf-types-registry-actor-ci-pipeline`

#### Registry Federation

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-registry-federation`

The system **MUST** support multiple Registry Sources, including Types Registry's own managed storage and External Registry Sources integrated through governed Registry Source Plugins. Types Registry **MUST NOT** persist external entity definitions, identifiers, revisions, content hashes, lifecycle state, Registry Reference mappings, query indexes, caches, or tombstones, and the owning plugin **MUST** serve that state live through the Types Registry federation contract. Under ADR-0011 this prohibition has no exception, and Registry Source Plugins **MUST NOT** have any write path into Types Registry state.

- **Rationale**: Vendor products may already have authoritative type registries, but platform gears still need one Types Registry contract for resolving, discovery, and platform governance.
- **Actors**: `cpt-cf-types-registry-actor-xaas-vendor-architect`, `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-registry-source-plugin`

#### Registry Source Routing

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-registry-source-routing`

Each Registry Source Plugin **MUST** declare one or more validated Source Claims and a deterministic priority. A Source Claim is a GTS Identifier pattern and nothing more: the trailing `~` of an identifier already determines its kind and overlap is checked over the identifier space regardless of kind, so a claim declaring an entity-kind restriction **MUST** be rejected rather than narrowed to the kinds it names. A claim **MUST** be one rooted GTS segment with a wildcard at a token boundary, from `gts.<vendor>.*` through `gts.<vendor>.<package>.<namespace>.<type>.*`; activation **MUST** reject multi-segment patterns. The matching rooted claim prefix therefore selects the source and keeps the identifier's whole derivation chain within it.

The Types Registry federation contract is total: across its whole claimed identifier space an active P1 plugin **MUST** implement all of it, with no optional or advisory part and nothing separately declared or negotiated. The contract requires:

- batch forward and reverse resolution, with reverse resolution retained after deletion;
- complete bounded candidate queries with opaque pagination;
- lifecycle, ownership/visibility, and tenant-state assertions;
- revision/hash and conditional-read semantics; and
- structured source failures.

For a Type Schema result — an identifier with a trailing `~` — it **MUST** also return resolved effective schema and trait artifacts. A claim covers both entity kinds in its space, so a source holding none of one kind **MUST** report that kind's identifiers absent exactly as it reports any other absent identifier, with no separate outcome for an unheld kind. These obligations are mandatory and authoritative; dependency registration and reverse-impact lookup are absent from the contract under the closed boundary.

Candidate queries **MUST NOT** have false negatives; Types Registry **MUST** accept a broader candidate set and apply normalized platform filtering. A source response that is non-conforming or incomplete for the identifier it returns **MUST** be rejected rather than interpreted, and the affected request **MUST** fail closed; conformance is therefore established on every response and by plugin conformance tests, never by an activation-time check against a plugin's own declaration.

P1 Source Claims **MUST NOT** overlap one another or managed identifier space, including by nesting a Managed Entity beneath a claim. Source Claims are declared by ordinary platform-plane admission of an Instance of the Registry Source Plugin type, never through the plugin contract itself, which keeps no write path; that Instance is a Managed Entity of the platform's own plugin region and so lies outside every vendor claim. A claim's lifecycle is therefore that Instance's: it routes while the Instance is `ACTIVE`, deleting the Instance retires its claims — they no longer route but **MUST** remain reservations, so overlapping managed registration or claim activation remains forbidden — and only ADR-0013 purge of that Instance releases the reserved space. Managed storage **MUST** be consulted first, then plugins in deterministic priority order. Because no source declares which kinds it serves, a kind filter **MUST NOT** narrow the set of sources consulted: a kind-filtered query goes to every source whose claim intersects the queried identifier space. Absence **MUST** be authoritative — Types Registry **MUST NOT** report an identifier absent until every source required to establish that has answered authoritatively. An exact or batch source failure **MUST** remain distinct from absence; a batch **MUST** report it per affected key while returning unaffected keys. List and search operations **MUST** fail closed on any selected source failure or invalid/incomplete response and **MUST NOT** return partial pages or reinterpret failure as exhaustion or absence.

Source Claim activation **MUST** also reject a pattern covering an `x-gts-ref` authority identifier already present in Managed Type Schema content. This check reclassifies current content and stores no dependency.

- **Rationale**: Live federation requires deterministic ownership and routing without a per-external-entity index or identifier shadowing.
- **Actors**: `cpt-cf-types-registry-actor-platform-gear`, `cpt-cf-types-registry-actor-registry-source-plugin`

#### Externally Managed Entities

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-externally-managed-entities`

The system **MUST** distinguish Managed from Externally Managed Entities and **MUST NOT** persist source-authoritative state (ADR-0011).

Their identifier spaces **MUST** be disjoint, with no reference or derivation across the boundary in either direction. Managed admission **MUST** reject a crossing `$ref` or derivation edge and an `x-gts-ref` that names an externally managed target; the latter is classified from candidate content and creates no dependency. A vendor deriving from a platform contract **MUST** register the result as Managed. External derivation chains remain within one source by Source Claim routing.

Types Registry **MUST NOT** parse external content to detect a source-authored `$ref` or `x-gts-ref` to a Managed Entity. Such a reference receives no platform guarantee: no managed-target deletion safety, availability propagation, dependent revalidation, lifecycle notification, or protection from purge and identifier rebinding. This limitation does not weaken the managed target's own compatibility guarantee.

The External Registry Source **MUST** remain sole authority for source-owned entity validity; Types Registry **MUST NOT** require, interpret, or reproduce its validation results. Before exposure, Types Registry validates only platform-owned response invariants: identifier and Registry Reference integrity, Source Claim, entity kind as determined by the identifier's trailing `~`, authorization, visibility, lifecycle mapping, availability, and freshness. Every result **MUST** carry External Revision and canonical content hash, neither persisted by Types Registry.

- **Rationale**: External source ownership must not bypass platform contract governance, while source-owned entity validation policies and results remain outside the Types Registry responsibility boundary.
- **Actors**: `cpt-cf-types-registry-actor-platform-gear`, `cpt-cf-types-registry-actor-domain-gear`, `cpt-cf-types-registry-actor-registry-source-plugin`

#### Owning-Gear Semantic Validation

- [ ] `p2` - **ID**: `cpt-cf-types-registry-fr-validation-hooks`

In P2, the system **MUST** invoke every matching required owning-gear Validation Hook before initial admission, before admission of a new content revision, and before deletion of a Managed Type Schema or managed registered Instance. Admission of a higher-major Version Successor is covered as initial admission; it changes no other member of its version family and therefore triggers no additional hook.

Deletion is included because an owning gear is the only component that can see its own runtime objects, and P1 deletion cannot: a type may be deleted while live domain data still conforms to it. Until hooks exist, that exposure is a stated P1 limitation of `cpt-cf-types-registry-fr-lifecycle` rather than a gap the registry can close.

Validation Hooks **MUST NOT** apply to Externally Managed Entities, P2 Aliases, or tenant enablement changes. Those operations remain governed by their registry, dependency, lifecycle, source, and authorization rules.

- **Rationale**: Some gear-specific type requirements cannot be validated by GTS schema rules alone; the owning gear may need to enforce domain semantics while Types Registry remains the central control-plane authority.
- **Actors**: `cpt-cf-types-registry-actor-platform-gear`, `cpt-cf-types-registry-actor-domain-gear`, `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-xaas-vendor-developer`

### 5.2 References, Aliases, And Queries

#### Alias Management

- [ ] `p2` - **ID**: `cpt-cf-types-registry-fr-aliasing`

The system **MUST** allow multiple Aliases per Managed GTS Type Schema and per Managed registered GTS Instance, and **MUST** provide management and resolving behavior for Aliases. Every Alias **MUST** be a Managed Entity for which Types Registry is the source of truth. An External Registry Source **MUST NOT** supply an Externally Managed Alias, and an Externally Managed Entity **MUST NOT** be an Alias target. Each Alias has its own globally unique GTS Identifier; no Type Schema, registered Instance, or Alias may use the same canonical identifier. Tenant ownership affects Alias visibility and management only: tenant-local Alias shadowing and resolution fallback are not supported.

- **Rationale**: Users and gears need stable alternate names without duplicating registry entities. Restricting Alias ownership and targets to Managed Entities keeps Alias identity, lifecycle, uniqueness, and target validity under one authoritative consistency boundary.
- **Actors**: `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-xaas-vendor-developer`, `cpt-cf-types-registry-actor-tenant-admin`, `cpt-cf-types-registry-actor-domain-gear`

#### Reference And Identifier Resolution

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-id-resolution`

The system **MUST** resolve between user-facing GTS Identifiers and machine-readable Registry References and return entity kind, Context Tenant ownership view, and lifecycle status for both single and batch lookups. Exact resolution **MUST** be literal: it resolves the entity whose canonical identifier equals the supplied one, or nothing. In particular the system **MUST NOT** resolve a major-only identifier to the highest minor of that major, since doing so would return the reference-pinning property that `cpt-cf-types-registry-fr-minor-version-profile` exists to provide. For domain-owned data, the Types Registry SDK **MUST** return an opaque Registry Reference UUID for the exact client-supplied GTS Identifier. Domain gears **MUST** persist that Registry Reference rather than deriving it or persisting the GTS Identifier as the type reference. Types Registry **MUST** resolve Managed Entities locally, then delegate unresolved external references to Registry Source Plugins in deterministic priority order. A plugin-returned GTS Identifier **MUST** derive to the requested Registry Reference and match the plugin's Source Claim. Where Types Registry observes two distinct GTS Identifiers resolving to one Registry Reference, it **MUST** fail with a structured identity-collision error rather than select a winner, since silently choosing one corrupts persisted domain references. A collision between two External Registry Sources that is never co-observed cannot be detected and is an accepted, documented residual of deterministic derivation. When P2 Alias support is introduced, reverse resolution **MUST** preserve an exact client-supplied Alias GTS Identifier while exposing Alias target metadata separately, and Managed Aliases **MUST** resolve locally.

- **Rationale**: Domain gears need stable references for stored data and human-readable identifiers for APIs, logs, and operator workflows.
- **Actors**: `cpt-cf-types-registry-actor-domain-gear`, `cpt-cf-types-registry-actor-platform-gear`

#### Type Query Assistance

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-type-query-assistance`

The system **MUST** translate exact GTS Identifiers, version-family membership, derivation constraints, and GTS wildcard patterns into a traversal-complete, deduplicated Concrete Reference Set for querying gear-owned data by Registry Reference UUID. A pattern without a minor **MUST** select every minor of matching majors encountered by that traversal. This is membership, not compatibility, and **MUST NOT** be described as an upgrade-safe set.

Exact identifiers use unpaged batch resolution; other filters use paged expansion. Query assistance **MUST NOT** return a database predicate or executable plan and **MUST** fail rather than return a partial constraint when a required source is unavailable or invalid.

Paged traversal **MUST** be exhaustive but is not an atomic snapshot: membership may change between pages. A completed traversal is complete only for the traversal performed; an accumulated prefix **MUST NOT** be presented as a Concrete Reference Set. The result size **MUST** have a documented maximum, and exceeding it **MUST** produce a structured failure rather than truncate. The SDK **MUST** finish the traversal before presenting a Concrete Reference Set.

Query assistance is tenant-plane and carries propagated `SecurityContext`. Results **MUST** be visible to the requesting subject and available to the Context Tenant; runtime handling of unavailable domain objects remains with their owning gear.

Federated expansion **MUST** follow the managed-first deterministic source ordering of `cpt-cf-types-registry-fr-registry-source-routing`. A continuation token **MUST** fail rather than splice results from a different query, visibility context, Context Tenant, authorization scope, or source-routing state. Global ordering across sources is outside P1.

- **Rationale**: Domain gears persist Registry Reference UUIDs and need a portable constraint that can be applied consistently across SQLite, PostgreSQL, and MySQL without executing Registry-owned predicates or query plans inside gear-owned storage.
- **Actors**: `cpt-cf-types-registry-actor-domain-gear`

### 5.3 Ownership, Lifecycle, And Caching

#### Tenant And Global Ownership

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-tenant-ownership`

The system **MUST** support platform-global and tenant-owned entries; `cpt-cf-types-registry-fr-registration-policy` decides where tenant ownership is admissible.

On the tenant plane, global entries are visible to every tenant subject to lifecycle, availability, and authorization. A tenant-owned entry **MUST** be visible only to its owning tenant and descendants, never ancestors, siblings, or unrelated tenants. Discovery, search, exact and batch resolution, and query assistance **MUST** enforce the same boundary without disclosing an invisible entry's existence or metadata. Visibility grants no management authority.

Platform-plane reads **MUST** span all tenants without visibility filtering but remain authorized and **MUST NOT** disclose owning tenant identity; only ADR-0013's purge report may name owners. Platform requests **MUST NOT** create tenant-owned entities.

A read **MUST NOT** expose an owning tenant identifier, because a descendant could otherwise map identities in the hierarchy above it. With a Context Tenant it **MUST** expose only whether that tenant owns the entity; without one the value is absent. Discovery **MUST** filter by ownership scope, not by a caller-supplied tenant identifier.

An External Registry Source **MUST** assert each entity as platform-wide or owned by one tenant. Types Registry derives subtree visibility and retains authority over hierarchy, authorization, and availability. Missing or unknown-tenant assertions **MUST** be rejected as invalid responses and confer no management authority.

Ownership is fixed at admission and **MUST NOT** change. Correction requires deletion, ADR-0013 purge, and re-registration under the intended owner.

Every member of a Version Family **MUST** share the family's ownership scope. Derivation creates a new family owned by the admitting context and grants no authority over the base or its family.

- **Rationale**: Platform types and tenant customizations must coexist without cross-tenant leakage or accidental global mutation, while descendants can reuse contracts governed by an ancestor tenant.
- **Actors**: `cpt-cf-types-registry-actor-platform-gear`, `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-xaas-vendor-architect`, `cpt-cf-types-registry-actor-xaas-vendor-developer`, `cpt-cf-types-registry-actor-tenant-admin`

#### Registration Authority

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-registration-authority`

The system **MUST** authorize every initial admission, content revision, and deletion against the candidate's canonical GTS Identifier before checking identifier availability.

| Plane | Ownership and authority |
|---|---|
| Platform | Global mutations **MUST** carry `PlatformSecurityContext` and **MUST NOT** be reachable from the tenant REST surface or sent to the tenant PDP; no tenant-policy permission may exist solely for this plane. Any authenticated platform workload may mutate any global entity; `owning_gear` is attribution, not authority. Purge additionally follows ADR-0013 deployment policy. (`cpt-cf-adr-two-plane-auth`, `cpt-cf-adr-platform-plane-auth`) |
| Tenant | Tenant ownership **MUST** derive from `SecurityContext`; a payload attempting to name an owner or global scope **MUST** be rejected. The platform PDP **MUST** authorize subject, action, and canonical GTS Identifier supplied as a resource property. Negative, absent, unreachable, or unenforceable-constraint results **MUST** fail closed. A grant governs a region; registering first grants nothing. |

A tenant request **MUST NOT** mutate a global entity, and a platform request **MUST NOT** create a tenant-owned one. `cpt-cf-types-registry-fr-registration-policy` is evaluated during envelope validation before the tenant PDP: tenant-ownership refusal applies only to tenant creation, while vendor refusal applies on both planes. No grant overrides either decision. Because ownership is fixed, correcting an admitted owner requires deletion and ADR-0013 purge before re-registration.

An unauthorized caller **MUST** receive the same response whether the identifier is free, visible, invisible, deleted, or reserved by a Source Claim. Batch authorization **MUST** hold for every member within the batch's single authorization scope.

- **Rationale**: GTS Identifiers are globally unique in a vendor-structured namespace, so the right to name something is a governed right. Neither platform authority nor prefix ownership can be inferred from the order in which registrations arrive.
- **Actors**: `cpt-cf-types-registry-actor-platform-gear`, `cpt-cf-types-registry-actor-tenant-admin`, `cpt-cf-types-registry-actor-xaas-vendor-architect`, `cpt-cf-types-registry-actor-xaas-vendor-developer`

#### Registration Policy

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-registration-policy`

Types Registry **MUST** carry deployment and platform-release configuration that decides, per GTS Identifier Region:

- whether a new logical entity may be tenant-owned; and
- which vendors the candidate's own identifier may carry.

Both decisions **MUST** default to closed and **MUST NOT** be changed by a grant, request field, or authored document. The vendor decision applies on both planes; tenant ownership applies only to tenant-plane creation. For global candidates, the platform's own vendor **MUST** be implicitly admitted in every region. That exception **MUST NOT** apply to tenant-owned candidates, including those carrying the platform vendor, unless configuration explicitly admits it.

Each parameter **MUST** resolve independently from the most specific matching entry that defines it: an exact identifier precedes a pattern, otherwise the longest literal prefix wins, and absence yields the closed default. A selected value replaces rather than extends a less-specific one.

Policy applies only when creating a logical entity. It **MUST NOT** block revision or deletion of an admitted entity; closing a region leaves its owner able to revise and delete, while ongoing write authority remains governed by grants.

The platform release **MUST NOT** ship an open region. Outside the implicit global platform-vendor exception, a deployment admitting a vendor **MUST** name it in every region its identifiers reach, including regions beneath platform base types. Any policy refusal — whether caused by an absent value, a selected vendor set that omits the candidate, or tenant ownership not being enabled — **MUST** identify the decisive region and parameter. It **MUST** fail the first affected registration and be distinguishable from authorization failures and malformed identifiers.

Configuration **MUST NOT** relax `cpt-cf-types-registry-fr-gts-validation`, `cpt-cf-types-registry-fr-minor-version-profile`, `cpt-cf-types-registry-fr-externally-managed-entities`, or `cpt-cf-types-registry-fr-registration-authority`.

- **Rationale**: A vendor building a product on the platform decides which of its contracts third parties may extend and which GTS Identifier Regions its tenants may own, while the gear that authors a type states what that type was designed for; neither decision belongs to the order in which registrations arrive. Closed defaults make a missing entry a visible over-restriction rather than a silent hole, which matters because ownership is fixed at admission.
- **Actors**: `cpt-cf-types-registry-actor-xaas-vendor-architect`, `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-ci-pipeline`

#### Lifecycle Management

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-lifecycle`

The P1 Lifecycle Status of a logical entity **MUST** be `ACTIVE` or terminal `DELETED`. `pending` and `running` belong only to an Admission Candidate or operation; `DEPRECATED` and transitions for deprecation, undeprecation, restore, or reactivation **MUST NOT** be exposed in P1 (ADR-0008).

Initial admission **MUST** atomically create an `ACTIVE` entity at revision `1`; a failed candidate creates none. A successful content update **MUST** create the next entity-scoped, monotonically increasing revision. Candidates in `pending` or `running`, candidates completing `failed`, and idempotent `unchanged` candidates **MUST NOT** change the current revision or Lifecycle Status. Lifecycle-only transitions **MUST NOT** create content revisions, and a lifecycle mutation and its cache-freshness metadata **MUST** become visible atomically.

`unchanged` **MUST** be returned only for registration whose canonical authored content equals the current admitted content. It **MUST** return the existing `gts_uuid` and `resource_version`, create no revision, and leave entity state unchanged. Creation under a nonexistence precondition and deletion **MUST NOT** return `unchanged`.

**Version families.** Admitting a Version Successor **MUST NOT** alter another family member, and the system **MUST** permit multiple members to be `ACTIVE`. Major members may be admitted in any order; minor members follow `cpt-cf-types-registry-fr-minor-version-profile`. The system **MUST NOT** compute or expose a newest family member, while discovery **MUST** enumerate all family members, including every minor. P2 Aliases **MUST** use this lifecycle model unless their decision explicitly supersedes it.

**Deletion.** The system **MUST** permit an authorized deletion to move an `ACTIVE` entity directly to `DELETED`, without a successor or constraint from other family members, but **MUST** fail while a live registered dependent exists. P1 derives dependants from managed `$ref`, derivation, and Instance-conformance edges; `x-gts-ref` creates no edge and **MUST NOT** block deletion. The registry neither calls plugins nor sees runtime domain objects, so owning-gear validation is deferred to `cpt-cf-types-registry-fr-validation-hooks`.

A deleted GTS Identifier **MUST NOT** be restored or reused. Admitted identity and content **MUST NOT** expire through retention, TTL, or background policy; only ADR-0013's explicit platform purge physically removes them. Purge **MUST** be operator-invoked on the platform plane, disabled by default, and restricted to `DELETED` entities with no live registered dependent; its contract **MUST** state that releasing an identifier may rebind its deterministic Registry Reference. Unreferenced terminal operation records may expire without affecting any entity, revision, tombstone, or identifier. Deletion **MUST** preserve resolution of previously issued Registry References.

**Externally Managed Entities.** Types Registry **MUST** obtain lifecycle live from the owning source and expose only `ACTIVE` or `DELETED`. A source-side deprecation **MUST** map to `ACTIVE`; pending candidates **MUST NOT** be exposed. A source may transition directly to `DELETED`, and resolution, reference validation, and search **MUST** respect the mapped status.

- **Rationale**: Type evolution needs controlled admission and removal. The registry neither invents owner intent nor restates version ordering that the identifiers already carry.
- **Actors**: `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-xaas-vendor-architect`, `cpt-cf-types-registry-actor-xaas-vendor-developer`, `cpt-cf-types-registry-actor-tenant-admin`

#### Tenant Availability Evaluation

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-tenant-availability`

The system **MUST** evaluate Tenant Availability State for an entity and Context Tenant from Lifecycle Status, subject visibility, availability-blocking relationships, and authoritative external tenant state and freshness where applicable. A relationship blocks availability when its target contributes to the subject's semantic contract; materialization does not remove the edge, and unavailability propagates transitively only along outgoing blocking edges.

The availability-blocking relationships are:

| Relationship | Blocking |
|---|---|
| Registered Instance → conforming Type Schema | yes |
| Type Schema → each derivation base | yes |
| Type Schema → `$ref` targets | yes |
| Type Schema → `x-gts-ref` targets | no — the target contributes no content to the schema's semantic contract |
| P2 Alias → target | yes |
| Target → reverse dependents | no |
| Entity → Version Family siblings | no |

A new relationship kind **MUST** be classified by the same semantic-contract rule before it affects availability. Blocking edges exist only between Managed Entities; an Externally Managed Entity's availability is obtained live from its source and has no registry-composed Availability Closure.

P1 has no managed enablement override. A visible `ACTIVE` Managed Entity is eligible for `AVAILABLE` but **MUST** be reasoned `UNAVAILABLE` when a blocking target is unavailable. A `DELETED` entity **MUST** be unavailable yet still be returned by exact read as deleted; discovery, search, and query assistance exclude it. Admission Candidates **MUST NOT** participate.

The Context Tenant defaults to the subject tenant on the tenant plane. A caller may name a descendant only when the platform PDP authorizes the subject-to-context ancestor relation. The platform plane has no default; without an explicit Context Tenant the verdict **MUST** be absent, with no synthetic not-evaluated state.

Visibility **MUST** use the requesting subject while availability uses the Context Tenant; substituting the latter for visibility would disclose descendant-owned contracts. If an External Registry Source cannot confirm required state, evaluation **MUST** fail closed. Handling existing runtime objects whose registry entity is unavailable remains the owning Gear's policy.

- **Rationale**: Consumers need one authoritative usability result instead of independently combining lifecycle, tenancy, dependency, and external-source rules.
- **Actors**: `cpt-cf-types-registry-actor-platform-gear`, `cpt-cf-types-registry-actor-domain-gear`, `cpt-cf-types-registry-actor-tenant-admin`, `cpt-cf-types-registry-actor-registry-source-plugin`

#### Tenant Enablement Management

- [ ] `p2` - **ID**: `cpt-cf-types-registry-fr-tenant-enablement`

The system **MUST**, after P1, support a stored Tenant Enablement State for an entity: `NOT_INITIALIZED`, `ENABLED`, or `DISABLED`. The state carries no reason or expiry; any policy change is represented by a state transition. This state is a policy input to Tenant Availability State, not the consumer-facing result. Types Registry **MUST** allow authorized actors to manage this state for Managed Entities. For Externally Managed Entities, the External Registry Source remains authoritative for tenant enablement state.

- **Rationale**: Tenant policy must be independently controllable without conflating it with platform lifecycle or computed availability.
- **Actors**: `cpt-cf-types-registry-actor-platform-gear`, `cpt-cf-types-registry-actor-domain-gear`, `cpt-cf-types-registry-actor-tenant-admin`, `cpt-cf-types-registry-actor-registry-source-plugin`

#### Casting

- [ ] `p2` - **ID**: `cpt-cf-types-registry-fr-casting`

The system **MUST** support casting supplied instance content between two registered GTS Type Schemas that Types Registry can relate, and **MUST** report incompatible casts as structured failures.

GTS OP#9 defines casting only between compatible minor versions. In a Minor-Bearing Major that transition exists and is exactly the one `cpt-cf-types-registry-fr-minor-version-profile` establishes a compatibility relation over, so a cast between two minors of one major **MUST** be presentable as an OP#9 result — but only where the relation was actually established, which excludes a step admitted under `force` and excludes a major-0 family, where no mode is enforced at all. Everywhere else it does not: a major-only major has no minors, and the remaining transitions this requirement covers — between major identities in one version family, and between content revisions of one logical entity — lie outside OP#9 whatever the profile. Types Registry **MUST** present those as a platform capability and **MUST NOT** present them as an OP#9 conformance result. The exact admissible transition set is an open question.

- **Rationale**: Consumers need a central, consistent way to migrate or interpret versioned typed content.
- **Actors**: `cpt-cf-types-registry-actor-domain-gear`, `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-xaas-vendor-developer`

#### Cache Freshness Metadata And Conditional Reads

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-cache-freshness-metadata`

Every exact resolution, by either key or as a batch member, **MUST** return metadata that lets a later read determine whether the same result remains current. A mutation **MUST** publish replacement metadata atomically with the invalidation. Discovery pages are exempt: their set membership changes independently of any member, so they **MUST NOT** expose an entity-style validator.

The validator **MUST** cover the complete projected result, including tenant availability, rather than only entity `resource_version`. It **MUST** be scoped to the entity, Context Tenant, visibility context, and field projection for which it was issued; a validator from another scope or projection **MUST** yield the full result, never a false unchanged response.

For an Externally Managed Entity, the validator **MUST** derive from the source's opaque revision and content hash, remain unpersisted by Types Registry, and change whenever the platform-visible result for that entity and tenant changes, including source-owned tenant enablement. Validation **MUST** delegate to the owning source's conditional-read semantics under the federation contract.

Single and batch reads **MUST** accept caller-supplied validators and return an unchanged outcome instead of the full result when current; batch evaluation is per item. Types Registry **MUST NOT** report unchanged unless currentness is established (`cpt-cf-types-registry-principle-fail-closed`). Callers may use this contract directly or through the P1 SDK cache of `cpt-cf-types-registry-fr-client-cache`.

- **Rationale**: Once a managed identifier is mutable, a consumer cannot tell a current result from a stale one without the registry saying so. This is a correctness property of the registry, not of its clients, and emitting the validator without honouring it leaves the correct behaviour available in principle and unaffordable in practice. A later event-based invalidation transport does not retire it: events say when to invalidate, a validator says whether what is held now is current, and only the second answers for a process that just started or missed a message.
- **Actors**: `cpt-cf-types-registry-actor-domain-gear`, `cpt-cf-types-registry-actor-platform-gear`

#### Client-Side Cache Correctness

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-client-cache`

The SDK **MUST** define cache storage, freshness validation, batch polling, cold-start behavior, and eviction so that an invalidated result is never treated as current across an observed registry mutation. Its key and validator scope **MUST** include every input that can change the projected result, including Context Tenant and visibility context.

A client may disable caching and resolve on every use, or manage the validators and conditional reads of `cpt-cf-types-registry-fr-cache-freshness-metadata` directly.

- **Rationale**: Registry lookups are common on startup and hot paths; caching must not return stale type authority.
- **Actors**: `cpt-cf-types-registry-actor-domain-gear`, `cpt-cf-types-registry-actor-platform-gear`

#### Batch Admission And Startup Registration

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-two-phase-init`

The system **MUST** support batches in which references resolve against submitted candidates before previously committed state. A batch may mix initial admissions and content revisions, each with its own precondition.

Registration and deletion **MUST** always be asynchronous and return an operation. They **MUST** require an `Idempotency-Key` scoped to plane, owning tenant, and principal and bound to every behavior-affecting request field; an identical replay returns the stored operation, while conflicting reuse fails. Each candidate **MUST** declare nonexistence for creation or a positive caller-observed `resource_version` for update or deletion. A mismatch **MUST** fail that candidate without silently rebasing it.

A registration batch **MUST** be non-empty, contain at most 100 candidates, and use one plane and ownership/authorization scope. Every distinct identifier **MUST** be authorized independently.

P1 uses ADR-0012's **dependency-aware partial admission**, not one all-or-nothing transaction. Each candidate is admitted independently, in dependency order. Independent valid candidates **MUST** commit despite failures elsewhere. A candidate whose selected in-batch dependency failed **MUST NOT** commit and **MUST** be distinguished from one evaluated and failed on its own checks. Every member **MUST** receive an outcome keyed by exact GTS Identifier, with actionable diagnostics.

An admitted initial candidate creates `ACTIVE` revision `1`; a failed or blocked initial candidate creates nothing and leaves committed state unchanged.

Types Registry **MUST NOT** implement a global startup barrier or expected startup set. It **MUST** publish ready state when its own storage is ready and **MUST NOT** wait for registrants. At startup, a registrant **MUST** read and reconcile its declared inventory, omit equal content, and conditionally submit missing or changed definitions. It **MUST** retry missing-dependency failures and **MUST NOT** become ready until its registrations succeed; such a failure **MUST** be retryable and succeed once the dependency exists.

Cycles in the combined `$ref` and derivation graph **MUST NOT** be admitted, in one batch or across operations, because both edge kinds are inlined into the effective form. The resulting dependency relation is acyclic, so no candidate group requires atomic admission (ADR-0012).

- **Rationale**: A gear can have interdependent definitions whose admission order matters, while an unrelated invalid candidate should not prevent valid independent registrations. Separately, the registry cannot know the membership of a platform-wide startup set, and making its readiness depend on every registrant would put the slowest gear on the platform boot path.
- **Actors**: `cpt-cf-types-registry-actor-platform-gear`

#### Dry Run

- [ ] `p1` - **ID**: `cpt-cf-types-registry-fr-dry-run`

Registration, deletion, and ADR-0013 purge **MUST** support Dry Run as a mode of the real operation. It **MUST** execute the same ordered checks and authorization, including P2 Validation Hooks when present, without creating an entity or revision, moving a current-revision pointer, advancing `resource_version`, changing Lifecycle Status, or removing content.

The result **MUST** report per candidate the outcome and diagnostics produced against the observed state. A Dry Run `succeeded` registration candidate carries neither a content revision nor resulting `resource_version`; a Dry Run `unchanged` candidate follows the lifecycle rule and returns the existing `gts_uuid` and `resource_version`. A passing result **MUST NOT** be presented as an admission guarantee because relevant state may change before submission.

Registration and deletion retain ADR-0012's asynchronous shape. Dry Run mode **MUST** participate in their request fingerprint, so a real request sharing a request key with a Dry Run is not answered from the Dry Run result. ADR-0013 purge remains synchronous and stores no request identity.

Authorization **MUST** precede identifier availability, and Dry Run **MUST NOT** disclose anything the real operation would hide. It identifies a visible refusing dependent, reports an invisible one only as a count, and does not report non-refusing impact. P2 hooks make registration and deletion Dry Runs no more synchronous than their real operations; purge invokes none.

- **Rationale**: The checks a caller wants before deploying are exactly the checks admission performs, but admission commits when they pass. Under `cpt-cf-types-registry-fr-lifecycle` an admitted revision cannot be withdrawn, so using a real registration as a test publishes the contract as a side effect of testing it. Separately, because a registrant gates its own readiness on its registrations succeeding, an incompatible change discovered at admission is a failed rollout rather than a failed build.
- **Actors**: `cpt-cf-types-registry-actor-ci-pipeline`, `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-xaas-vendor-architect`, `cpt-cf-types-registry-actor-xaas-vendor-developer`, `cpt-cf-types-registry-actor-tenant-admin`

## 6. Non-Functional Requirements

> **Global baselines**: Project-wide architectural and quality baselines are defined in [docs/ARCHITECTURE_MANIFEST.md](../../../../docs/ARCHITECTURE_MANIFEST.md), [guidelines/README.md](../../../../guidelines/README.md), and [ToolKit Unified System](../../../../docs/toolkit_unified_system/README.md). This section defines only Types Registry-specific NFRs.
>
> **Testing strategy**: NFRs are verified through automated benchmarks, integration tests, security checks, and monitoring as appropriate to the requirement.

### 6.1 Gear-Specific NFRs

#### Lookup Latency

- [ ] `p1` - **ID**: `cpt-cf-types-registry-nfr-lookup-latency`

The system **MUST** resolve an exact Managed Entity Registry Reference or GTS Identifier lookup within 10 ms at p95 under the supported production benchmark profile defined in DESIGN. For an Externally Managed Entity, the same threshold applies only to Types Registry federation and policy-processing overhead; Registry Source Plugin and External Registry Source execution time are governed by the federation contract.

- **Threshold**: p95 < 10 ms for a managed exact lookup and p95 < 10 ms for Types Registry external-resolution overhead.
- **Rationale**: Registry resolving is used by gear startup and runtime paths.
- **Verification Method**: Automated benchmark against the versioned production benchmark profile defined in DESIGN.

#### Query Latency

- [ ] `p2` - **ID**: `cpt-cf-types-registry-nfr-query-latency`

The system **MUST** return bounded Managed Entity searches within 100 ms at p95 under the supported production benchmark profile defined in DESIGN. For federated searches, the same threshold applies only to Types Registry processing overhead; participating source execution time is governed by the federation contract.

- **Threshold**: p95 < 100 ms for a bounded managed search and p95 < 100 ms for Types Registry federated-search overhead.
- **Rationale**: Discovery and management views must remain responsive.
- **Verification Method**: Automated benchmark against the versioned production benchmark profile defined in DESIGN.

#### Multi-Pod Correctness

- [ ] `p1` - **ID**: `cpt-cf-types-registry-nfr-multi-pod-correctness`

The system **MUST** make every committed Managed Entity or Registry Source Plugin configuration mutation visible to every Types Registry pod after transaction commit. External entity consistency across plugin instances, pods, and data centers is governed by the federation contract.

- **Threshold**: 100% of committed mutations are visible on every pod's first post-commit read.
- **Rationale**: Production deployments are horizontally scaled.

#### Cache Correctness

- [ ] `p1` - **ID**: `cpt-cf-types-registry-nfr-cache-correctness`

The system **MUST** prevent SDK clients from treating invalidated registry lookup results as current after a relevant registry mutation is observed.

- **Threshold**: Zero stale registry results are accepted as current after the relevant mutation is observed by the client.
- **Rationale**: Client-side caching is required but cannot weaken type authority.
- **Verification Method**: Integration tests cover mutation, cache validation, and stale-entry rejection.

### 6.2 NFR Exclusions

- None identified.

## 7. Public Library Interfaces

### 7.1 Public API Surface

#### SDK Contract

- [ ] `p1` - **ID**: `cpt-cf-types-registry-interface-sdk`

- **Type**: Rust SDK trait and models.
- **Stability**: unstable until first platform-stable release.
- **Description**: In-process and remote-client contract for gear-to-gear registration, resolving, discovery, compatibility, and externally managed entity access.
- **Breaking Change Policy**: Breaking changes allowed before first stable release; afterwards require versioned contract.

#### REST API

- [ ] `p1` - **ID**: `cpt-cf-types-registry-interface-rest`

- **Type**: Authenticated REST API.
- **Stability**: unstable until first platform-stable release.
- **Description**: External and tenant-facing contract for management, discovery, resolving, validation, and externally managed entity visibility.
- **Breaking Change Policy**: Breaking changes allowed before first stable release; afterwards require versioned API.

#### Registry Source Plugin SPI

- [ ] `p1` - **ID**: `cpt-cf-types-registry-interface-source-plugin`

- **Type**: Rust plugin trait and models, resolved through the ToolKit scoped ClientHub.
- **Stability**: unstable until first platform-stable release.
- **Description**: The contract Types Registry defines and a Registry Source Plugin implements: batch forward and reverse resolution, bounded candidate queries, tenant state, freshness and conditional reads, ownership assertions, and the effective artifacts of a Type Schema result. It is shaped for a remote counterparty although P1 plugins are in-process.
- **Breaking Change Policy**: Breaking changes allowed before first stable release; afterwards require a versioned contract, because a plugin is built and shipped separately from the registry.

### 7.2 External Integration Contracts

#### GTS Implementation

- [ ] `p1` - **ID**: `cpt-cf-types-registry-contract-gts-rust`

- **Direction**: required by Types Registry.
- **Protocol/Format**: Rust library API.
- **Compatibility**: Types Registry relies on the approved GTS implementation for parsing, normalization, reference derivation, wildcard matching, validation, compatibility, and casting semantics.

#### Platform AuthN/AuthZ

- [ ] `p1` - **ID**: `cpt-cf-types-registry-contract-platform-auth`

- **Direction**: required by Types Registry.
- **Protocol/Format**: ToolKit SecurityContext, PolicyEnforcer, and platform authentication/authorization contracts.
- **Compatibility**: Tenant/global ownership checks must follow platform-level AuthN/AuthZ rules, and the two planes use different mechanisms rather than one mechanism with different inputs. Tenant-scoped registration authority is a PDP decision over the candidate GTS Identifier, expressed through the canonical permission GTS Type of `docs/arch/authorization/PERMISSION_GTS_TYPE.md`, whose `resource_type` field already accepts a GTS wildcard pattern. Global registration is a platform-plane operation under the two-plane model and `cpt-cf-adr-platform-plane-auth`, where the `PolicyEnforcer` is not on the path at all: the validated `PlatformIdentity` is the authorization, and per-workload narrowing, if a deployment wants it, is workload policy over that identity.

#### ToolKit Plugin Architecture

- [ ] `p1` - **ID**: `cpt-cf-types-registry-contract-toolkit-plugins`

- **Direction**: required by Types Registry for external registry source integration.
- **Protocol/Format**: ToolKit plugin and scoped ClientHub contracts.
- **Compatibility**: External Registry Sources must be integrated behind Types Registry rather than consumed directly by regular gears. Across its claimed identifier space, a Registry Source Plugin must implement the whole mandatory P1 federation and completeness contract defined by Registry Source Routing; concrete plugin traits and transport models are versioned SDK design.

## 8. Use Cases

#### Register A GTS Type Schema

- [ ] `p1` - **ID**: `cpt-cf-types-registry-usecase-register-type-schema`

**Actor**: `cpt-cf-types-registry-actor-gears-developer`, `cpt-cf-types-registry-actor-xaas-vendor-architect`, or `cpt-cf-types-registry-actor-xaas-vendor-developer`

**Preconditions**:
- A GTS Type Schema is available for registration.

**Main Flow**:
1. Actor registers the GTS Type Schema.
2. Types Registry creates an Admission Candidate and validates identity, ownership, compatibility, lifecycle, and conflicts.
3. On successful admission, Types Registry atomically creates the logical Type Schema in `ACTIVE` with revision `1`.
4. Owning gears can discover the Type Schema, resolve it for their tenant, and use its registry reference in their own data.

**Postconditions**:
- The Type Schema is discoverable and governed by Types Registry.

#### Resolve A User-Facing Type Filter For Gear-Owned Data

- [ ] `p1` - **ID**: `cpt-cf-types-registry-usecase-resolve-type-filter`

**Actor**: `cpt-cf-types-registry-actor-domain-gear`

**Preconditions**:
- The gear owns runtime objects that reference registry entities.
- A caller supplies a GTS Identifier, version-family membership expression, derivation constraint, or wildcard pattern.

**Main Flow**:
1. Gear asks Types Registry to resolve the user-facing type filter.
2. Types Registry applies ownership, lifecycle, version, and wildcard rules.
3. Gear receives a traversal-complete, bounded Concrete Reference Set and applies it to its own storage using backend-safe UUID-set filtering.

**Postconditions**:
- The gear returns domain objects by matching their stored Registry Reference UUIDs against the traversal-complete set selected by Types Registry.

#### Use An Externally Managed Entity

- [ ] `p1` - **ID**: `cpt-cf-types-registry-usecase-use-externally-managed-entity`

**Actor**: `cpt-cf-types-registry-actor-domain-gear`

**Preconditions**:
- An External Registry Source is available through a governed Registry Source Plugin.
- The external source provides a registry entity visible to the requesting subject and available to the Context Tenant.

**Main Flow**:
1. Types Registry checks managed storage and selects the owning Registry Source Plugin using the ordered Source Claim model.
2. The plugin resolves or queries the externally managed entity live and returns canonical content, opaque revision, content hash, source lifecycle and ownership/visibility assertions, and authoritative tenant state when required.
3. Types Registry validates federation response conformance, the Registry Reference, and the Source Claim, then applies platform-owned authorization, visibility, lifecycle mapping, availability, and cache/freshness rules.
4. The domain gear resolves or discovers the entity through the normal Types Registry SDK or REST contract.

**Postconditions**:
- The domain gear uses the entity through Types Registry without directly depending on the External Registry Source.

#### Validate A Type Evolution Before Deployment

- [ ] `p1` - **ID**: `cpt-cf-types-registry-usecase-validate-type-evolution`

**Actor**: `cpt-cf-types-registry-actor-ci-pipeline`

**Preconditions**:
- A Type Schema change is proposed.

**Main Flow**:
1. CI submits the proposed Type Schemas as a Dry Run of the ordinary registration operation.
2. Types Registry performs the complete admission check sequence and commits nothing.
3. CI polls the operation and reads the per-GTS-ID outcome: for each candidate, whether the real operation would have been accepted, and for each refusal the structured cause — a compatibility violation with its schema location, a derivation violation against a named base, or a lifecycle or dependency conflict.
4. CI reads the per-candidate diagnostics, which name the dependents a change would break that are visible to the requesting tenant, and report a count for the rest — the disclosure rule of ADR-0009 governs a Dry Run exactly as it governs the operation it rehearses. The Dry Run performs the same dependent revalidation admission does, so nothing further needs asking.
5. CI accepts or blocks the deployment based on those results.

**Postconditions**:
- Incompatible or unsafe type changes are detected before rollout, and nothing was published in the course of detecting them.

**Notes**:
- A passing Dry Run is not a guarantee of admission. The verdict is relative to the state observed during the run, and a target's `resource_version`, a dependency's current revision, or the entity's existence may change before the real submission.
- The comparison baseline is whatever the installation the Dry Run ran against currently holds — the entity's own current revision, or that of the preceding minor of its major. Under `cpt-cf-types-registry-constraint-single-installation` two installations need not hold the same entities, so a green result against one environment does not establish acceptance in another.

## 9. Acceptance Criteria

Acceptance of this PRD requires automated evidence for the cross-cutting outcomes below. Product P1 is gated only by criteria whose referenced capabilities belong to Product P1; Product P2 criteria become gates when those capabilities are delivered. Detailed edge cases, concurrency interleavings, and storage-level verification remain with the referenced requirements and ADR confirmation sections rather than being repeated here.

- [ ] **Managed registration** — authorized actors can register, retrieve, discover, revise where permitted, and delete Managed Type Schemas and registered Instances; invalid candidates create no logical entity or revision. (`cpt-cf-types-registry-fr-register-schemas`, `cpt-cf-types-registry-fr-register-instances`)
- [ ] **Batch admission and startup** — registration and deletion use idempotent asynchronous operations with optimistic candidate preconditions; in-batch references resolve against submitted candidates, independent valid branches may succeed, cycles in the combined `$ref` and derivation graph are refused, every candidate receives a keyed outcome, and registry readiness never waits for registrants. (`cpt-cf-types-registry-fr-two-phase-init`)
- [ ] **Dry Run** — registration, deletion, and purge Dry Runs execute the real check sequence with the same authorization and diagnostics, commit nothing, and are documented as state-relative predictions rather than admission guarantees. (`cpt-cf-types-registry-fr-dry-run`)
- [ ] **Managed GTS profile** — managed identifiers and documents satisfy the platform profile, including derived Registry Reference uniqueness, Instance version restrictions, and the Draft-07 root declaration; external content is not reinterpreted under that profile. (`cpt-cf-types-registry-fr-gts-validation`)
- [ ] **Minor-version profile** — a major is permanently major-only or minor-bearing; minors are immutable, contiguous from `M.0`, and released only as a suffix; major-only identifiers never resolve to a minor. Platform-owned `gts.cf.*` declarations remain major-only through the architecture lint rather than admission policy. (`cpt-cf-types-registry-fr-minor-version-profile`)
- [ ] **Compatibility** — every stable candidate with a baseline is admitted only when backward compatibility is established, an undecidable verdict fails closed, and `force` can waive only an enabled cross-minor check and remains visible afterwards. (`cpt-cf-types-registry-fr-validate-schema-compat`)
- [ ] **Unstable quarantine** — major-0 Type Schemas remain subject to all checks except evolution compatibility; stable schemas cannot derive from them or include them through `$ref`; registered Instances cannot use or conform to the unstable profile; unstable schemas may depend on stable ones; `x-gts-ref` is outside the quarantine. (`cpt-cf-types-registry-fr-validate-schema-compat`, `cpt-cf-types-registry-fr-register-instances`)
- [ ] **Derivation and dependency safety** — derived schemas remain substitutable for their complete base chain; `$ref`, immediate derivation, and Instance conformance dependencies are tracked, revalidated when affected, and block deletion while live. `x-gts-ref` creates no dependency. (`cpt-cf-types-registry-fr-validate-type-derivation`, `cpt-cf-types-registry-fr-ref-tracking`)
- [ ] **Lifecycle and identity** — admitted entities expose only `ACTIVE` or terminal `DELETED` in P1; content revisions and lifecycle transitions remain distinct; deletion retains identity and reverse resolution, and only explicitly enabled operator purge releases them. (`cpt-cf-types-registry-fr-lifecycle`)
- [ ] **Federation ownership boundary** — Externally Managed Entity state is obtained live and never projected into registry storage; plugins have no registry write path, and no managed dependency guarantee crosses the managed–external boundary. (`cpt-cf-types-registry-fr-registry-federation`, `cpt-cf-types-registry-fr-externally-managed-entities`)
- [ ] **Source routing and completeness** — valid non-overlapping Source Claims route to conforming sources; managed storage is consulted first; a non-conforming or incomplete source response is rejected; exact source failures remain distinct from absence; discovery fails rather than returning a partial page. (`cpt-cf-types-registry-fr-registry-source-routing`)
- [ ] **Reference resolution** — exact GTS Identifier and Registry Reference resolution is literal and bidirectional for both origins, preserves deleted identities, validates plugin-returned mappings, and fails on an observed identity collision rather than selecting a winner. (`cpt-cf-types-registry-fr-id-resolution`)
- [ ] **Query assistance** — supported user-facing filters produce a traversal-complete, deduplicated, tenant-visible and tenant-available Concrete Reference Set; traversal is bounded, never silently truncated, and never returns an unfinished prefix as complete. (`cpt-cf-types-registry-fr-type-query-assistance`)
- [ ] **Ownership and disclosure** — one owner scope governs every Version Family; global entries are visible platform-wide; tenant-owned entries are visible only in their owning Tenant Subtree; reads disclose ownership only as the Context Tenant boolean `owned_by_context_tenant`, while platform reads cross subtrees without disclosing owner identity. (`cpt-cf-types-registry-fr-tenant-ownership`)
- [ ] **Registration authority** — global writes use only the platform plane; tenant ownership derives from `SecurityContext`; every tenant mutation is authorized against its canonical identifier before name availability is evaluated; authorization and infrastructure failures fail closed. (`cpt-cf-types-registry-fr-registration-authority`)
- [ ] **Registration policy** — without an opening entry no tenant-owned entity is admitted, including a derivation below a platform base, while global creation admits only the platform vendor; the platform-vendor exception applies only globally, vendor refusal applies on both planes, every policy refusal names its decisive region and parameter, grants and requests cannot open policy, and closing a region does not strand revisions or deletion. (`cpt-cf-types-registry-fr-registration-policy`)
- [ ] **Tenant availability** — visibility is evaluated for the requesting subject and availability for the Context Tenant; unavailable lifecycle, dependencies, or authoritative source state produce a platform-owned unavailable verdict, and unconfirmed source state fails closed. (`cpt-cf-types-registry-fr-tenant-availability`)
- [ ] **Freshness and cache correctness** — every exact result carries projection- and context-scoped freshness metadata, conditional batch reads transfer only changed results, and SDK caching never accepts an invalidated or unconfirmable result as current after observing the relevant mutation. (`cpt-cf-types-registry-fr-cache-freshness-metadata`, `cpt-cf-types-registry-fr-client-cache`)
- [ ] **P2 governed extensions** — Validation Hooks cover matching managed admission, revision, and deletion only; Aliases remain managed and globally unique with managed targets; Tenant Enablement remains distinct from computed availability; supported casts return structured outcomes. (`cpt-cf-types-registry-fr-validation-hooks`, `cpt-cf-types-registry-fr-aliasing`, `cpt-cf-types-registry-fr-tenant-enablement`, `cpt-cf-types-registry-fr-casting`)
- [ ] **Interfaces and isolation** — gears use the versioned SDK/REST contracts, Registry Source Plugins satisfy the isolated SPI behind Types Registry, and ordinary gears never consume a source plugin directly. (`cpt-cf-types-registry-interface-sdk`, `cpt-cf-types-registry-interface-rest`, `cpt-cf-types-registry-interface-source-plugin`)
- [ ] **Quality thresholds** — the production benchmark profile satisfies the managed lookup and bounded-query latency targets; every pod observes committed managed mutations on its first post-commit read; cache correctness admits zero stale results after client observation. (`cpt-cf-types-registry-nfr-lookup-latency`, `cpt-cf-types-registry-nfr-query-latency`, `cpt-cf-types-registry-nfr-multi-pod-correctness`, `cpt-cf-types-registry-nfr-cache-correctness`)
## 10. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| [GTS specification](https://github.com/globaltypesystem/gts-spec) | Defines canonical GTS identity, type/instance terminology, validation, derivation, compatibility, and reference semantics | `p1` |
| gts-rust | Platform-approved implementation of GTS parsing, validation, compatibility, reference derivation, wildcard, casting, and schema generation/conversion behavior | `p1` |
| ToolKit SDK/ClientHub | Gear-to-gear contract and client registration mechanism | `p1` |
| ToolKit plugin architecture | Plugin isolation and scoped client pattern for Registry Source Plugins | `p1` |
| Platform AuthN/AuthZ | Tenant/global access control and SecurityContext propagation | `p1` |
| Persistent platform database | Authoritative Managed Entity and Registry Source Plugin configuration state for multi-pod deployments | `p1` |

## 11. Assumptions

- GTS remains the canonical platform type identity model.
- Runtime domain objects remain owned by their domain gears, not by Types Registry.
- Gears use Types Registry for resolving and query assistance. Domain gears persist the opaque Registry Reference UUID returned by the Types Registry SDK for the exact client-supplied GTS Identifier; they do not derive the reference or persist the GTS Identifier as the type reference, as defined by ADR-0001.
- External Registry Sources remain authoritative for externally managed entities. Their plugins own external definitions, identifiers, Registry Reference mappings, revisions, queries, source-side dependency data, caches, tombstones, lifecycle assertions, and tenant state, while regular gears access them only through Types Registry. There is no exception and no plugin write path: an External Registry Source serves a self-contained type universe that neither depends on nor is depended upon by Managed Entities.
- Industry analogues are used as design inputs by pattern, not as direct product copies.

## 12. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Types Registry scope expands into a universal object store | Ownership confusion and excessive complexity | Keep runtime object storage and business behavior explicitly out of scope |
| P2 Alias resolution and filter semantics are underspecified | Inconsistent query and cache behavior across gears | Decide literal-versus-target matching, Alias chaining, and retargeting before P2 implementation |
| Cache protocol is too weak for multi-pod deployments | Stale type resolution in long-running clients | Make cache correctness a first-class requirement and integration-test mutation scenarios |
| Gear-specific semantic validation is underspecified | Types unsuitable for a gear's domain can be admitted | Define hook binding, execution, AuthN, timeout, and failure policy before implementation |
| Semantic validation hooks become an execution framework | Security, latency, and ownership complexity | Keep hooks as governed validation contracts owned by gears; define execution, AuthN, timeout, and failure policy before implementation |
| External sources bypass platform governance | Inconsistent contracts, resolving, or visibility across gears | Require every external result to pass platform-owned federation boundary checks before use by gears |
| A Registry Source Plugin serves stale tenant state from its internal cache | Tenants may see entities as available after the source changes lifecycle or tenant enablement | Require live plugin lookup at decision time and make any plugin-internal cache subject to explicit source invalidation and conformance guarantees |
| A Registry Source Plugin is unavailable or returns incomplete data | Exact resolution or list/search results may be mistaken for authoritative absence | Distinguish `NOT_FOUND` from source failure and fail closed for all P1 registry operations that require the source |
| Plugin Source Claims overlap | Priority silently becomes identifier shadowing and results vary by source order | Reject overlapping Source Claims and Managed Entity conflicts in P1 |
| An External Registry Source references a managed contract from inside its own schema, which the platform can neither prevent nor detect | The managed contract can be deleted, purged, or revised without any block, availability signal, or revalidation, and the external entity breaks with no registry event | Accepted, not mitigated: the integration contract states that no dependency guarantee crosses the boundary, and a vendor building on a platform contract registers the derived type as Managed instead. Detection by parsing external content is rejected by `cpt-cf-types-registry-fr-externally-managed-entities` |
| Federated pagination is unstable across plugin changes | Clients see duplicates, gaps, or inconsistent source ordering | Use source-major ordering and bind opaque cursors to a plugin configuration revision |
| Owners publish minors freely once the shape is chosen for a major | `ACTIVE` members accumulate, each pinned by dependents that block its deletion, with no way to signal that a minor should no longer be adopted | Partly mitigated: major-only is the recommendation and the rule for platform contracts, and a minor is a visibly deliberate act. The residual deprecation gap is ADR-0008's, reached sooner here; see open question 1 |
| The compatibility relation changes meaning — a GTS specification revision or a checker correction — after entities were admitted under the superseded rules | A major's whole-history statement lapses silently: the highest minor no longer provably accepts everything that major accepted | Partly deferred. Every admitted revision records the specification and implementation versions in force at its admission, so affected chains stay identifiable; the response is deliberately not built in P1, since the condition cannot arise before the first such change. Exposure does not compound |
| Two managed identifier profiles coexist, and a reader misjudges which one an identifier is under | A consumer treats a minor-bearing identifier as a floating channel, or a major-only one as a pinned snapshot | The distinction is legible in the identifier rather than in registry state, and the shapes cannot mix within one major, so no major is ambiguous |
| A production consumer depends on an unstable Type Schema, whose owner then reshapes it | Stored domain data stops conforming to its own type, with no registry event to have warned anyone | Partly accepted. The quarantine rule keeps the risk out of every stable contract and the identifier makes it legible; P1 cannot refuse the dependency on the read path, since managed tenant enablement is P2. Residual exposure is smaller than deletion of a stable type under live conforming data, which `cpt-cf-types-registry-fr-lifecycle` already permits |

## 13. Open Questions

Unresolved **requirement** questions — scope, policy, and what the product owes. A question leaves this table once its answer has a home in a requirement, in DESIGN, or in an ADR. Unmade **design** decisions live in [DESIGN §4, Open questions](./DESIGN.md#open-questions) as `D1`, `D2`, …; a question moves there once what remains of it is a construction decision.

Entry numbers are stable and never reused, so a gap marks a closed question rather than a missing one.

| # | Question | Affects | Recorded in |
|---|----------|---------|-------------|
| 1 | **Who marks an entity deprecated, when, and what the mark affects.** ADR-0008 settled that deprecation is authored rather than derived from publishing a successor, and deferred it for want of a named consumer. Open: which actor may deprecate, whether the act is discretionary, and what a consumer is expected to do on seeing a mark that leaves the entity fully usable — and, once the concept exists, whether a source-asserted deprecation is relayed. The P1 answer to that last part is closed: exposed as `ACTIVE`, not relayed | `cpt-cf-types-registry-fr-lifecycle` | ADR-0008 |
| 2 | **What the platform must expose about federation failures and about its own mutations.** Federation fails closed, so a source outage surfaces as a failed operation — but nothing requires naming which source failed, to which actor (itself a disclosure decision on the tenant plane), or how a chronically unhealthy source reaches an operator rather than only the caller it broke. Separately, §4.2 keeps an audit product out of scope while §5 still requires operation and audit records, without stating their content, readership, or retention | all federation requirements, `cpt-cf-types-registry-fr-registry-source-routing` | not yet recorded; DESIGN §3.3, *Registry Source Plugin contract*, notes the gap under *Federation observability* |
| 3 | **Which transitions casting must support.** Settled: an established, unforced transition between two minors of one stable major, presentable as an OP#9 result. Open: whether the requirement reaches transitions outside OP#9, each doubtful for its own reason — two majors of one family are incompatible by construction, two content revisions of one entity are not addressable by a consumer (ADR-0005), and a derived type needs no transformation to its base | `cpt-cf-types-registry-fr-casting` | this PRD, section 5.3 |
| 4 | **Whether a tenant's move within the hierarchy may change what it sees and who sees what it owns.** Relocation changes the visible audience in both directions, with no registry mutation behind it and no event a consumer could observe. Open: whether that is an acceptable outcome of an account-management operation and, if not, what the registry owes — refusing the move, reporting affected entities, or migrating. Out of scope for P1 per ADR-0009 | `cpt-cf-types-registry-fr-tenant-ownership`, `cpt-cf-types-registry-fr-tenant-availability` | ADR-0009; the design consequence of bringing it into scope is recorded in ADR-0010 |
| 5 | **How the entities of an offboarded tenant are retired.** Ordinary deletion belongs to the owning tenant, ADR-0013 requires `DELETED` before purge, and the platform plane cannot author in a tenant's place — so once the owner is gone nobody can retire its entries. The answer turns on account-management semantics the registry does not own, and must also decide what happens to dependents in other tenants | `cpt-cf-types-registry-fr-tenant-ownership`, `cpt-cf-types-registry-fr-lifecycle` | not yet recorded; adjacent to entry 4 |
| 6 | **Whether an Alias may target another Alias, and whether an admitted Alias may be retargeted.** Neither follows from the Alias identity model. Chaining makes the Alias-to-target relation transitive, and ADR-0010 classifies it as availability-blocking, so a chain propagates unavailability along its whole length. Retargeting changes what an already issued Registry Reference resolves to — an effect otherwise permitted only under purge — so allowing it means stating in the contract that an Alias reference is less permanent than an entity reference | `cpt-cf-types-registry-fr-aliasing`, `cpt-cf-types-registry-fr-id-resolution` | ADR-0001; DESIGN D2 holds the remaining construction half |

## 14. Traceability

- **Design**: [DESIGN.md](./DESIGN.md)
- **ADRs**: [ADR/](./ADR/)

## 15. References

- **GTS spec**: [Global Type System](https://github.com/globaltypesystem/gts-spec)
- **ToolKit**: [docs/toolkit_unified_system/README.md](../../../../docs/toolkit_unified_system/README.md)
- **ToolKit plugins**: [docs/TOOLKIT_PLUGINS.md](../../../../docs/TOOLKIT_PLUGINS.md)
