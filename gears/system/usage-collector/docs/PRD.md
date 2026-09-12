Created:  2026-02-25 by Virtuozzo International GmbH
Updated:  2026-09-10 by Virtuozzo International GmbH

# PRD — Usage Collector

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
  - [5.1 Usage Ingestion](#51-usage-ingestion)
  - [5.2 Aggregation Fold](#52-aggregation-fold)
  - [5.3 Attribution & Isolation](#53-attribution--isolation)
  - [5.4 Pluggable Storage](#54-pluggable-storage)
  - [5.5 Usage Query & Aggregation](#55-usage-query--aggregation)
  - [5.6 Corrections](#56-corrections)
  - [5.7 Usage Record Typing](#57-usage-record-typing)
  - [5.8 Data Classification](#58-data-classification)
  - [5.9 Billing Integration](#59-billing-integration)
- [6. Non-Functional Requirements](#6-non-functional-requirements)
  - [6.1 Gear-Specific NFRs](#61-gear-specific-nfrs)
  - [6.2 NFR Exclusions](#62-nfr-exclusions)
- [7. Public Library Interfaces](#7-public-library-interfaces)
  - [7.1 Public API Surface](#71-public-api-surface)
  - [7.2 External Integration Contracts](#72-external-integration-contracts)
  - [7.3 Endpoints Summary](#73-endpoints-summary)
- [8. Use Cases](#8-use-cases)
- [9. Acceptance Criteria](#9-acceptance-criteria)
- [10. Dependencies](#10-dependencies)
- [11. Assumptions](#11-assumptions)
- [12. Risks](#12-risks)
- [13. Open Questions](#13-open-questions)
- [14. Traceability](#14-traceability)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

A usage metering gear for collecting **Usage Records** from platform services and providing raw reads, derived aggregate views, and a replay-safe usage feed to clients. The Usage Collector is the centralized product surface for platform usage data: it accepts **Usage Records**, retains them durably, and serves the public read paths downstream consumers need.

### 1.2 Background / Problem Statement

Platform services need a centralized place to report resource consumption (API calls, AI tokens, storage bytes, compute hours) so that downstream systems (billing, quota reporting, dashboards) can operate on consistent data. Without a central usage gear, each consumer implements its own collection logic, leading to inconsistent data, duplicated effort, and no single source of truth.

The Usage Collector addresses this by accepting **Usage Records** from calling gears and providing raw query, aggregate query, and feed APIs to consumers. Business logic (pricing, billing rules, invoice generation, quota enforcement decisions) remains the responsibility of downstream consumers.

### 1.3 Goals (Business Outcomes)

- **Centralized metering**: All platform services that measure resource consumption report to a single authoritative store, eliminating per-service tracking implementations and data inconsistencies across the platform.
- **Operator self-service for new meters**: Platform operators can register a new billable **Usage Record GTS type** (e.g., GPU hours, custom credit units) without code changes or service redeployment to the Usage Collector, supporting rapid product iteration. This concerns only the GTS types usage records are metered against; it says nothing about the other GTS types the platform defines.
- **Downstream consumers need no duplicate usage infrastructure**: Charging consumers read the replay-safe usage feed, while quota, dashboard, and reconciliation consumers obtain derived aggregate views directly from the Usage Collector within their published freshness bounds.
- **Developer integration efficiency**: Platform developers can integrate a service with the SDK or REST API using published examples and receive actionable validation errors during ingestion.
- **Operator support readiness**: Platform operators can diagnose common ingestion, authorization, and storage-extension readiness problems using self-service documentation and standard service health information.

**Success Metrics**:

| Goal                                           | Measurable Success Criterion                                                                                                                                                                                         | Baseline                                                                                                                | Target                                                                                                                           | Timeframe                                                                                             |
| ---------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| Centralized metering                           | Existing platform services with billable operations integrated with Usage Collector as the authoritative usage source                                                                                                | No authoritative platform-wide usage source; billable services use per-service or consumer-specific tracking            | 100% of existing billable platform services integrated; zero per-service custom metering implementations remain for launch scope | By first production deployment; verified again within 30 calendar days after launch                   |
| Operator self-service                          | Time to register a new billable **Usage Record GTS type** and emit the first accepted **Usage Record** without code changes or service redeployment                                                                             | New billable usage dimensions require service-specific coordination outside Usage Collector                             | ≤ 5 minutes from authorized API request to first accepted **Usage Record** for a valid **Usage Record GTS type**                            | Available at first production deployment and sustained in monthly release-readiness checks            |
| Downstream consumers need no duplicate usage infrastructure | Registered launch consumers serve charging feed and primary aggregation use cases through Usage Collector public read paths                                                                                          | Billing, quota, and dashboard consumers require separate feed or aggregation paths, or cannot use one authoritative usage source | 0 downstream-maintained usage-feed or aggregation stores for launch-scope billing, quota, and dashboard use cases                | By first production deployment; verified during the first 90 calendar days after launch               |
| Developer integration efficiency               | Platform developer can use SDK or REST examples to submit a valid **Usage Record** in a clean service integration                                                                                                    | No shared Usage Collector integration guide or sample flow exists                                                       | First successful ingestion in ≤ 30 minutes for a developer familiar with platform auth and tenant concepts                       | Documentation and examples ready before production release candidate                                  |
| Operator support readiness                     | Platform operator can identify the owner-facing cause category for common failures: authn/authz denial, unregistered GTS type, metadata limit rejection, storage-extension readiness, and query-latency breach | Troubleshooting depends on gear maintainer assistance and ad hoc log review                                             | ≥ 90% of sampled common failure cases resolved to a documented cause category without maintainer escalation                      | Runbook complete before production release candidate; sampled during each quarterly operations review |

### 1.4 Glossary

| Term                   | Definition                                                                                                                                                                                                                                                                                                                                                                                                        |
| ---------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Usage Record           | A single data point representing resource consumption by a tenant, with a finite signed decimal quantity and the interval `[window_start, window_end)` it covers (equal bounds for a point event), attributed to a registered **Usage Record GTS type**                                                                                                                                                                       |
| Usage Record GTS type  | The GTS type a **Usage Record** is metered against — the meter it belongs to. Registered in the platform `types-registry` gear, it carries the aggregation fold, canonical metering unit, metadata surface, and retention policy that govern every record referencing it. The Usage Collector defines no type of its own and holds no catalog of them.                                                             |
| Aggregation Fold       | The single aggregation a GTS type declares — `SUM`, `COUNT`, `MAX`, `MIN`, or `LATEST`. It is what the aggregate query path serves for that meter, and what a consumer reads to learn whether the meter's quantities are additive.                                                                                                                                           |
| Idempotency Key        | A client-provided identifier that makes at-least-once processing safe: an exact-equality re-submission under the same key is silently absorbed (no duplicate **Usage Record**), while a same-key submission whose content differs is surfaced as a conflict rather than silently dropped. Both outcomes are bounded by the **idempotency horizon**.                                                                |
| Operational Replay Horizon | How far back a charging consumer may rewind its feed cursor and still be **guaranteed** service — the maximum age of usage it can be required to re-read and re-process. It is the guaranteed window rather than the refusal boundary. Beyond it service is plugin-dependent, and a cursor older than what the store retains is refused (`cpt-cf-usage-collector-fr-billing-retention-floor`). A deployment parameter, 35 days at launch, and one of the two terms of the retention floor. Distinct from the recovery *speed* bounded by `cpt-cf-usage-collector-nfr-replay-throughput`.                     |
| Idempotency Horizon    | How long the gear guarantees duplicate suppression for a given **Usage Record**: at least the referenced GTS type's retention policy, measured from the covered period. It is never reached from the ingestion side: the backfill window bounds an admissible covered period strictly inside it, so a submission old enough to have outlived its counterpart is refused on that bound before deduplication is consulted (`cpt-cf-usage-collector-fr-idempotency`).                                                             |
| Usage Collector Plugin | A storage extension selected by operators to provide the persistence and query capability behind the Usage Collector                                                                                                                                                                                                                                                                                              |
| Record Metadata        | The per-GTS-type extension surface of a **Usage Record**: a JSON object whose admissible properties are declared by the referenced GTS type, allowing usage sources to include context-specific properties (e.g., LLM model name, token category, geographic region) that are opaque to the Usage Collector and interpreted by downstream consumers                                                     |
| Invalidation           | An appended entry referring to one previously accepted **Usage Record**, withdrawing it: no aggregation counts the withdrawn measurement, while both entries remain on the ledger. It is the only correction the gear offers, and it adjusts no quantity: it copies the withdrawn one rather than negating or replacing it.                                                    |
| GTS                    | Global Type System — the platform type and identifier system, in which a **Usage Record GTS type** is defined, and which the platform also uses for registry/orchestration dependencies outside the Usage Collector PRD boundary                                                                                                                                                                                                          |
| Types Registry         | The platform `types-registry` gear, which holds GTS type declarations. It owns the catalog of usage GTS types: declarations are registered, amended, and withdrawn there                                                                                                                                                                                                                                             |
| PDP                    | Policy Decision Point — the platform authorization service that gates every operation in this PRD.                                                                                                                                                                                                                                                                                                                |
| SecurityContext        | A platform-resolved structure carrying the authenticated caller's identity; supplied to the gear by the platform — never accepted from the payload.                                                                                                                                                                                                                                                               |
| Audit Trail            | The combination of platform gateway access logs, platform authentication and PDP decision logs, and platform audit infrastructure that records authentication, authorization, ingestion, query, and operator-write outcomes for non-repudiation and forensic purposes. The Usage Collector contributes correlation identifiers to this trail but does not host its own audit log in v1. |
| PII                    | Personally identifiable information — any information relating to an identified or identifiable natural person. Within the Usage Collector boundary the gear handles only opaque platform identifiers; resolution of those identifiers to natural persons is owned by the platform identity layer.                                                         |
| SPI                    | Service Provider Interface — the storage-plugin extension contract; distinct from the SDK trait and the REST API.                                                                                                                                                                                                                                                                                                 |

## 2. Actors

### 2.1 Human Actors

#### Platform Operator

**ID**: `cpt-cf-usage-collector-actor-platform-operator`

- **Role**: Deploys and configures the usage collector gear, selects storage backend, monitors system health.
- **Needs**: Ability to choose and configure storage backends without code changes.

#### Platform Developer

**ID**: `cpt-cf-usage-collector-actor-platform-developer`

- **Role**: Integrates platform services with the Usage Collector using the SDK or API to emit usage data.
- **Needs**: Well-documented SDK for emitting usage data with minimal integration effort.

#### Tenant Administrator

**ID**: `cpt-cf-usage-collector-actor-tenant-admin`

- **Role**: Queries raw and aggregated usage data for their tenant.
- **Needs**: Access to raw and aggregated **Usage Records** filtered by GTS type, subject, and resource for their tenant only, with time-range filtering.

### 2.2 System Actors

#### Usage Source

**ID**: `cpt-cf-usage-collector-actor-usage-source`

- **Role**: Any authenticated system that produces **Usage Records**.

#### Usage Consumer

**ID**: `cpt-cf-usage-collector-actor-usage-consumer`

- **Role**: Any system that reads usage data, including charging consumers through the feed and quota, dashboard, or reconciliation consumers through raw or aggregate query paths.

#### Storage Backend

**ID**: `cpt-cf-usage-collector-actor-storage-backend`

- **Role**: The underlying data store (e.g., ClickHouse or TimescaleDB) that persists **Usage Records**.

#### Types Registry

**ID**: `cpt-cf-usage-collector-actor-types-registry`

- **Role**: The platform `types-registry` gear. Holds the GTS type declarations the Usage Collector validates against — their aggregation fold, canonical metering unit, metadata surface, and retention policy — and owns their lifecycle. The Usage Collector resolves declarations from it and keeps no catalog of its own.

**Actor Permissions** (shared across human and system actors):

| Actor                                             | Permitted Operations                                                                                                                                                                                                                                                                                                                                                                                                                                                  | Denied by Default                                                                                                                                                                                                                                                     |
| ------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cpt-cf-usage-collector-actor-platform-operator`  | Append invalidation entries for **Usage Records** the operator is PDP-authorized to correct, through the same ingestion path used by any usage source                                                                                                                                                                                                                                                                                                                | Querying or correcting **Usage Records** belonging to any tenant without an explicit security context                                                                                                                        |
| `cpt-cf-usage-collector-actor-platform-developer` | Emit **Usage Records** for GTS types the calling gear is PDP-authorized to emit, within the calling gear's authorized tenant scope (the calling-gear identity is derived from the platform-resolved `SecurityContext`)                                                                                                                                                                                                                                          | Emitting **Usage Records** for GTS types outside the calling gear's PDP-authorized set; attributing **Usage Records** to subjects or resources outside the authorized scope                                                                                     |
| `cpt-cf-usage-collector-actor-tenant-admin`       | Query aggregated and raw **Usage Records** scoped to their own tenant                                                                                                                                                                                                                                                                                                                                                                                                 | Accessing usage data of any other tenant                                                                                                                                                                                                                              |
| `cpt-cf-usage-collector-actor-usage-source`       | Emit **Usage Records** for registered GTS types; the scope of permitted target tenants, resources, and GTS types is enforced by the platform PDP at emit time — the caller must be PDP-authorized for the tenant supplied in the **Usage Record** (covering both same-tenant and parent→subtenant scenarios), the supplied resource, and the referenced GTS type, with the calling-gear identity carried in the platform-resolved `SecurityContext` | Emitting **Usage Records** attributed to tenants or resources outside the PDP-authorized scope; emitting **Usage Records** referencing GTS types outside the PDP-authorized set; emitting **Usage Records** referencing GTS types that are not registered |
| `cpt-cf-usage-collector-actor-usage-consumer`     | Read feed, raw, and aggregate usage data scoped to the authenticated tenant; subject to PDP constraint filters                                                                                                                                                                                                                                                                                                                                                         | Accessing cross-tenant data                                                                                                                                                                                                                                           |
| `cpt-cf-usage-collector-actor-storage-backend`    | Receive and persist **Usage Records** forwarded by the gateway plugin; respond to query operations initiated by the plugin                                                                                                                                                                                                                                                                                                                                            | Direct access from any actor other than the authorized plugin instance                                                                                                                                                                                                |
| `cpt-cf-usage-collector-actor-types-registry`     | Serve GTS type declarations in response to resolution requests initiated by the Usage Collector                                                                                                                                                                                                                                                                                                                                                                 | N/A — passive service; does not initiate operations on the Usage Collector, and holds no **Usage Record** data                                                                                                                                                        |

Authorization is enforced via the platform PDP (`authz-resolver`) on all read and write operations. Unauthenticated requests are rejected before any authorization check. Failures result in immediate rejection with no partial operation (fail-closed).

## 3. Operational Concept & Environment

### 3.1 Gear-Specific Environment Constraints

- All **Usage Record** state lives in the operator-selected storage plugin. The gear itself needs one small database of its own, sized by the number of declared GTS types rather than by usage volume; DESIGN states what it holds.
- Deployment, observability, and storage-tier HA are governed by platform operations and the active plugin's deployment guide.

## 4. Scope

### 4.1 In Scope

- **Usage Record** ingestion from platform services
- A single declared aggregation fold per GTS type, drawn from a closed set
- Signed **Usage Record** quantities, admitted on every GTS type under every fold
- Per-tenant usage attribution, PDP-authorized at emit time
- Per-subject (user, service account) usage attribution, PDP-authorized at emit time
- Per-resource usage attribution
- Ingestion authorization via the platform PDP
- Idempotency via client-provided keys
- Pluggable storage backend selection
- Query API for aggregated usage data with time-range filtering and grouping
- Tenant isolation on all read and write operations
- Per-**Usage Record** metadata constrained by the metadata surface the GTS type's schema declares
- Resolution of GTS type declarations from the platform `types-registry`, and validation of every **Usage Record** against the resolved declaration
- Caller authentication is performed by the platform gateway upstream of the gear
- Delegated audit trail through platform gateway access logs and platform audit infrastructure, with gear-emitted correlation identifiers on every API operation
- Custodianship of tenant usage data under PDP-mediated read and write boundaries, including tenant-owner, operator-steward, and gear-custodian role distinctions
- Interval time attribution: every **Usage Record** covers `[window_start, window_end)`, with equal bounds expressing a point event, alongside a gear-assigned `accepted_at`
- A system-assigned, offline-reproducible **Usage Record** identity, addressable for point lookup and correction references
- A canonical metering unit bound to every GTS type, and published additivity per declared aggregation fold
- A reason code on every correction
- A replay-safe, pull-based usage feed with per-GTS-type subscription, snapshot-consistent cursors, and watermarks
- Per-GTS type retention policy honoured by the storage deployment
- Dedicated origin-flagged bulk backfill, isolated from live ingestion
- Ingestion quotas per calling gear and per (calling gear, tenant)
- Per-scope reconciliation metadata and watermarks

### 4.2 Out of Scope

- **Business Logic**: Pricing, rating, billing rules, invoice generation, quota enforcement decisions — responsibility of downstream consumers
- **GTS type lifecycle**: registration, amendment, and withdrawal of GTS type declarations are owned by the platform `types-registry` gear ([§5.7](#57-usage-record-typing)). The Usage Collector resolves declarations and validates against them; it exposes no GTS type surface of its own and holds no catalog of its own
- **Commercial identity resolution**: subscription, SKU, and the payer/seller tenant axes are **not** carried or resolved by the gear; downstream consumers resolve them from the **Usage Record**'s tenant/resource attribution
- **Multi-Region Replication**: Deferred to future phase
- **Individual Event Amendment or Retirement**: no operation modifies an accepted **Usage Record** in place — neither an operator-initiated property update nor a retirement or deactivation of the record. Corrections are appended as invalidation entries ([§5.6](#56-corrections))
- **Signed compensating entries**: no correction adjusts a quantity. A real decrease in consumption is an ordinary **Usage Record** carrying a negative quantity ([§5.2](#52-aggregation-fold)); the only correction is the withdrawal of a whole **Usage Record** ([§5.6](#56-corrections))
- **Bulk or range invalidation**: v1 withdraws one **Usage Record** per invalidation entry; withdrawing many is submitted as many entries through the ordinary batched ingestion path ([§13](#13-open-questions))
- **Integration of a level series into a period quantity**: the gear offers no fold that integrates. A meter whose consumption is naturally a level is pre-integrated at the emitter into an accrued quantity ([§5.2](#52-aggregation-fold))
- **Import or correction beyond the backfill window**: the configured window (default 90 days) is a hard bound on every surface. v1 offers no elevated-authorization override reaching past it, so usage older than the window can be neither imported nor withdrawn ([§5.9](#59-billing-integration), [§13](#13-open-questions))
- **Audit Events**: Structured audit-event emission to a platform `audit_service` for operator-initiated writes remains deferred; the platform gateway/PDP access trail plus gear-emitted correlation IDs remain the audit surface

## 5. Functional Requirements

### 5.1 Usage Ingestion

#### Usage Record Ingestion

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-ingestion`

The system **MUST** accept **Usage Records** from authenticated usage sources. Each **Usage Record** represents a single measurement of resource consumption attributed to a tenant.

- **Rationale**: Centralizing usage ingestion ensures all downstream consumers operate on the same data.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`

#### Idempotent Ingestion

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-idempotency`

The system **MUST** require a client-provided idempotency key on every **Usage Record**. The system **MUST** reject any **Usage Record** submitted without an idempotency key with an actionable error. The dedup identity of a submission is its idempotency key together with the tenant, the GTS type, and the window bounds (see `cpt-cf-usage-collector-fr-usage-windows` and `cpt-cf-usage-collector-fr-record-identity`); submissions that differ in any part of that identity are distinct **Usage Records** rather than duplicates or conflicts. The same requirement and the same dedup identity govern an invalidation entry, whose window bounds are those of the **Usage Record** it withdraws ([§5.6](#56-corrections)). When a submission matches a previously accepted **Usage Record** on the dedup identity **and** every caller-supplied field is identical, the system **MUST** silently deduplicate the submission (no error, no duplicate **Usage Record**); this is the exact-equality retry case. When a submission matches on the dedup identity but **any** caller-supplied field differs from the stored **Usage Record** — including a metadata-only difference — the system **MUST** reject the submission with an actionable conflict error and **MUST NOT** silently drop the second write.

**Key construction.** The key **MUST** distinguish what the dedup identity does not: two resources or subjects measured over one period, two **Usage Records** covering one period, and an invalidation entry from its target ([§5.6](#56-corrections)). A retry **MUST** repeat its submission's key exactly, since a retry under a fresh key is a new **Usage Record** rather than a duplicate.

**Idempotency horizon.** Both outcomes above hold only while the **Usage Record** a submission would match is still retained. The dedup identity of an accepted **Usage Record** remains visible to subsequent submissions for **at least** as long as the referenced GTS type's retention policy retains that record (`cpt-cf-usage-collector-fr-billing-retention-floor`, `cpt-cf-usage-collector-nfr-query-freshness`). That span is a guaranteed floor rather than an exact boundary: a deployment **MUST NOT** be required to retain dedup identities beyond the data they protect, and a purge that runs on the storage plugin's own schedule can leave an aged record, and its dedup identity with it, in place for longer. The horizon is therefore **per-meter** rather than gear-wide, and — because retention runs from the covered period rather than from the acceptance instant — the horizon remaining to any submission is the referenced GTS type's retention policy less the age of the period that submission covers.

**No admissible submission outlives the horizon.** The covered period is both what the dedup identity carries and what the ingestion bounds check, and the retention floor keeps the backfill window strictly inside retention (`cpt-cf-usage-collector-fr-billing-retention-floor`). A submission old enough for its counterpart to have been purged is therefore refused on the window bound before deduplication is consulted, and no caller carries a post-horizon obligation. A charging consumer still deduplicates on **Usage Record** identifier, for the at-least-once feed delivery that is its own reason (`cpt-cf-usage-collector-fr-billing-usage-feed`).

Three paths approach the boundary, and none of them crosses it:

- **Retry.** A live-path retry re-submits a covered period emitted in near-real time, so it holds substantially the whole retention policy as horizon. Retry safety on the live path is not practically constrained by the boundary.
- **Replay.** Feed replay never crosses the boundary: a cursor older than the retention floor **MUST** be refused with an actionable error rather than served as a silently truncated range (`cpt-cf-usage-collector-fr-billing-usage-feed`), so a consumer cannot replay past the horizon and reach a processed set different from the one it reached live.
- **Backfill.** A backfilled **Usage Record** arrives with its horizon already partly spent, since retention runs from the period it covers rather than from its import. What remains is a full replay horizon — 35 days at the launch defaults — and it cannot run out while the import is still possible, so a re-run either lands inside the horizon and is deduplicated, or is refused on the window bound.

- **Depends on**: `cpt-cf-usage-collector-fr-record-identity`, `cpt-cf-usage-collector-fr-billing-retention-floor`
- **Rationale**: Client-side retries on transient failures can produce duplicate submissions; deduplication prevents incorrect aggregations. Under a `SUM` fold, a retry of a keyless quantity inflates the accrued total without any means of detection or correction. Under any other fold, duplicate observations can still poison downstream consumers that derive counts, distinct observation windows, or rate-of-change signals from raw **Usage Records**. Requiring an idempotency key on every emission eliminates this data integrity risk at the calling gear, keeps the ingestion contract free of any fold-dependent special case, and lets calling gears adopt a single retry pattern across all GTS types they emit. Splitting the same-key outcome is deliberate: an exact-equality retry is the benign at-least-once case and remains safe to absorb silently, but a key reused with different content is a caller bug. Surfacing that divergence as a conflict rather than silently dropping the second write protects billing-correctness and other downstream consumers from data that would otherwise be lost without any signal. Stating the horizon here, rather than leaving it implicit in a retention policy and a consistency floor, is what makes the idempotency contract self-contained for billing: an emitter or charging consumer reading this requirement learns what the gear suppresses and why the ingestion bounds keep every admissible submission inside that span, instead of reconstructing the ordering from two other requirements and coding defensively against a case those bounds exclude.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`

#### Per-Record Extensible Metadata

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-record-metadata`

Metadata is the per-GTS-type extension surface of a **Usage Record**, and its admissible properties are declared by the referenced GTS type. An invalidation entry carries the metadata of the **Usage Record** it withdraws and is bound by this requirement identically (`cpt-cf-usage-collector-fr-record-invalidation`). Every metadata key supplied on a **Usage Record** **MUST** be a property that schema declares. Undeclared metadata keys **MUST NOT** be accepted — the system rejects such **Usage Records** at the gateway with an actionable validation error. The system **MUST** enforce a configurable maximum metadata size and **MUST** reject **Usage Records** exceeding the configured limit with an actionable error.

The metadata surface is **closed**: there is no free-form remainder, no open-extras escape hatch, and no silently-preserved undeclared properties. Downstream consumers (billing, reporting, analytics) extract declared properties by name; the Usage Collector's query surface addresses the same declared properties.

**Value typing.** In v1 all metadata values are treated as strings on the wire and at rest, irrespective of any richer typing the GTS type schema is capable of expressing. The schema is normative for *which* properties may appear; it is not yet normative for their value types. Widening to schema-typed values is deferred, because value typing propagates into the storage contract, the grouping and filtering surface below, and the Plugin SPI, and none of those are narrowed by the v1 consumer set.

**Grouping and filtering contract.** The declared metadata properties are the addressable dimensions of the query surface, and this **MUST** hold on both read paths: the system **MUST** permit callers to group and equality-filter on any declared property of the queried GTS type, in any combination and any order, alongside the fixed **Usage Record** fields. Admissibility is computed per request from the queried GTS type's resolved schema; a request naming any property that schema does not declare **MUST** be rejected with an actionable validation error **before** dispatch to the storage plugin, rather than silently yielding an empty or absent dimension. The result is that the groupable surface is exactly the declared set — bounded and known at declaration time — with no second list to keep in step with it.

Cardinality is bounded by a configurable aggregation result limit and by the time range the aggregate path requires — the limit's value and the range obligation are settled in DESIGN — not by restricting which declared properties may be grouped. Where an individual declared property is unsuitable for grouping (a high-cardinality correlation identifier, for example), that is a property of the key and belongs on the key's own declaration.

- **Depends on**: `cpt-cf-usage-collector-fr-usage-type-declaration`, `cpt-cf-usage-collector-fr-usage-type-resolution`
- **Rationale**: Different usage sources need to attach context-specific properties to **Usage Records** (e.g., LLM model name, token type, request category, geographic region) that enable downstream reporting and analytics. Carrying the admissible set as the GTS type schema's own extension surface, rather than as a separate key list the gear maintains alongside it, means there is one declaration to author and one to read: closure follows from the schema declaring no additional properties, and the groupable surface is read off the same document. Deferring value typing keeps the v1 storage and query contract unchanged while leaving the schema free to tighten later without a second migration of the declaration model.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-platform-developer`

### 5.2 Aggregation Fold

#### Declared Aggregation Fold

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-aggregation-fold`

Every GTS type **MUST** declare exactly **one** aggregation fold, drawn from the following closed set. The declared fold is what tells a consumer how that meter's quantities relate to one another:

| Fold     | The quantity of a **Usage Record** is                                     | Additive across disjoint periods |
| -------- | ------------------------------------------------------------------------- | -------------------------------- |
| `SUM`    | the amount accrued over the period the **Usage Record** covers            | yes                              |
| `COUNT`  | not read — the fold counts **Usage Records**, and a withdrawn pair counts none | not applicable              |
| `MAX`    | one observation; the fold reports the largest in range                    | no                               |
| `MIN`    | one observation; the fold reports the smallest in range                   | no                               |
| `LATEST` | one observation; the fold reports the one with the greatest covered-period end in range, ties broken by the greatest acceptance sequence | no                               |

**The quantity under `COUNT`.** A `COUNT` meter's records still carry a quantity, because the record shape is the same for every GTS type, but it means nothing: under `COUNT` one record is one event, and the fold reads only how many there are. An emitter **SHOULD** send `1`, and a consumer **MUST NOT** read a `COUNT` meter's quantity as a measurement. Ingestion does not enforce the convention — it never consults the declared fold, so a `COUNT` meter's record is accepted or rejected on exactly the terms any other record is (`cpt-cf-usage-collector-fr-record-quantity`).

**What `COUNT` counts.** The fold **MUST** count the **Usage Records** in range that no accepted invalidation entry withdraws, and **MUST NOT** count an invalidation entry or the record it withdraws. This is the withdrawal exclusion read under `COUNT` (`cpt-cf-usage-collector-fr-record-invalidation`): a withdrawn record counts 1 and its invalidation counts 1, so a fold admitting both returns 2 where the answer is 0.

**Ties under `LATEST`.** Two **Usage Records** of one meter can share a covered period, since they are distinguished by idempotency key as well as period (`cpt-cf-usage-collector-fr-record-identity`), and `LATEST` selects a record rather than reducing values — so an undefined tie would let two consumers read the same range and obtain different quantities. The order is therefore total: the greatest covered-period end, then the greatest acceptance sequence. That terminates because the acceptance sequence is monotonic per (tenant, GTS type) (`cpt-cf-usage-collector-fr-billing-usage-feed`), and an aggregation group is always inside one such scope — the aggregate path serves exactly one GTS type and groups within a tenant. `MAX` and `MIN` need no such rule: a tie there returns the same value whichever record supplied it.

The fold is a property of the GTS type, resolved through the **Usage Record**'s GTS type reference. It is **not** carried per **Usage Record**, and the system **MUST NOT** infer it from the shape of the GTS type identifier. A caller does not choose it: the aggregate query path serves the declared fold and no other (`cpt-cf-usage-collector-fr-query-aggregation`).

**Additivity is normative.** A consumer reading **Usage Records** directly — through the raw query path or the usage feed — **MUST** derive period consumption by summing quantities only where the declared fold is `SUM`, and **MUST** leave out any **Usage Record** withdrawn by an accepted invalidation entry along with the invalidation itself ([§5.6](#56-corrections)). Under every other fold the quantities are observations rather than accrued amounts, and summing them is invalid. This is the whole of what a consumer needs in order to fold a meter correctly, and it is why a consumer that never calls the aggregate path still resolves the declaration.

**Only `SUM` yields a chargeable period quantity.** `MAX`, `MIN`, and `LATEST` are descriptive: they characterise a series without producing an amount consumed over a period. A meter whose consumption is naturally a level — stored volume being the case that hits this first — **MUST** therefore be pre-integrated at the emitter into an accrued quantity carrying an accrued unit, such as `byte-hours`, and declared `SUM`. The gear offers no fold that integrates a level series, and `cpt-cf-usage-collector-fr-quantity-semantics` forbids it from integrating on any path.

The declared fold **MUST** be immutable for the lifetime of the GTS type, as a case of the declaration immutability rule in `cpt-cf-usage-collector-fr-usage-type-declaration`.

The system **MUST NOT** consult the declared fold on the ingestion path. No ingestion invariant depends on it: what a **Usage Record** must satisfy to be accepted is stated by `cpt-cf-usage-collector-fr-record-quantity` and the requirements it depends on, and the sign of a quantity is not among the constraints there.

- **Depends on**: `cpt-cf-usage-collector-fr-usage-type-declaration`, `cpt-cf-usage-collector-fr-usage-type-resolution`
- **Rationale**: A meter must yield one number for a period, or two consumers reading the same records can obtain two defensible answers and neither can say which one a charge derived from. Declaring the fold rather than accepting it per request is what makes that number determinate, and it is the reason the aggregate request carries no aggregation parameter. Binding the fold at declaration also lets a single declared attribute carry what a separate write-side classification of the meter used to: additivity follows from the fold, so there is one thing to declare and one to read rather than two that can disagree. The set is deliberately closed and small — adding a fold later is an additive change, while removing one is breaking — and it excludes any fold that cannot be computed from the stored quantities alone: an unweighted mean over irregularly spaced observations resembles the time-weighted mean that money would derive from without being it, and a distinct count is not re-aggregatable without either a mergeable sketch in every rollup or a full scan.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-usage-consumer`, `cpt-cf-usage-collector-actor-platform-operator`

### 5.3 Attribution & Isolation

#### Tenant Attribution

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-tenant-attribution`

1. The system **MUST** attribute every **Usage Record** to a tenant supplied by the caller in the request.
2. The system **MUST** authorize the caller's tenant attribution via the platform PDP before any **Usage Record** is accepted, verifying that the authenticated caller is permitted to emit **Usage Records** for the specified tenant. This covers both same-tenant emission and parent→subtenant scenarios (e.g., a platform-level metering agent collecting usage for resources owned by its subtenants).
3. The gateway **MUST** independently validate tenant attribution on ingest as a defense-in-depth check.

- **Rationale**: Requiring callers to supply the target tenant explicitly supports all emission scenarios — including remote forwarders and external systems that emit **Usage Records** on behalf of multiple tenants — through a single uniform path. PDP authorization remains the security boundary enforcing which tenants a given caller is permitted to report for.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`

#### Resource Attribution

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-resource-attribution`

Every **Usage Record** **MUST** be attributed to a specific resource instance within a tenant, identified by a resource ID and resource type. Resource attribution is mandatory; the system **MUST** reject **Usage Records** that omit either field.

- **Rationale**: Per-resource attribution enables granular billing, per-resource quota enforcement, and detailed usage analysis at the resource level. Mandatory attribution ensures downstream consumers always have a resource scope to aggregate and filter on, without needing to handle the absence of this field.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`

#### Subject Attribution

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-subject-attribution`

1. The system **MUST** support attributing **Usage Records** to a subject (user, service account, or other principal) within a tenant, identified by a caller-supplied subject ID and, when available, an optional subject type. Subject attribution is optional per **Usage Record** to accommodate system-level resource consumption not attributable to a specific subject (e.g., background jobs where per-user attribution is not meaningful); when subject attribution is supplied, the subject ID **MUST** be present, subject type omission is valid for systems without subject-type taxonomies, and a subject type **MUST NOT** be supplied without a subject ID.
2. When a subject is supplied, the system **MUST** authorize the caller's subject attribution via the platform PDP before any **Usage Record** is accepted, verifying that the authenticated caller is permitted to emit **Usage Records** attributed to the specified subject ID and, when supplied, subject type. When no subject ID is supplied, PDP subject validation is skipped.
3. The system **MUST NOT** derive subject identity from the caller's SecurityContext: subject attribution is always caller-supplied, never implicitly populated from the authenticated principal.

- **Rationale**: Per-subject attribution enables chargeback, per-subject quota enforcement, and visibility into which principals drive consumption within a tenant. Accepting the target subject explicitly from the caller — rather than implicitly from the caller's own SecurityContext — supports emission scenarios where the calling service attributes consumption to subjects other than itself (e.g., a service emitting per-user **Usage Records** on behalf of the users it serves, or a remote forwarder relaying **Usage Records** originally produced by multiple named subjects). PDP authorization remains the security boundary enforcing which subjects a given caller is permitted to report for, preventing spoofing.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`
- **Data Classification**: Subject IDs are opaque platform identifiers; PII handling is owned by the platform identity layer (see [§6.2](#62-nfr-exclusions) NFR Exclusions).

#### Tenant Isolation

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-tenant-isolation`

The system **MUST NOT** grant any caller access to a tenant's usage data — for reads or writes — without an explicit PDP authorization for that tenant. The system **MUST** treat every tenant scope independently: no caller is implicitly authorized for any tenant, and authorization for one tenant **MUST NOT** be inferred from authorization for another (sibling, parent, or child). Cross-tenant access is permitted only when the PDP explicitly authorizes the authenticated caller for the target tenant (e.g., a parent tenant administrator authorized to read its subtenants' usage). The system **MUST** fail closed on authorization failures.

- **Rationale**: Tenant data isolation is a security and compliance requirement, but parent→subtenant hierarchies and platform-level administrative roles legitimately require cross-tenant visibility. Anchoring isolation on PDP authorization keeps the security boundary precise while supporting the hierarchical scenarios the platform exposes (see `cpt-cf-usage-collector-fr-tenant-attribution`, `cpt-cf-usage-collector-fr-ingestion-authorization`).
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-usage-consumer`

#### Ingestion Authorization

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-ingestion-authorization`

1. The system **MUST** authorize each **Usage Record** emission before it is persisted. The security boundary for ingestion authorization is the PDP check on the caller's authenticated identity (including the calling-gear identity carried in the platform-resolved `SecurityContext`) against the supplied tenant, resource, and referenced GTS type.
2. The system **MUST** verify the caller is permitted to emit **Usage Records** attributed to the specified tenant and resource, against the calling-gear identity from `SecurityContext` and the referenced GTS type, before any **Usage Record** is accepted.
3. The system **MUST** validate that the referenced GTS type resolves to a registered declaration, rejecting **Usage Records** that reference an unknown GTS type (`cpt-cf-usage-collector-fr-usage-type-resolution`).
4. Authorization failures **MUST** be surfaced immediately to the caller before any domain operation is committed.
5. The system **MUST** fail closed: unauthorized **Usage Records** are never persisted, and there is no silent discard of denied emissions.

- **Rationale**: Anchoring authorization on the authenticated caller (with the calling-gear identity derived from `SecurityContext`) plus the caller-supplied attribution tuple (tenant, resource, GTS type) lets the PDP enforce per-caller emission scope without trusting any caller-supplied claim of "who is emitting". GTS type resolution preserves data quality by ensuring **Usage Records** reference known GTS types.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`

### 5.4 Pluggable Storage

#### Pluggable Storage Backend

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-pluggable-storage`

The system **MUST** support pluggable storage backends. Operators **MUST** be able to select the active backend without changing Usage Collector product behavior.

**Scope**: Pluggable storage covers **Usage Records** (ingestion, query, invalidation), reached through the storage plugin; details in DESIGN.

- **Rationale**: Pluggable storage avoids lock-in and allows operators to choose the backend that fits their needs, and keeps the storage plugin the single seam through which the gear reaches durable state.
- **Actors**: `cpt-cf-usage-collector-actor-platform-operator`, `cpt-cf-usage-collector-actor-storage-backend`

### 5.5 Usage Query & Aggregation

**The ledger and the derived view.** Accepted **Usage Records** are the Usage Collector's record of what was measured, and the raw query path and the usage feed serve them as persisted facts. The aggregate path is a **derived view** over those same records: it computes the declared fold and states nothing a consumer could not, in principle, compute from the records themselves. A charging consumer therefore reads the feed (`cpt-cf-usage-collector-fr-billing-usage-feed`), and the aggregate path serves consumers that tolerate bounded staleness and never compute a charge — dashboards, quota evaluation, and reconciliation. This split is what allows the aggregate path to be served from a pre-computed or materialised representation, at whatever freshness the active plugin publishes (`cpt-cf-usage-collector-nfr-query-freshness`, `cpt-cf-usage-collector-nfr-aggregate-freshness`), without weakening any guarantee a charging consumer depends on.

#### Aggregated Usage Query

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-query-aggregation`

The system **MUST** provide an API for querying aggregated usage data. Queries **MUST** support time-bounded aggregation for exactly one GTS type. The time range is mandatory and narrows every aggregation. Queries **MUST** allow consumers to narrow and group results by tenant, subject, and resource where authorized.

The aggregation applied is the **declared fold** of the queried GTS type, resolved from its declaration (`cpt-cf-usage-collector-fr-aggregation-fold`).

**One value per group.** The time range selects the entries an aggregation folds. It does not divide the result into sub-periods, and a time bucket is not a grouping dimension in v1. A consumer that needs a per-period series requests one aggregation per period, or reads the raw query path (`cpt-cf-usage-collector-fr-query-raw`) and folds the entries itself.

The system **MUST** authorize each query via the platform PDP. PDP-returned constraints define the authorization boundary and **MUST** be applied as query filters before execution. User-supplied filters (including `tenant`) **MUST** be applied in addition to PDP-returned constraints — they can only further narrow the result set, never widen it beyond the PDP-authorized scope. The system **MUST** fail closed on authorization failures (PDP denial or empty constraints).

**Invalidated records.** Withdrawal is a rule of the fold, not a deletion: within any aggregation, a **Usage Record** withdrawn by an accepted invalidation entry and the invalidation entry itself **MUST** each contribute nothing to the selected set (`cpt-cf-usage-collector-fr-record-invalidation`). Because an invalidation carries the covered period of the **Usage Record** it withdraws, both entries share one period end, so no requested range selects one of the pair without the other (`cpt-cf-usage-collector-fr-usage-windows`). The result therefore never depends on where the invalidation lands, no choice of placement is available that would change one, and no aggregation can count a withdrawn measurement.

**Stability after the backfill horizon.** No entry whose covered period ends further back than the configured backfill window is accepted on either ingestion path, and an invalidation carries the period of the **Usage Record** it withdraws. An aggregate over a period entirely older than the backfill window therefore cannot change at all: the window is a hard bound on every surface, and no ingestion path admits an entry that would alter such an aggregate (`cpt-cf-usage-collector-fr-backfill`). A consumer that closes its books on that horizon can rely on the figure it read.

- **Depends on**: `cpt-cf-usage-collector-fr-aggregation-fold`
- **Rationale**: Downstream consumers such as dashboards, quota evaluators, and reconciliation jobs need aggregated views without fetching and processing raw **Usage Records**. Restricting each aggregation to a single GTS type ensures the aggregated values share one unit and one fold — combining counts, byte volumes, or duration measures across different GTS types is meaningless and would mask data-quality issues. Serving only the declared fold is what makes "usage for this period" a single number per meter: a caller free to pick a different fold over the same records could obtain a second defensible answer, with nothing recording which one a charge derived from. It also removes an aggregation parameter, and with it a class of request that could previously be well-formed and semantically wrong. Product-level filtering and grouping still enable rich breakdowns within a GTS type while preserving PDP-authorized scope.
- **Actors**: `cpt-cf-usage-collector-actor-usage-consumer`, `cpt-cf-usage-collector-actor-tenant-admin`

#### Raw Usage Query

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-query-raw`

The system **MUST** provide an API for querying raw **Usage Records** as paged results. Queries **MUST** target exactly one GTS type, **MUST** support a mandatory time range, and **MUST** allow consumers to narrow results by tenant, subject, and resource where authorized.

The system **MUST** authorize each query via the platform PDP using the same decision and constraint-enforcement model as the aggregation query path: PDP-returned constraints define the authorization boundary, and user-supplied filters (including `tenant`) only further narrow the result set within that scope. The system **MUST** fail closed on authorization failures.

**Invalidated records.** Raw query is a ledger read path, not a derived view: a **Usage Record** withdrawn by an accepted invalidation entry, and the invalidation entry itself, **MUST** both be returned as persisted, the invalidation naming the record it withdraws (`cpt-cf-usage-collector-fr-billing-fields-on-read`). The path applies no fold, so it has none to correct; leaving a withdrawn pair out of a fold computed from these rows is the reader's obligation (`cpt-cf-usage-collector-fr-aggregation-fold`), and the aggregate path is where the gear discharges it (`cpt-cf-usage-collector-fr-query-aggregation`).

- **Rationale**: Some consumers need access to individual **Usage Records** for auditing, debugging, or dispute resolution. Restricting each query to exactly one GTS type, as on the aggregate path, means the returned rows share one unit and one declared metadata-key set, so the filterable and groupable surface is well defined per request (see `cpt-cf-usage-collector-fr-record-metadata`).
- **Actors**: `cpt-cf-usage-collector-actor-usage-consumer`, `cpt-cf-usage-collector-actor-tenant-admin`

### 5.6 Corrections

The Usage Collector is an append-only ledger: an accepted **Usage Record** is never rewritten, retired, or otherwise altered, and no operation on any surface changes a record that has already been accepted. A correction is expressed by appending an **invalidation** entry that refers to exactly one previously accepted **Usage Record**. An invalidation entry is submitted via the **same ingestion path** used for **Usage Records** (no dedicated correction endpoint, SDK method, or storage-plugin call exists), is attributed via the platform PDP on the caller's identity, and is protected by the existing mandatory idempotency key (cross-reference `cpt-cf-usage-collector-fr-idempotency`).

There is exactly **one** correction, and it withdraws a whole **Usage Record**. A correction asserts that the referenced measurement was never true; it adjusts no quantity, and the gear carries no signed compensating entry. The quantity an invalidation entry carries is a copy of the withdrawn one, restating what is being withdrawn — never a negation of it, and never a term any fold applies. A real decrease in consumption is not a correction at all but an ordinary **Usage Record** with a negative quantity, carrying no reference and no reason code and validated exactly as any other **Usage Record** is (`cpt-cf-usage-collector-fr-record-quantity`). The two cases therefore stay distinguishable: a negative measurement records something that happened, while an invalidation records that something did not.

Because a correction is itself an appended entry, the Usage Collector offers no reversal of a correction: an accepted invalidation is permanent, and both the invalidated **Usage Record** and its invalidation remain queryable so that correction history stays reconstructible.

#### Record Invalidation

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-record-invalidation`

The system **MUST** accept append-only **invalidation** entries that withdraw a previously accepted **Usage Record**. Withdrawal takes effect in the fold rather than on the ledger: once an invalidation is accepted, every aggregation **MUST** treat both the withdrawn **Usage Record** and the invalidation as contributing nothing, while both remain persisted and readable exactly as accepted.

**What an invalidation entry carries.** An invalidation entry travels the ingestion path a measurement of its covered period would travel, and is a **faithful copy** of the **Usage Record** it withdraws: every caller-supplied field — tenant, GTS type, resource, subject, covered period, quantity, and metadata — **MUST** equal the corresponding field of the target and is validated against it at ingestion. The departures from that copy are closed, and are exactly three: the entry **MUST** carry its own idempotency key distinct from the target's, **MUST** name the target by reference, and **MUST** carry a reason code (`cpt-cf-usage-collector-fr-invalidation-reason-code`). Naming the target is itself what marks the entry as an invalidation, so there is no separate marker a caller could set inconsistently with the reference it accompanies. The gear assigns it four server-side fields in its own right rather than by copy, as it does for any accepted entry. Those four are an identifier (`cpt-cf-usage-collector-fr-record-identity`), an acceptance instant (`cpt-cf-usage-collector-fr-usage-windows`), an acceptance-sequence position (`acceptance_sequence`), and the origin marker of the path it arrived on (`cpt-cf-usage-collector-fr-backfill`). The acceptance-sequence position is what gives the invalidation its own acceptance position on the feed (`cpt-cf-usage-collector-fr-billing-usage-feed`).

The copied quantity is an **echo, not a compensation**. It restates what is being withdrawn and is never negated or otherwise adjusted, so no signed compensating entry exists on any surface. It is also never a type signal: the reference to the withdrawn record is the sole discriminator, and no reader may infer a correction from a quantity's value or sign.

The system **MUST** enforce the following at ingestion before persistence:

- **Explicit reference**: an invalidation entry **MUST** be identified by the reference it carries to the record it withdraws, and **MUST NOT** be distinguished by the value of its quantity. Quantity cannot identify a correction, because zero and negative quantities are ordinary measurements (`cpt-cf-usage-collector-fr-record-quantity`) and because an invalidation echoes the quantity it withdraws rather than negating it. The reference and the reason code are both-or-neither: a submission carrying one without the other is malformed and rejected with an actionable error. A submission carrying neither is an ordinary **Usage Record** whatever its other fields.
- **Valid reference**: an invalidation entry **MUST** name an existing **Usage Record** by that record's identifier (`cpt-cf-usage-collector-fr-record-identity`). A reference resolving to nothing is rejected with an actionable error.
- **Faithful copy**: every caller-supplied field of an invalidation entry **MUST** equal the target's — tenant, GTS type, resource, subject (where presence against absence of a subject is itself a mismatch), covered period (zero-length where the target is a point event), quantity, and metadata. Any difference is rejected with an actionable error naming the field that differs. The copied period is also what makes the entry's own identity derivable (`cpt-cf-usage-collector-fr-record-identity`, `cpt-cf-usage-collector-fr-idempotency`).
- **No invalidation of an invalidation**: an invalidation entry **MUST NOT** refer to another invalidation entry.
- **At most one per record**: an invalidation entry referring to an already-invalidated **Usage Record** **MUST** be rejected with an actionable error. An exact-equality resubmission under the same idempotency key is absorbed as a duplicate rather than treated as a second invalidation (cross-reference `cpt-cf-usage-collector-fr-idempotency`).
- **Path-owned period bounds**: the covered period an invalidation copies **MUST** be validated against the bounds of the path it arrived on, exactly as an ordinary **Usage Record**'s period is (`cpt-cf-usage-collector-fr-live-future-time-bound`, `cpt-cf-usage-collector-fr-backfill`). The entry kind confers no wider retroactive reach. Withdrawing a **Usage Record** older than the live past tolerance therefore belongs on the backfill path, which the live-path rejection **MUST** name, and beyond that path's window it cannot be withdrawn at all, since no ingestion path admits the covered period a faithful copy would have to carry.
- **Reason code**: every invalidation entry carries a reason code (cross-reference `cpt-cf-usage-collector-fr-invalidation-reason-code`).

**Effect.** Within any aggregation, the invalidated **Usage Record** and the invalidation entry itself **MUST** each contribute nothing to the selected set (`cpt-cf-usage-collector-fr-query-aggregation`); because both carry the same covered period, no requested range selects one without the other, so no placement of the invalidation can change a result. The withdrawal reaches no further than the fold: on the ledger read paths — raw query, point lookup, and the usage feed — both entries **MUST** be returned as persisted. Neither is deleted, neither is rewritten, each keeps its own acceptance position on the feed, and the invalidation names the record it withdraws (`cpt-cf-usage-collector-fr-billing-fields-on-read`). A consumer that folds **Usage Records** it has read directly leaves a withdrawn pair out itself, recognising it from that reference (`cpt-cf-usage-collector-fr-aggregation-fold`).

**Ledger against derived state.** An invalidation is appended and modifies no persisted **Usage Record**, so the ledger remains append-only in the strict sense. This does **not** mean an invalidation's effect can be applied to an already-computed aggregate by appending a further contribution to it: `MAX`, `MIN`, and `LATEST` cannot be reversed by any additional term, and `SUM` and `COUNT` can be adjusted only by a store able to resolve the withdrawn record's own contribution. Any derived or materialised aggregate is therefore obliged to **recompute** over the affected range. The append-only guarantee is a property of the ledger, not of the derived read paths.

**Correcting a value.** Invalidation is not amendment: correcting a mis-measured quantity is an invalidation followed by a fresh emission under a new idempotency key, which the system accepts as a new and distinct **Usage Record**. That fresh emission **MUST** carry the same attribution and the same covered period as the withdrawn record. A submission changing either is a different measurement rather than a correction of that one.

**A quantity correction is not atomic.** The withdrawal and the replacement are two separate submissions, each accepted in its own right and each at its own acceptance position. A consumer reading the feed can therefore observe the withdrawal before the replacement arrives. A consumer **MUST NOT** treat that intermediate state as a settled net-zero period, because the replacement is still in flight.

- **Rationale**: Withdrawal is the only correction whose meaning does not depend on what a quantity means, which is what lets a single rule cover every meter: a signed adjustment would have to be interpreted differently for an accrued amount than for an observation, and no such interpretation is available without reintroducing a per-meter classification on the ingestion path. Expressing the withdrawal as an appended entry rather than an in-place change keeps the ledger append-only and keeps both the original measurement and its withdrawal auditable, which an update in place would destroy; it is also what keeps the usage feed's snapshot guarantee intact, since a status flip on a delivered row would be a mutation a paginated scan could observe. Routing invalidation through the ordinary ingestion path reuses the existing PDP attribution, idempotency, and quota machinery and keeps the public contract surface stable. Reusing that path is also what makes the entry a faithful copy rather than a sparse marker. A field left unspecified on the correction is a field every surface has to special-case: the dedup identity and the derived identifier are computed over the covered period, so an entry without one has no derivable identity; an entry carrying a period of its own would fall outside the range an auditor scans to re-read a closed month, returning the withdrawn **Usage Record** without the entry that withdraws it (`cpt-cf-usage-collector-fr-query-raw`); and an entry without the target's metadata would drop out of exactly the grouped and filtered reads that surfaced the target (`cpt-cf-usage-collector-fr-record-metadata`), so a consumer narrowing by a declared property would see the measurement and never its withdrawal. Copying the quantity likewise lets a consumer that reads the withdrawal alone know what is being withdrawn, without the second round-trip `cpt-cf-usage-collector-fr-billing-fields-on-read` exists to avoid. One rule — copy the target, mark it, key it, reference it, give a reason — also leaves nothing to restate per field as the record shape grows. The cost is that the aggregation exclusion becomes load-bearing for correctness rather than merely tidy: a fold that wrongly admitted a quantity-less invalidation once contributed nothing, whereas one that admits an echoed quantity double-counts the measurement, which is why the fold rule is stated over both entries of the pair rather than the withdrawn one alone, and why any materialised aggregate recomputes. The retroactive reach is bounded because an unbounded one obliges the store to retain per-record invalidation state for longer than it retains the records themselves, and because it cannot usefully exceed the retention that covers the target anyway. That bound belongs to the path rather than to the entry kind, which keeps one rule where there would otherwise be two and repairs two things a kind-dependent bound broke: a far-past withdrawal arrived on the live path marked as live, defeating the origin marker, and a bulk withdrawal — the emitter-defect case — competed with live ingestion instead of running under the backfill route's isolation.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-platform-operator`
- **Depends on**: `cpt-cf-usage-collector-fr-ingestion`, `cpt-cf-usage-collector-fr-idempotency`, `cpt-cf-usage-collector-fr-record-identity`, `cpt-cf-usage-collector-fr-ingestion-authorization`, `cpt-cf-usage-collector-fr-backfill`

### 5.7 Usage Record Typing

Every **Usage Record** is metered against a GTS type that defines what is being measured. Such a type is platform-global: it exists once for the whole deployment and is referenced by any tenant's **Usage Records**. These types are not scoped to or owned by tenants.

The Usage Collector does not mint them. It resolves declarations and validates **Usage Records** against them; the declarations themselves are held by the platform `types-registry` gear (`cpt-cf-usage-collector-actor-types-registry`), where the gears and vendors that need a new meter register one. Declaring a meter is therefore an extension point of the platform type system rather than an operation on this gear.

The fields a **Usage Record** carries are the same for every GTS type, and a declaration cannot restate or relax them: what varies per GTS type is the metadata surface and the type-level attributes listed below. Requirements in [§5.9](#59-billing-integration) that constrain the shape of a **Usage Record** therefore hold for every GTS type without being restated per type.

#### Usage GTS Type Declaration

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-usage-type-declaration`

A **Usage Record**'s GTS type is registered in `types-registry`, which owns its whole lifecycle. The Usage Collector **MUST NOT** expose a GTS type operation of any kind — read or write — on any of its surfaces, and **MUST NOT** maintain a second catalog that has to be kept in step with the registry.

A GTS type declaration **MUST** carry:

- **Aggregation fold** — exactly one, drawn from the closed set in `cpt-cf-usage-collector-fr-aggregation-fold`, as a type-level property of the declaration. The Usage Collector **MUST NOT** infer it from the shape of the type identifier.
- **Canonical metering unit** — one unit per GTS type, per `cpt-cf-usage-collector-fr-metering-unit-binding` and `cpt-cf-usage-collector-fr-canonical-units`.
- **Metadata surface** — the closed set of metadata properties **Usage Records** of this GTS type may carry, per [§5.1](#51-usage-ingestion) `cpt-cf-usage-collector-fr-record-metadata`.
- **Retention policy** — per `cpt-cf-usage-collector-fr-billing-retention-floor`. It also bounds how long the dedup identity of an accepted **Usage Record** remains visible (`cpt-cf-usage-collector-nfr-query-freshness`).

A declaration can additionally carry a **nominal sampling interval**, on any fold. It is informational: the gear never reads it, and it serves two consumer-side purposes — letting a consumer reading a series of observations detect gaps and choose an integration step (`cpt-cf-usage-collector-fr-quantity-semantics`), and supplying the expected cadence a consumer compares an ingestion watermark against to detect an emitter that has silently stopped (`cpt-cf-usage-collector-fr-reconciliation-metadata`).

A GTS type **MUST** be uniquely identified across the deployment. Declaring one **MUST NOT** require Usage Collector code changes or redeployment: a newly registered GTS type becomes available for ingestion across all tenants with no Usage Collector-side action.

A declaration **MUST** be immutable in the attributes that give persisted **Usage Records** their meaning — the aggregation fold, the canonical metering unit, and the metadata surface. A meter that must change any of them is a new GTS type, not an edit in place. A **Usage Record** carries only a reference to its GTS type, so redefining a declaration would silently restate the meaning of every **Usage Record** already accepted under it.

Which calling-gear identities may emit **Usage Records** referencing a given GTS type, and for which tenants, is declared in PDP policy and stored by neither this gear nor the registry (`cpt-cf-usage-collector-fr-ingestion-authorization`).

Primary use cases: AI/LLM token metering (input/output tokens, custom credit units), compute metering (vCPU-hours, GPU-hours), API request metering (calls by tenant and endpoint), storage metering (GB-hours across tiers), and network transfer (bytes ingress/egress).

- **Rationale**: New resource types must be meterable without redeployment, and the platform already runs a component whose job is holding type declarations; a second catalog inside the Usage Collector would be one more place the same fact can be read and disagree. A single registry also removes the divergence between this gear and Quota Enforcement, which already resolves the same meters as registered platform types. Declaring the fold as a property rather than inferring it from the identifier lets a declaration be read without knowing which naming convention was in force when it was registered, and keeps the classification legible to the other gears that now read it. Immutability is stated because the alternative is worse than it looks: a persisted `value` carries neither unit nor fold of its own, so reinterpreting a declaration reprices history silently and undetectably.
- **Actors**: `cpt-cf-usage-collector-actor-platform-operator`, `cpt-cf-usage-collector-actor-types-registry`

#### GTS Type Resolution and Record Validation

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-usage-type-resolution`

On every ingestion the system **MUST** resolve the referenced GTS type to its registered declaration and validate the **Usage Record** against it before the **Usage Record** is accepted for delivery. Validation covers the declared metadata surface and the bound metering unit. It does **not** depend on the declared aggregation fold: no ingestion invariant is fold-dependent, and in particular the sign of a quantity is never constrained (cross-reference [§5.2](#52-aggregation-fold) `cpt-cf-usage-collector-fr-aggregation-fold`, `cpt-cf-usage-collector-fr-record-quantity`). Rejections **MUST** be returned to the caller immediately with an actionable error before any **Usage Record** is accepted for delivery.

The system **MUST** fail closed. Where the referenced GTS type cannot be resolved — an identifier this deployment has never resolved, or `types-registry` unreachable with no usable resolved declaration — the **Usage Record** **MUST** be rejected with an actionable error naming the unresolved identifier, and **MUST NOT** be persisted. The system **MUST NOT** admit a **Usage Record** it could not validate, and **MUST NOT** relax validation in order to protect ingestion availability.

Resolution sits on the ingestion path and is bound by `cpt-cf-usage-collector-nfr-ingestion-latency` and `cpt-cf-usage-collector-nfr-throughput`. In the steady state the system **MUST** serve resolution from a local cache of resolved declarations rather than a per-**Usage Record** call to `types-registry`; cache maintenance, invalidation, and recovery are in DESIGN. A `types-registry` outage or restart **MUST NOT** stop ingestion for a GTS type already in use: the system **MUST** keep serving a declaration it has already resolved, and **MUST** recover one the registry has lost, so that a registry failure degrades the introduction of new GTS types rather than the ingestion of existing ones. Recovery is best-effort and is not guaranteed for every declaration; where a reference does not resolve, the **Usage Record** is rejected fail-closed rather than admitted unvalidated. The system **MUST NOT** reject an otherwise valid **Usage Record** in order to make recovery possible.

The same resolution governs the read paths: an aggregation query naming a GTS type that does not resolve **MUST** be rejected rather than dispatched to the storage plugin (cross-reference `cpt-cf-usage-collector-fr-query-aggregation`).

Where a declaration is removed from `types-registry`, **Usage Records** already accepted under it remain persisted and unmodified, but the gear can no longer interpret them: the attributes that give them their meaning exist only in the declaration, and the gear **MUST NOT** substitute a default for any of them. Every operation that depends on resolving the declaration is therefore rejected for those **Usage Records** for as long as the identifier does not resolve. Whether a declaration is ever removed is a `types-registry` decision this gear does not constrain.

- **Depends on**: `cpt-cf-usage-collector-fr-usage-type-declaration`, `cpt-cf-usage-collector-fr-ingestion-authorization`
- **Rationale**: Validating against the declaration is what keeps unrateable and unattributable **Usage Records** out of the store, where the error is far cheaper to diagnose than at charge time. Fail-closed is stated explicitly because the tempting failure mode is the opposite one: a registry outage on the ingestion path invites accepting **Usage Records** unvalidated to protect throughput, which converts an availability incident into silent data corruption that surfaces on an invoice weeks later. Caching is raised to requirement level rather than left to DESIGN because the resolution dependency is what makes the ingestion NFRs contingent on a second gear's availability, and `types-registry` publishes no latency obligation of its own; serving the steady state from cache is what keeps this gear's ingestion obligations self-contained.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-types-registry`

### 5.8 Data Classification

#### Data Classification

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-data-classification`

The system **MUST** treat its persisted data as one of three classes:

- **Opaque platform identifiers** (tenant ID, subject ID, resource ID, GTS type reference) — internal platform references issued upstream. The Usage Collector **MUST NOT** interpret, decode, or correlate these identifiers to natural persons; PII management belongs to the platform identity layer.
- **Operational telemetry** (**Usage Record** value, window bounds, acceptance instant, idempotency key, correction references) — non-personal metering data.
- **Caller-supplied metadata** (the optional per-**Usage Record** metadata object) — opaque to the Usage Collector. Calling gears **MUST NOT** place PII, payment data, regulated health data, or credentials into metadata; this is a product-level contract on usage sources, reiterated to integrators in the API documentation.

- **Rationale**: Explicit classification bounds the data the gear holds and keeps Privacy by Design, regulatory, and residency obligations delegated to the platform layer and the operator-selected plugin.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-platform-developer`, `cpt-cf-usage-collector-actor-platform-operator`

### 5.9 Billing Integration

This section states the metering contract a charging pipeline requires of the Usage Collector: how a **Usage Record** attributes consumption to a period, how it is identified, what unit its quantity carries, how corrections are attributed, and how a downstream consumer reads the stream without gaps or duplicates. **Usage Records** remain **append-only**; corrections are expressed through `cpt-cf-usage-collector-fr-record-invalidation`. Commercial identity — subscription, SKU, and the payer/seller axes — is **not** carried by the gear: an emitter cannot know it, and it is resolved by downstream consumers from the **Usage Record**'s tenant and resource attribution.

Requirements below apply to every GTS type. Where a requirement constrains what a GTS type **declaration** carries, it is written against the declaration rather than against the component that stores it; [§5.7](#57-usage-record-typing) places that component in `types-registry`, and these requirements hold unchanged wherever the catalog lives.

#### Interval Usage Windows

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-usage-windows`

Every **Usage Record** **MUST** carry exactly one emitter-supplied time attribution: the half-open period it covers — start inclusive, end exclusive, expressed as UTC instants, with the start no later than the end. The **Usage Record** **MUST NOT** carry any other emitter-supplied time attribution. The gear **MUST** reject timestamps that carry no offset information and **MUST** normalize non-UTC offsets to UTC.

**Selection.** Every read path **MUST** select a **Usage Record** when the end of its covered period falls in the requested range — `from <= window_end < to`. No other comparison of period against range is admissible on any path.

**Point events.** A **Usage Record** whose period start and end coincide is a **point event** — a single instant rather than a period, the shape used by emitters that observe discrete events. It needs no selection rule of its own, because its period end is its instant.

Because the period end is exclusive, a **Usage Record** whose period ends exactly on the range's upper bound belongs to the next range. This is the expected shape, not a defect to work around: an emitter tiles its periods edge to edge, and a consumer that wants a given calendar period shifts the requested range forward by one emission period.

**Acceptance instant.** The system **MUST** assign every **Usage Record**, and every invalidation entry, a gear-assigned UTC instant recording when the gear accepted it, and **MUST** expose that instant on read. The acceptance instant **MUST NOT** be settable or overridable by the emitter. Late arrival is evaluated as the acceptance instant against the end of the covered period.

- **Depends on**: `cpt-cf-usage-collector-fr-ingestion`, `cpt-cf-usage-collector-fr-idempotency`, `cpt-cf-usage-collector-fr-query-raw`, `cpt-cf-usage-collector-fr-query-aggregation`
- **Rationale**: Charging evaluates consumption over a period and detects late arrival against a period boundary; high-rate infrastructure emitters such as object storage can only integrate by pre-aggregating into periods, and a single instant makes "N operations during this period" inexpressible. Carrying a period *and* a separate instant of occurrence would leave two overlapping notions of when a **Usage Record** happened with no stated relationship between them, so the period subsumes the instant: a point event is a zero-length period. Requiring exactly one time attribution — rather than naming a field to drop — also forecloses re-introducing a parallel one later. Selecting on the period end makes every range a partition of the **Usage Records**: each one falls in exactly one range, so adjacent ranges add up and a storage plugin can serve a range from a rollup. Late arrival is deliberately measured against a **gear-assigned** acceptance instant: an emitter-supplied one would let a skewed or misbehaving emitter hide its own lateness.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-usage-consumer`

#### Record Identity

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-record-identity`

Every accepted **Usage Record**, and every accepted invalidation entry, **MUST** carry a system-assigned identifier that is:

- **server-derived** — never client-supplied;
- **stable** — an exact-equality retry of the same submission yields the same identifier;
- **unique per tenant, GTS type, idempotency key, and covered period** — **Usage Records** covering different periods are therefore distinct even under a single stable per-meter idempotency key;
- **reproducible offline** by the emitter from those same attributes alone, with no round-trip, so a correction reference can be computed before submission;
- **addressable** — point lookup and the reference an invalidation entry carries both resolve against it.

Downstream consumers **MUST** use this identifier as their deduplication and reference key, and the ledger read paths **MUST** carry it (`cpt-cf-usage-collector-fr-billing-fields-on-read`).

An invalidation entry is identified by the same derivation over the same four attributes, which is well defined because [§5.6](#56-corrections) fixes the period it carries to that of the **Usage Record** it withdraws. Its identifier differs from its target's because the two carry different idempotency keys; an invalidation submitted under its target's own key collides on all four attributes and is therefore rejected as a same-key content mismatch (`cpt-cf-usage-collector-fr-idempotency`) rather than accepted as a second entry. Entry type is deliberately **not** part of the derivation: admitting it would let one key stand for both a measurement and its withdrawal, so an emitter defect that reused a key would silently produce both instead of surfacing a conflict.

**Derivation precondition.** The system **MUST** reject a submission whose idempotency key or GTS type reference contains an ASCII control character, with an actionable validation error. The derivation joins these values with a control-character separator, so admitting one would break the uniqueness the identifier is required to have.

**Precision precondition.** The system **MUST** reject a submission whose covered period carries a precision finer than one microsecond, with an actionable validation error. The derivation reads a fixed-width microsecond form of each bound, so a finer value has no canonical spelling. The system **MUST NOT** truncate such a value instead: a truncated bound would be persisted, read back, and then re-derive an identifier other than the one the entry carries, which defeats offline reproducibility.

The derivation algorithm and its namespace constant are recorded in the identity ADR, not in this document.

- **Depends on**: `cpt-cf-usage-collector-fr-idempotency`
- **Rationale**: A charging consumer must guarantee at most one rated charge per **Usage Record**, which needs a stable reference key it can dedup on; a correction must name exactly one entry, which the attribution alone cannot do, since several accepted **Usage Records** can share tenant, type, resource, and subject and differ only in period and quantity. Uniqueness is stated over the covered period so that an emitter does not have to encode the period into the key itself. Offline reproducibility is what allows a correction to reference its target without first reading it back.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-usage-consumer`

#### Live-Path Time Bounds

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-fr-live-future-time-bound`

On the live ingestion path the system **MUST** bound the covered period on both sides. Both bounds are configurable, and both are enforced before persistence.

- **Future bound.** The system **MUST** reject a **Usage Record** whose covered period ends further into the future than the configured future tolerance (default 5 minutes). The error **MUST** identify the offending instant and the bound.
- **Past bound.** The system **MUST** reject a **Usage Record** whose covered period **ends** further into the past than the configured past tolerance (default 48 hours). The error **MUST** identify the offending instant, the bound, and the dedicated backfill route as the path such a submission belongs on (`cpt-cf-usage-collector-fr-backfill`).

Both bounds belong to the live path and govern **every** entry it admits, an invalidation entry included, over the period it copies (`cpt-cf-usage-collector-fr-record-invalidation`). The dedicated backfill path admits older periods within its own configured window, for **Usage Records** and invalidation entries alike (`cpt-cf-usage-collector-fr-backfill`).

Both bounds read the end of the covered period, so the live path admits a covered period of any length whose end falls inside the past tolerance. A meter that accrues over a month emits there as soon as its period closes, and its entry carries the live origin marker. No bound reads the period start, because no read path selects on it (`cpt-cf-usage-collector-fr-usage-windows`).

The requirement identifier names the future bound alone because it predates the past one. It is retained unchanged, because the corpus references it.

- **Depends on**: `cpt-cf-usage-collector-fr-ingestion`, `cpt-cf-usage-collector-fr-backfill`
- **Rationale**: Emitter clock skew must not open a not-yet-existing consumption period, and a bounded future tolerance turns silent time-corruption into a visible emitter defect. The past bound exists so that the origin marker can separate imported history from current consumption. A submission whose consumption ended long ago arrives marked as live, which defeats that marker, so the bound reads the period end. A period start reaching further back does not make the consumption stale. A default of 48 hours covers emitter outage and retry lag, which is what genuinely late live data is. Anything older is history, and history belongs on the route that marks it and isolates its load. Naming that route in the error lets an emitter act on the rejection rather than guess. That gap closes only if the bound governs every entry. A withdrawal of a closed period is a correction of history, so exempting it would re-open the marker gap and put the emitter-defect workload back on the live path.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`

#### Metering Unit Binding

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-fr-metering-unit-binding`

Every GTS type declaration **MUST** bind a metering unit drawn from the canonical unit list (`cpt-cf-usage-collector-fr-canonical-units`). Ingestion **MUST** reject a **Usage Record** whose GTS type has no bound unit. The unit is a property of the GTS type, resolved through the **Usage Record**'s GTS type reference; it is **not** carried per **Usage Record**.

A bound unit **MUST** be immutable for the lifetime of the GTS type, as a case of the declaration immutability rule in `cpt-cf-usage-collector-fr-usage-type-declaration`. Rebinding a unit would silently redenominate every **Usage Record** already accepted under the old unit, since a persisted quantity carries no unit of its own; a meter that must change unit is a new GTS type at a new GTS major version.

- **Depends on**: `cpt-cf-usage-collector-fr-usage-type-declaration`, `cpt-cf-usage-collector-fr-usage-type-resolution`, `cpt-cf-usage-collector-fr-canonical-units`
- **Rationale**: Failing fast at the gear keeps unrateable **Usage Records** out of the pipeline, where the error is far cheaper to diagnose than at charge time. Immutability is stated explicitly because the alternative is worse than it looks: without it, correctly interpreting a historical **Usage Record** requires knowing which unit was in force when it was accepted, which turns every read into a point-in-time catalog lookup and every unit change into a silent repricing of history.
- **Actors**: `cpt-cf-usage-collector-actor-platform-operator`

#### Canonical Units

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-fr-canonical-units`

The system **MUST** publish a normative canonical unit list (at minimum `bytes`, `byte-hours`, `count`, `seconds`) and **MUST NOT** convert, scale, or round stored or emitted quantities. Quantities persist and travel in the declared canonical unit; presentation conversions such as GiB against GB, or hours against seconds, are the consumer's responsibility.

- **Rationale**: Unit conversion inside a metering substrate is a classic source of silent billing discrepancy, decimal against binary prefixes being the usual culprit. One canonical unit per meter, converted only at the edge, keeps every stored quantity comparable and auditable. Object storage maps directly: `byte-hours` for stored volume over time, `bytes` for traffic, `count` for requests.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-usage-consumer`

#### Usage Record Quantity

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-record-quantity`

Every **Usage Record** **MUST** carry exactly one quantity, expressed in the canonical metering unit bound to its GTS type (`cpt-cf-usage-collector-fr-metering-unit-binding`). An invalidation entry carries the quantity of the **Usage Record** it withdraws, unchanged and never negated, and is subject to every rule below in its own right (`cpt-cf-usage-collector-fr-record-invalidation`).

A quantity **MUST** be a finite, signed decimal number. The system **MUST** reject, with an actionable error, any submission whose quantity is absent, non-numeric, non-finite, or outside the range and decimal precision the ingestion contract publishes. No entry type is exempt: an invalidation carries a quantity because it copies one.

The gear **MUST** publish that range and precision as part of its public contract, and every storage plugin **MUST** round-trip the full published range — including its negative half — and the full published precision without loss. The system **MUST NOT** convert, scale, round, truncate, or otherwise alter a quantity between acceptance and read (`cpt-cf-usage-collector-fr-canonical-units`): a quantity read back **MUST** equal the quantity submitted, digit for digit, on every read path.

- **Depends on**: `cpt-cf-usage-collector-fr-ingestion`, `cpt-cf-usage-collector-fr-metering-unit-binding`, `cpt-cf-usage-collector-fr-canonical-units`
- **Rationale**: A metering substrate that does not pin what a quantity is numerically leaves each storage plugin to choose, and the choices diverge in exactly the ways that corrupt a bill quietly — a backend holding quantities as a binary float rounds a decimal `byte-hours` figure, one with a narrower range saturates a large aggregate, and a single non-finite value propagates through a `SUM` and poisons every total that touches it. Bounding the domain at ingestion keeps those failures at the emitter, where they are cheap to diagnose, instead of at charge time. The obligation is expressed as a published range every plugin must round-trip rather than as a fixed numeric type, so the wire contract can name concrete bounds without this document dictating how a backend represents them. The negative half is called out because it is the half a plugin author is most likely to assume away: nothing in this document constrains the sign of a measurement, and a store that cannot hold one is unfit rather than merely limited.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-storage-backend`

#### Quantity Semantics Under Windows

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-quantity-semantics`

The gear **MUST** publish, as a normative part of this contract, how a **Usage Record**'s quantity relates to the period it covers. That relation is carried by the GTS type's declared aggregation fold, and the normative statement of it is the fold table in [§5.2](#52-aggregation-fold) `cpt-cf-usage-collector-fr-aggregation-fold`, which is the single enumeration of the folds and their additivity. Two consequences of that table bear restating here, because a charging consumer depends on both:

- Under a `SUM` fold the quantity is the amount **accrued over** the period, quantities for disjoint periods of the same series are additive, and a consumer obtains period consumption by summing. Pre-integrated resource-time meters — stored volume expressed in `byte-hours` accumulated across the period — are `SUM` meters.
- Under every other fold the quantity is a **single observation**, not an amount accrued over the period. Such quantities **MUST NOT** be summed across periods by any consumer, and no fold the gear offers converts a series of them into a period quantity. A meter whose consumption must be charged is therefore declared `SUM`, with any integration performed at the emitter before submission.

Which applies to a given **Usage Record** is read from the referenced GTS type's declaration (`cpt-cf-usage-collector-fr-usage-type-declaration`); it is not carried per **Usage Record** and is not inferred from the identifier's shape.

The gear **MUST NOT** integrate, differentiate, interpolate, re-window, or synthesize missing samples in either direction: it carries the emitted quantity and the declared fold, and integration is a consumer concern (`cpt-cf-usage-collector-contract-downstream-usage-reader`). Where a declaration carries a nominal sampling interval, the gear **MUST NOT** act on it.

- **Depends on**: `cpt-cf-usage-collector-fr-aggregation-fold`, `cpt-cf-usage-collector-fr-usage-windows`
- **Rationale**: A period alone does not tell a consumer whether to sum or not, and the two readings of one series differ by orders of magnitude — summing a byte-level observation series yields a bill that is wrong rather than merely imprecise. The declared fold already encodes the distinction and is resolvable from the GTS type every **Usage Record** references, so no new **Usage Record** attribute is introduced; what this requirement adds is the statement of what the fold means once a **Usage Record** covers a period, and the prohibition on the gear closing the gap itself. Stored volume is the case that hits this first: it is naturally a level, and the only shape the gear will charge on is a pre-integrated `byte-hours` `SUM` meter. The sampling interval is deliberately informational — the gear is forbidden from integrating, so a value it may not act on has no business being mandatory.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-usage-consumer`, `cpt-cf-usage-collector-actor-platform-operator`

#### Invalidation Reason Code

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-fr-invalidation-reason-code`

Every invalidation entry **MUST** carry a non-empty reason code, and it **MUST** be returned on every read path that exposes the correction. All other invalidation invariants are unchanged: the explicit entry type, the reference contract, the faithful copy of the target's caller-supplied fields, no invalidation of an invalidation, and at most one per **Usage Record**. The copied period is bounded by the path the entry arrived on (`cpt-cf-usage-collector-fr-record-invalidation`).

An ordinary **Usage Record** carries no reason code, including one whose quantity is negative: a negative quantity records real consumption rather than a correction ([§5.6](#56-corrections)), and requiring a justification for it would misdescribe it.

- **Depends on**: `cpt-cf-usage-collector-fr-record-invalidation`
- **Rationale**: A consumer and an auditor must be able to tell a duplicate withdrawal from a mis-attribution fix from a metering-bug fix. Without a reason code every invalidation looks alike, and the distinction is only recoverable by correlating with whatever out-of-band ticket prompted it. Confining the requirement to invalidations is what keeps it meaningful: once a real decrease is an ordinary measurement, the entries that carry a reason code are exactly the entries asserting that a prior fact was untrue.
- **Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-platform-operator`

#### Billing Fields on Read Paths

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-billing-fields-on-read`

Raw query, point lookup by identifier, and the usage feed **MUST** return the following unstripped: the **Usage Record**'s identifier, the GTS type it references, the period it covers, its acceptance instant, its declared metadata values, its signed quantity, its entry type, the **Usage Record** it withdraws and its reason code where it is an invalidation, and the origin marker that distinguishes a backfilled **Usage Record** (`cpt-cf-usage-collector-fr-backfill`). The metering unit and the aggregation fold are resolved from the GTS type declaration and are not carried per **Usage Record**. Point lookup by identifier **MUST** return the exact persisted fact.

No read path carries a reverse link. Its value would depend on entries accepted later, so on the usage feed it would change an entry already delivered and break the replay guarantee (`cpt-cf-usage-collector-fr-billing-usage-feed`). A reader holding a **Usage Record** finds any withdrawal of it with one raw query, filtering on the target reference over the period that record covers.

The aggregate path (`cpt-cf-usage-collector-fr-query-aggregation`) is excluded: an aggregate is a fold over many **Usage Records** and has no single identifier, covered period, quantity, or correction linkage to return.

- **Depends on**: `cpt-cf-usage-collector-fr-query-raw`, `cpt-cf-usage-collector-fr-record-identity`
- **Rationale**: Replay, backfill charging, and finance audit all need the persisted fact with its identity and lifecycle intact; stripping any of it forces a consumer back into store internals or a second round-trip. Type-level attributes are deliberately excluded: every **Usage Record** names its GTS type and resolves them through the cached declarations of `cpt-cf-usage-collector-fr-usage-type-resolution`, so denormalizing them onto every entry of a high-rate stream would only create a second place the same fact can be read and disagree. Declaration immutability (`cpt-cf-usage-collector-fr-usage-type-declaration`) is what makes that resolution safe to perform at read time rather than pinned at acceptance time.
- **Actors**: `cpt-cf-usage-collector-actor-usage-consumer`

#### Usage Feed for Downstream Consumers

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-fr-billing-usage-feed`

The system **MUST** provide consumers a deterministic, replay-safe read path over accepted **Usage Records**, **pull-based** over the Downstream Usage Reader Contract (§7.2) as the primary mechanism. It **MUST** provide:

- a documented, monotonic acceptance-sequence ordering scoped per (tenant, GTS type) — the same scope as the consistency floor in `cpt-cf-usage-collector-nfr-query-freshness`; no cross-tenant or cross-GTS-type total order is claimed;
- stable cursor pagination over a consistent snapshot: a paginated scan **MUST NOT** observe feed entries appearing, disappearing, or changing mid-scan, except append-only arrivals demarcated by an explicit watermark returned with the page. The cursor is **opaque** and covers every (tenant, GTS type) scope of the subscription; the per-scope ordering guarantee holds within each scope, the interleaving of scopes within a page is implementation-defined but deterministic — the same cursor **MUST** yield the same continuation, extended only by entries accepted since, and a replay that resends a previously recorded watermark **MUST** be identical entry for entry — and the watermark returned with a page **MUST** be opaque and **MUST** carry the completed acceptance position for every scope of the subscription, which a single acceptance-sequence value cannot do, since the sequence is monotonic only within a scope;
- **subscription to a subset of GTS types**, so a consumer reads only the meters it rates. A consumer **MUST** be able to declare the set of GTS types it consumes, and the feed **MUST** exclude everything else, including from the watermark and cursor it returns for that subscription;
- correction linkage per `cpt-cf-usage-collector-fr-billing-fields-on-read`, so a reader can reconstruct correction history;
- **corrections as ordinary entries**. Every correction is an appended invalidation entry, so it enters the sequence as itself at its own acceptance position, and no feed entry represents a change to an already-delivered **Usage Record**. The snapshot rule above therefore holds over **Usage Records** directly: an already-read entry never changes, and a correction to it arrives as a later entry. An accepted invalidation **MUST NOT** remove either entry from the feed — withdrawal is expressed by the arrival of the later entry, never by the disappearance of the earlier one — so a replay from a cursor within the retention floor observes the same entries the original scan observed. A negative quantity likewise arrives as an ordinary entry, since it is a measurement rather than a correction ([§5.6](#56-corrections)).

- **Depends on**: `cpt-cf-usage-collector-fr-query-raw`, `cpt-cf-usage-collector-contract-downstream-usage-reader`
- **Rationale**: A charging consumer's inbound path must be replay-safe under concurrent ingest — a consumer outage beyond its buffer, a region loss, a bounded re-rating — and without snapshot-consistent cursors a scan is silently incomplete or silently duplicated. Pull over the existing reader contract reuses a surface that is already built, and with dedup by **Usage Record** identifier an overlapping replay is harmless. Subscription is what keeps the read side proportionate: a consumer that rates a handful of meters should not have to drain, and be sized against, the platform's entire telemetry volume, and it is also what makes `cpt-cf-usage-collector-nfr-replay-throughput` a bounded obligation rather than one that grows with unrelated traffic.
- **Actors**: `cpt-cf-usage-collector-actor-usage-consumer`

#### Retention Floor

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-fr-billing-retention-floor`

A GTS type declaration **MUST** be able to carry a retention policy, and the operator-selected storage plugin deployment profile **MUST** honor it. For **every** GTS type, that policy **MUST** be at least:

> `floor = configured backfill window + operational replay horizon`

The **operational replay horizon** is the maximum age of usage a charging consumer can be required to re-read from the feed and re-process. It is the **guaranteed** replay window rather than the refusal boundary. It is a deployment parameter with a launch default of **35 days**: one monthly billing cycle plus close-out slack, so that a pricing or rating defect found shortly after a cycle closes can still be re-rated from source rather than reconstructed downstream. It is distinct from `cpt-cf-usage-collector-nfr-replay-throughput`, which bounds how *fast* a consumer that has fallen behind returns to the live watermark, not how far back it may start.

**Three zones govern a rewound cursor** (`cpt-cf-usage-collector-fr-billing-usage-feed`):

- A cursor no older than the operational replay horizon **MUST** be served. That is the guarantee a charging consumer codes against.
- A cursor older than what the store retains **MUST** be refused with an actionable error carrying the oldest position still servable.
- Between those two ages service is plugin-dependent. A consumer **MUST NOT** rely on it.

With the launch defaults — a 90-day backfill window (`cpt-cf-usage-collector-fr-backfill`) and a 35-day replay horizon — the floor is **125 days**.

Retention runs from the **end of the covered period** of a **Usage Record**, not from the instant it was accepted: a record is retained for the policy duration measured against the consumption it reports. That is why the two terms are summed rather than alternatives, and the sum yields the invariant the floor exists to deliver:

> Every accepted **Usage Record**, whenever it was imported, retains at least one full replay horizon from the moment it first becomes readable.

The floor is stated as a formula rather than a constant, so widening either term raises it: a deployment that widens the backfill window **MUST** revalidate retention against the new floor before the wider window takes effect. The same clock bounds how long a **Usage Record**'s dedup identity stays visible to further submissions (`cpt-cf-usage-collector-fr-idempotency`), and the floor's replay-horizon term is what holds the backfill window *strictly* inside retention rather than merely within it. The floor admits no exemption: it binds every GTS type, whatever consumes it.

This is an **operator and plugin deployment obligation**, not gear-level enforcement: retention, archival, and purging remain delegated to the active storage plugin's deployment profile and the platform governance layer. A deployment below the floor for any GTS type is a readiness failure surfaced at operator onboarding and storage-plugin readiness review.

The Usage Collector is **not** the system of record for long-term charging evidence. Retention of rated charges, billable items, and invoice detail across dispute, audit, and statutory periods is a downstream obligation. Accordingly this requirement sets no multi-year floor and mandates no aggregate: the floor covers the **operational** recovery horizon only.

- **Depends on**: `cpt-cf-usage-collector-fr-pluggable-storage`, `cpt-cf-usage-collector-fr-backfill`
- **Rationale**: The feed's disaster path is only real if the data outlives the outage it recovers from, and the backfill path is only real if imported history outlives its own import; deriving the floor from both horizons removes the inconsistency of accepting 90-day-old history into a 35-day store. The terms are summed rather than maximised because retention runs from the covered period, and the floor carries no exemption because no declared attribute records whether a charging consumer reads a meter. Keeping the floor operational, rather than extending it to dispute and audit periods, is what keeps the [§6.2](#62-nfr-exclusions) exclusion honest — the gear can only claim not to be a financial-reporting source if the record of what was charged genuinely lives downstream.
- **Actors**: `cpt-cf-usage-collector-actor-platform-operator`, `cpt-cf-usage-collector-actor-storage-backend`

#### Dedicated Backfill

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-fr-backfill`

The system **MUST** provide a dedicated bulk-import path for historical **Usage Records**, with a bounded backfill window (configurable; default 90 days) enforced as a hard bound, rejecting any submission whose covered period ends further back with an actionable error naming the bound — a period ending exactly at the bound is admitted, since the bound refuses only what lies past it — an origin marker identifying every backfilled **Usage Record**, and every invalidation entry accepted on that path, both as persisted and on every read path, workload isolation from live ingestion such that backfill load does not breach live-path SLOs, and validation identical to the live path but for the covered-period bounds it replaces.

The path **MUST** be reachable on the REST API and on the in-process SDK trait (`cpt-cf-usage-collector-interface-sdk-client`), since it is the only route admitting a covered period older than the live past tolerance and both surfaces must emit and withdraw over that period. It accepts **Usage Records** and invalidation entries alike, and marks both. A deployment **MUST NOT** admit a backfill window that leaves any GTS type's retention below `window + operational replay horizon` (`cpt-cf-usage-collector-fr-billing-retention-floor`), since a **Usage Record** imported at the far edge of a window merely *within* retention would be eligible for purge on arrival. That floor binds every meter, so widening the window raises it for all of them, and retention **MUST** be revalidated against the new floor before the wider window takes effect.

**The window is a hard bound.** The system **MUST NOT** admit a covered period older than it on any surface, and v1 exposes no override reaching past it ([§4.2](#42-out-of-scope)). Two properties rest on that: an aggregate over a period entirely older than the window cannot change ([§5.5](#55-usage-query--aggregation)), and — since every meter retains for at least the window plus one replay horizon, holding the window strictly inside retention — every submission the gear admits falls inside its own idempotency horizon (`cpt-cf-usage-collector-fr-idempotency`). An operator whose history predates the window widens the window before importing, and raises retention with it: the floor is the window plus one replay horizon, so a wider window raises the floor and retention **MUST** be revalidated against it (`cpt-cf-usage-collector-fr-billing-retention-floor`).

- **Depends on**: `cpt-cf-usage-collector-fr-ingestion`, `cpt-cf-usage-collector-fr-billing-retention-floor`
- **Rationale**: Bulk historical import on the live path competes with real-time SLOs and would surface stale **Usage Records** to consumers unmarked; an isolated, flagged path lets a consumer route backfill to batch handling instead of treating it as current consumption. The path carries both retroactive directions — import in and withdrawal out — because an invalidation is bounded by the path it travels rather than by its kind (`cpt-cf-usage-collector-fr-record-invalidation`). It is on the SDK because confining it to REST would turn every correction found after the live past tolerance into an operator escalation.
- **Actors**: `cpt-cf-usage-collector-actor-platform-operator`, `cpt-cf-usage-collector-actor-usage-source`

#### Ingestion Rate Limiting

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-fr-rate-limiting`

The system **MUST** enforce configurable ingestion quotas per calling gear and per (calling gear, tenant) pair across all ingestion paths, rejecting over-quota submissions with an actionable throttle error carrying retry guidance. Throttling **MUST NOT** silently drop **Usage Records**.

- **Depends on**: `cpt-cf-usage-collector-fr-ingestion`
- **Rationale**: A misbehaving emitter must not degrade the ingestion SLO for meters that feed charging; an explicit throttle error lets a well-behaved emitter apply backpressure and retry instead of losing data it cannot recover.
- **Actors**: `cpt-cf-usage-collector-actor-platform-operator`, `cpt-cf-usage-collector-actor-usage-source`

#### Reconciliation Metadata and Watermarks

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-fr-reconciliation-metadata`

The system **MUST** expose, via API, per-scope ingestion metadata at minimum at the (tenant, GTS type) granularity: accepted **Usage Record** counts, a quantity summary appropriate to the declared fold — the accrued sum for a `SUM` GTS type, and the observation count together with the latest observation otherwise, since quantities under any other fold are not summable (`cpt-cf-usage-collector-fr-aggregation-fold`) — the latest acceptance-instant watermark, the latest covered period end, and the latest acceptance-sequence watermark. Accepted **Usage Record** counts are reported for every GTS type irrespective of its declared fold: they count ingestion activity rather than aggregating the meter, so they are unaffected by the single-fold rule on the aggregate path (`cpt-cf-usage-collector-fr-query-aggregation`). The metadata **MUST** be sufficient for an external reconciliation job to compare gear-side accepted totals against a consumer's processed totals for a time range without a full raw scan, and to detect an emitter that has silently stopped.

**Stall detection is consumer-side.** A watermark alone identifies no stall: the same silence is routine for a daily meter and alarming for a per-minute one, so detection needs an expected cadence to compare against. That comparison is performed by the consumer or an external reconciliation job, against the GTS type's declared nominal sampling interval where one is declared (`cpt-cf-usage-collector-fr-usage-type-declaration`) and against the consumer's own expected cadence where none is. Exposing the watermarks on read is the whole of the gear's obligation. The declared interval is served by `types-registry`, which the consumer reads for itself. The gear performs no part of the comparison, raises no stalled-emitter signal, and holds no threshold of its own (`cpt-cf-usage-collector-fr-quantity-semantics`).

- **Depends on**: `cpt-cf-usage-collector-fr-ingestion`
- **Rationale**: Revenue assurance compares emitter, gear, and consumer totals; without cheap per-scope counters that comparison is a full scan, and a meter that stops emitting is invisible until an invoice is wrong. Exposing both a wall-clock and a sequence watermark lets a reconciliation job distinguish a stalled emitter from a stalled consumer. Detection stays with the consumer because the gear cannot make the judgment correctly: a watermark going quiet means either that an emitter broke or that consumption legitimately ceased — a deleted resource, a workload scaled to zero, a tenant that simply stopped — and only the consumer knows which applies to that resource. A gear-side flag would fire on every legitimate stop. Owning it would also oblige the gear to sweep an unbounded set of (tenant, GTS type) scopes on a timer and to hold a missed-interval threshold that is per-meter policy, for a signal it could not attribute.
- **Actors**: `cpt-cf-usage-collector-actor-platform-operator`, `cpt-cf-usage-collector-actor-usage-consumer`

- [ ] `p3` - **ID**: `cpt-cf-usage-collector-fr-reconciliation-caller-scopes`

The system **MUST** expose the same reconciliation metadata at the (calling gear) and (calling gear, tenant) granularities.

This is deferred beyond v1. The platform keeps the two identities on separate planes. The platform plane carries the calling-gear name, and never carries a tenant. Usage ingestion runs on the tenant plane, whose security context names the caller by opaque subject identifier only. Until one plane carries both, the gear cannot group accepted counts by calling gear. Quotas are unaffected: the gear applies them live, per call, and persists nothing (`cpt-cf-usage-collector-fr-rate-limiting`).

- **Depends on**: `cpt-cf-usage-collector-fr-reconciliation-metadata`
- **Rationale**: Revenue assurance compares what an emitter believes it sent against what the gear accepted from it. The (tenant, GTS type) scope answers this only where one gear owns the meter type.
- **Actors**: `cpt-cf-usage-collector-actor-platform-operator`, `cpt-cf-usage-collector-actor-usage-consumer`

## 6. Non-Functional Requirements

### 6.1 Gear-Specific NFRs

#### Query Latency

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-nfr-query-latency`

Aggregation queries over a 30-day range for a single tenant **MUST** complete within 500ms at p95 under the load envelope defined by `cpt-cf-usage-collector-nfr-throughput-profile` (sustained ≥ 10,000 **Usage Records**/sec ingestion, ≥ 100 concurrent aggregation queries, no active burst in progress), measured over a ≥ 30-minute steady-state window.

- **Threshold**: p95 ≤ 500ms over a ≥ 30-minute steady-state window inside the `cpt-cf-usage-collector-nfr-throughput-profile` envelope; permitted measurement tolerance ±10% (i.e., p95 ≤ 550ms accepted for any single steady-state window) provided the 30-minute trailing trend stays at or below 500ms.
- **Rationale**: Interactive dashboard and billing queries need timely responses. Anchoring on the throughput profile and a measurement tolerance removes the ambiguity in the prior wording and makes the criterion repeatable.
- **Architecture Allocation**: See DESIGN.md

#### High Availability

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-nfr-availability`

The system **MUST** maintain 99.95% monthly availability for usage ingestion endpoints.

- **Threshold**: 99.95% uptime per calendar month
- **Rationale**: Usage collection is on the critical path for all billable operations.
- **Architecture Allocation**: See DESIGN.md

#### Ingestion Throughput

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-nfr-throughput`

The system **MUST** sustain ingestion of at least 10,000 **Usage Records** per second under the steady-state load envelope defined by `cpt-cf-usage-collector-nfr-throughput-profile` (sustained ≥ 10,000 **Usage Records**/sec; concurrent aggregation queries ≤ 100; no active burst in progress; measurement window ≥ 30 minutes of steady-state operation; sample-mean and p95 reported separately).

- **Threshold**: ≥ 10,000 **Usage Records**/sec sustained sample-mean over a ≥ 30-minute steady-state measurement window; instantaneous 1-minute sample-mean tolerance ≥ 0.95 × sustained rate (i.e., ≥ 9,500 **Usage Records**/sec for any 1-minute sample inside the steady-state window).
- **Rationale**: High-volume services (LLM Gateway, API Gateway) generate significant event throughput; the ingestion path must not become a bottleneck. Anchoring on the throughput profile removes the ambiguity in "normal operation" by pinning the test condition to the sustained, burst, and concurrent-query envelope defined in `cpt-cf-usage-collector-nfr-throughput-profile`.
- **Architecture Allocation**: See DESIGN.md

#### Ingestion Latency

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-nfr-ingestion-latency`

The system **MUST** complete **Usage Record** ingestion within 200ms at p95 under the load envelope defined by `cpt-cf-usage-collector-nfr-throughput-profile` (sustained ≥ 10,000 **Usage Records**/sec, burst ≤ 30,000 **Usage Records**/sec for ≤ 5 minutes per 60-minute window, ≥ 100 concurrent aggregation queries, ≥ 700,000,000 accepted calls per 24-hour day), measured at the platform gateway over a ≥ 30-minute steady-state window.

- **Threshold**: p95 ≤ 200ms over a ≥ 30-minute steady-state measurement window inside the `cpt-cf-usage-collector-nfr-throughput-profile` envelope; permitted measurement tolerance ±10% (i.e., p95 ≤ 220ms accepted for any single steady-state window) provided the 30-minute trailing trend stays at or below 200ms.
- **Rationale**: Low ingestion latency prevents blocking in usage source services. Anchoring on the throughput profile and a measurement tolerance removes the ambiguity in "normal load" and makes the criterion repeatable.
- **Architecture Allocation**: See DESIGN.md

#### Workload Isolation

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-nfr-workload-isolation`

The system **MUST** ensure that aggregation query workloads do not degrade ingestion latency. These workloads **MUST** be isolated from the ingestion path such that concurrent execution maintains ingestion p95 latency within the `cpt-cf-usage-collector-nfr-ingestion-latency` threshold.

- **Threshold**: Ingestion p95 latency remains ≤ 200ms during concurrent query operations
- **Rationale**: Aggregation queries are analytical workloads that can compete for storage resources with the latency-sensitive ingestion path.
- **Architecture Allocation**: See DESIGN.md

#### Query Freshness

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-nfr-query-freshness`

The system **MUST** publish a plugin-agnostic consistency contract between the synchronous ingestion ack path and the subsequent raw / aggregated / catalog query surfaces. The contract is **floor-and-ceiling**: the gear floor is the minimum every active plugin honours under default deployment posture, and each plugin's deployment guide can advertise a stronger ceiling.

- **Floor (gear-level)**: an ingestion acknowledgement is durable, and the accepted **Usage Record**'s dedup identity — its tenant, GTS type, idempotency key, and covered period — remains visible to subsequent ingestion attempts for at least as long as the referenced GTS type's retention policy retains the **Usage Record** itself (`cpt-cf-usage-collector-fr-billing-retention-floor`). The dedup window is therefore per-meter and bounded rather than unbounded: once a **Usage Record** has aged out of retention, its identity may be reused, and a retry arriving after that point draws no guaranteed outcome. `cpt-cf-usage-collector-fr-idempotency` states that floor as the product contract, including its consequences for retry, replay, and backfill. A deployment **MUST NOT** be required to retain dedup identities beyond the data they protect. Visibility of that same **Usage Record** through `cpt-cf-usage-collector-fr-query-raw` and `cpt-cf-usage-collector-fr-query-aggregation` is **eventually consistent with no upper bound** relative to the ingestion acknowledgement. GTS type declarations are not covered by this floor: they are resolved from `types-registry` through `cpt-cf-usage-collector-fr-usage-type-resolution`, and their propagation delay is a property of that resolution path and its cache rather than of the storage plugin. The floor is scoped per (tenant, GTS type); no cross-tenant or cross-GTS-type ordering claim is made, and no monotonic-reads guarantee is made within a scope at the floor.
- **Ceiling (per-plugin)**: each storage plugin's deployment guide **MUST** publish that plugin's actual consistency profile (e.g., "sync, single-node", "bounded-staleness ≤ N ms", "eventual, no bound — see workload-isolation routing"). Consumers that depend on a tighter bound consciously couple themselves to that plugin's ceiling; the coupling **MUST** be recorded in the consumer's own design document.
- **Consumer rule**: read-after-write calling-gear flows (admission control, post-emit summary, immediate-readback dashboards) **MUST NOT** be designed against the query surfaces. Same-request outcome flows **MUST** consume the ingestion acknowledgement. Near-real-time observers poll within `cpt-cf-usage-collector-nfr-query-latency` and accept lag bounded by the active plugin's published ceiling.
- **Threshold**: Floor: no gear-level numeric bound (absence claim, verified by review of the design and plugin-SPI consistency-profile documentation). Ceiling: per-plugin published profile, verified against each plugin's release-readiness review.
- **Rationale**: The workload-isolation NFR routes ingestion and query to isolated backend pools (`cpt-cf-usage-collector-nfr-workload-isolation`); that isolation creates queryability lag between the ack path and the query path that nothing else names. Publishing the floor at PRD level lets consumers code defensively against the weakest plugin without reading per-plugin documentation, and lets plugin authors advertise stronger ceilings honestly rather than under an implicit gear-wide claim that overpromises for backends like ClickHouse-replicated. The architectural decision is recorded in DESIGN §5.1 (consistency-contract ADR).
- **Architecture Allocation**: See DESIGN.md.

#### Aggregate Freshness

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-nfr-aggregate-freshness`

This NFR is a **plugin readiness gate**, not a gear-level upper bound: the gear floor declared by `cpt-cf-usage-collector-nfr-query-freshness` — query-surface visibility is eventually consistent with **no upper bound** — is unchanged.

Because the aggregate path is a derived view ([§5.5](#55-usage-query--aggregation)), a plugin can serve it from a pre-computed or materialised representation, and the resulting lag is part of that plugin's consistency profile. A storage plugin deployment is fit to serve the aggregate query path to a consumer that acts on the result — quota evaluation being the case that matters, alongside operator dashboards carrying an SLO — only if the consistency ceiling it publishes under `cpt-cf-usage-collector-nfr-query-freshness` bounds acceptance → aggregate visibility at a **published, finite** value, and at **≤ 5 minutes p95** where the consumer acts on it, under the `cpt-cf-usage-collector-nfr-throughput-profile` envelope, measured over a ≥ 30-minute steady-state window.

A plugin serving the aggregate path from a materialised representation **MUST** additionally publish how an accepted invalidation reaches that representation (`cpt-cf-usage-collector-fr-record-invalidation`), since withdrawal obliges recomputation rather than a further contribution and its propagation delay need not match that of an ordinary **Usage Record**.

- **Threshold**: Gear floor: no numeric bound (absence claim, per `cpt-cf-usage-collector-nfr-query-freshness`). Readiness gate: published finite ceiling for acceptance → aggregate visibility, and p95 ≤ 5 minutes where a consumer acts on the aggregate, verified against the plugin's published consistency profile at release-readiness review, together with the published invalidation-propagation bound.
- **Rationale**: A fast answer over data of unbounded staleness is not a usable product guarantee, and `cpt-cf-usage-collector-nfr-query-latency` promises interactive latency without saying anything about how current the answer is. Naming the aggregate path as derived is what makes materialisation legitimate; publishing its lag is what stops that materialisation from silently degrading a consumer that cannot tell a stale answer from a current one. The bound is expressed as a qualifying condition on the plugin's own published ceiling rather than as a gear-wide promise, reusing the floor-and-ceiling machinery already established for the feed instead of contradicting it. Invalidation propagation is called out separately because it is the one update a materialised aggregate cannot absorb incrementally, so a plugin that quotes a single freshness number for both would be overstating one of them.
- **Architecture Allocation**: See DESIGN.md.

#### Plugin Contract Stability Across Versions

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-nfr-plugin-contract-stability`

The Plugin SPI (`cpt-cf-usage-collector-interface-plugin`), the SDK trait (`cpt-cf-usage-collector-interface-sdk-client`), and the REST API (`cpt-cf-usage-collector-interface-rest-api`) **MUST** each stay stable within a major version. This guarantee starts at the 1.0 release of each surface. A plugin built against Plugin SPI version `N` **MUST** keep working against every `N.x` release. The same guarantee covers in-process consumers of the SDK trait and remote consumers of the REST API. A breaking change **MUST** ship as a new major version that coexists with the prior major for one migration window. Plugin authors, consumer gears, and remote callers then migrate on schedules independent of the Usage Collector.

- **Threshold**: From its 1.0 release onward, a consumer compiled or wired against major version `N` of a surface **MUST** keep working unchanged across every `N.x` release. At most one prior major version stays supported per surface.
- **Rationale**: Plugin authors, downstream consumer gears, and remote usage sources are typically not the same teams as Usage Collector maintainers (e.g., a TimescaleDB or ClickHouse plugin maintained by an external storage team, or a billing system in a separate release train). Forcing them to recompile or redeploy on every minor Usage Collector release creates ecosystem coordination overhead and discourages reuse.
- **Architecture Allocation**: See DESIGN.md

#### Throughput Profile

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-nfr-throughput-profile`

The system **MUST** sustain the following ingestion and query workload profile at launch capacity:

- **Sustained ingestion**: ≥ 10,000 **Usage Records** per second (cross-reference `cpt-cf-usage-collector-nfr-throughput`).
- **Peak ingestion burst**: ≥ 30,000 **Usage Records** per second for ≤ 5 minutes in any 60-minute window without breaching `cpt-cf-usage-collector-nfr-ingestion-latency` (p95 ≤ 200ms).
- **Concurrent query consumers**: ≥ 100 active aggregation queries without breaching `cpt-cf-usage-collector-nfr-query-latency` (p95 ≤ 500ms) or degrading ingestion p95 (`cpt-cf-usage-collector-nfr-workload-isolation`).
- **Daily transaction volume**: ≥ 700,000,000 accepted ingestion calls per 24-hour day at the sustained rate.
- **Seasonal / cyclical pattern**: monthly billing-cycle close is the highest concurrent-query period; ingestion volume is not expected to spike seasonally beyond the burst envelope.

- **Threshold**: Sustained ≥ 10,000 **Usage Records**/sec; burst ≥ 30,000 **Usage Records**/sec for ≤ 5 minutes per 60-minute window; ≥ 100 concurrent aggregation queries; ≥ 700,000,000 accepted ingestion calls per 24-hour day.
- **Rationale**: Documenting the steady-state, peak, burst, and concurrent-consumer profile lets capacity planning, alert thresholds, and load tests share one product-level envelope.
- **Architecture Allocation**: See DESIGN.md

#### Operational Visibility

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-nfr-operational-visibility`

Usage Collector domain metrics **MUST** be integrated into shared platform dashboards and alert routing. At minimum, operator treatment **MUST** exist for ingestion latency, ingestion error rate, query latency, PDP error rate, storage-plugin readiness, and GTS type resolution failures and declaration-cache staleness (`cpt-cf-usage-collector-fr-usage-type-resolution`). Every accepted and rejected API operation **MUST** emit a structured log entry carrying the correlation identifier propagated unchanged from the inbound platform-resolved security context, so gear activity reconciles with platform gateway access logs.

#### Billing Feed Freshness

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-nfr-billing-feed-freshness`

This NFR is a **plugin readiness gate**, not a gear-level upper bound: the gear floor declared by `cpt-cf-usage-collector-nfr-query-freshness` — query-surface visibility is eventually consistent with **no upper bound** — is unchanged.

A storage plugin deployment is fit to serve a charging consumer only if the consistency ceiling it publishes under `cpt-cf-usage-collector-nfr-query-freshness` bounds acceptance → feed visibility (`cpt-cf-usage-collector-fr-billing-usage-feed`) at **≤ 5 minutes p95**, under the `cpt-cf-usage-collector-nfr-throughput-profile` envelope, measured over a ≥ 30-minute steady-state window. A deployment whose active plugin publishes no qualifying ceiling **MUST NOT** be used to feed a charging consumer; the condition is surfaced at storage-plugin readiness review alongside `cpt-cf-usage-collector-fr-billing-retention-floor`.

- **Threshold**: Gear floor: no numeric bound (absence claim, per `cpt-cf-usage-collector-nfr-query-freshness`). Readiness gate: published p95 ≤ 5 minutes acceptance → feed visibility, verified against the plugin's published consistency profile at release-readiness review.
- **Rationale**: A pull feed is only a valid substitute for push if its staleness is bounded and published — but that bound cannot honestly be asserted gear-wide, because the gear delegates storage and the query-freshness contract deliberately publishes no gear-level ceiling. Expressing the 5-minute requirement as a qualifying condition on the plugin's own published ceiling gets the guarantee a charging consumer needs without overpromising on behalf of backends the gear does not control, and reuses the floor-and-ceiling machinery already established rather than contradicting it.
- **Architecture Allocation**: See DESIGN.md.

#### Bulk Replay Throughput

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-nfr-replay-throughput`

The usage feed (`cpt-cf-usage-collector-fr-billing-usage-feed`) **MUST** meet a **recovery objective**: a consumer that has fallen behind **MUST** be able to return to the live watermark within a bounded recovery time, without breaching the ingestion SLOs (`cpt-cf-usage-collector-nfr-ingestion-latency`, via `cpt-cf-usage-collector-nfr-workload-isolation`). Launch objective: **24 hours behind → caught up within 6 hours.**

A catching-up consumer drains its backlog while new **Usage Records** keep arriving, so the required read rate is a **multiple of the arrival rate of the consumer's own subscription**, not an absolute constant:

> required read rate ≥ subscribed arrival rate × (1 + backlog age / recovery time) — at the launch objective, at least five times the subscribed arrival rate

The subscribed arrival rate is the ingestion rate of the GTS types a given consumer subscribes to (`cpt-cf-usage-collector-fr-billing-usage-feed`), not the gear-wide rate. The obligation is therefore per subscription, and a consumer cannot be made to pay for traffic it does not read.

- **Threshold**: Recovery objective met (24h backlog cleared within 6h) with ingestion p95 within bounds throughout, measured against the subscription under test. At the launch planning assumption of ≤ 10,000,000 **Usage Records**/hour/region for a charging consumer's subscription, this yields a sustained bulk read rate of ≥ 50,000,000 **Usage Records**/hour/region. A subscription covering the full gear-wide envelope (10,000 **Usage Records**/sec ≈ 36M/h) would instead require ≥ 180,000,000 **Usage Records**/hour/region, which is why the obligation is scoped to a subscription.
- **Rationale**: Stated as a bare constant this requirement silently fails to deliver recovery. Net drain is the read rate minus the subscribed arrival rate, so a flat 50M/h against a stream arriving at the gear-wide envelope of `cpt-cf-usage-collector-nfr-throughput-profile` (10,000 **Usage Records**/sec ≈ 36M/h) leaves only ~14M/h of drain, and a 24-hour backlog would take roughly 60 hours to clear — the number looks like a guarantee while providing none. The multiple makes both terms explicit: the guarantee is the recovery objective, and the absolute rate follows from whatever share of the envelope a given consumer actually subscribes to. Anchoring on the subscription rather than on the gear-wide envelope is what keeps the obligation bounded: the envelope covers all telemetry, including high-volume sources no charging consumer reads, and sizing every consumer against it would demand roughly 180M/h for a consumer that rates a handful of meters. The subscribed rate is a planning assumption recorded in [§11](#11-assumptions) and **MUST** be revalidated as meters are onboarded, because the required read rate scales directly with it.
- **Architecture Allocation**: See DESIGN.md

### 6.2 NFR Exclusions

The following commonly applicable NFR categories are not applicable to this gear:

- **Safety (ISO/IEC 25010:2023 §4.2.9)**: Not applicable — the Usage Collector is a server-side data API with no physical interaction, no safety-critical operations, and no ability to cause harm to people, property, or the environment.
- **End-user UI accessibility and usability**: Not applicable — the Usage Collector exposes no user-facing UI. Developer, API consumer, and operator experience is delivered through the SDK trait, REST API, and platform-level documentation and support channels.
- **Internationalization / Localization**: Not applicable — the gear exposes no user-facing text, labels, or locale-sensitive output.
- **Privacy by Design (GDPR Art. 25) as a standalone regulatory conformance claim**: Not applicable. Subject IDs stored by the Usage Collector are opaque internal platform identifiers; PII management is the responsibility of the platform identity layer (cross-reference [§5.3](#53-attribution--isolation) Subject Attribution). Standalone GDPR Article 25 conformance is governed at platform level.
- **Regulatory Compliance (GDPR, HIPAA, PCI DSS, SOX) as standalone gear obligations**: Not applicable — this is an internal platform infrastructure gear. The gear handles no payment card data (PCI DSS N/A), no healthcare records (HIPAA N/A), and no financial-reporting source data (SOX N/A). Platform-level regulatory obligations are governed at the platform level.
- **Consent Management and Data Subject Rights (DSR) workflows**: Not applicable at gear level. Consent capture, withdrawal, and data-subject-rights execution (access, rectification, erasure, restriction, portability, objection) are owned by the platform identity, legal, and governance layers; the Usage Collector does not host a gear-local consent store or DSR workflow.
- **Data Sovereignty and Cross-Border Transfer policy at gear level**: Not applicable. Data residency, cross-border transfer restrictions, and replication topology are governed by the platform deployment topology and the operator-selected storage plugin's deployment profile (cross-reference [§4.2](#42-out-of-scope) deferred Multi-Region Replication).
- **Gear-Specific Disaster Recovery**: Not applicable as a standalone gear requirement. Recovery Point Objective (RPO), Recovery Time Objective (RTO), backup, and restore posture are governed by the platform's general disaster-recovery posture and the operator-selected storage backend's own DR mechanisms; the Usage Collector does not define gear-specific recovery thresholds.
- **Device / Platform Requirements (UX-PRD-004)**: Not applicable — the Usage Collector is server-side platform infrastructure with no UI client. It is consumed exclusively via the in-process SDK trait (`cpt-cf-usage-collector-interface-sdk-client`), the Plugin SPI (`cpt-cf-usage-collector-interface-plugin`), and the REST API (`cpt-cf-usage-collector-interface-rest-api`); no browser, mobile, desktop, offline, or responsive-design surfaces exist, so per-device, per-platform, and offline-mode obligations do not apply at gear level.
- **Inclusivity Requirements (UX-PRD-005)**: Not applicable — the Usage Collector serves a narrow technical audience (platform developers, platform operators, tenant administrators, and downstream consumer services) through the in-process SDK, Plugin SPI, and REST API. The gear exposes no end-user UI surface, no per-subject profile view, and no human-targeted content, so cognitive-accessibility, diverse-user-population, and cultural-sensitivity obligations remain at the platform level rather than being asserted as standalone gear obligations.

## 7. Public Library Interfaces

### 7.1 Public API Surface

The Usage Collector exposes three public surfaces: an in-process SDK trait consumed by platform gears, a Plugin SPI implemented by storage extensions, and a REST API consumed by remote usage sources, operator tooling, and downstream consumers. The REST API is the full product surface for ingestion, query, and health. None of the three carries a GTS type operation, read or write: declaring, amending, withdrawing, and reading GTS types are `types-registry` surfaces (`cpt-cf-usage-collector-fr-usage-type-declaration`). The SDK trait is a narrower in-process consumer surface, while the Plugin SPI is the storage-extension surface. The entries below describe stable capability surfaces at PRD level; detailed signatures and wire contracts are defined in DESIGN.md and the linked contract documents.

#### Usage Collector SDK

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-interface-sdk-client`

**Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-platform-developer`, `cpt-cf-usage-collector-actor-usage-consumer`

<!-- cpt-cf-id-content -->

**Type**: In-process async client trait
**Stability**: pre-1.0 (`V1`)
**Description**: In-process consumer surface covering ingestion of **Usage Records** (`cpt-cf-usage-collector-fr-ingestion`, `cpt-cf-usage-collector-fr-record-invalidation`, `cpt-cf-usage-collector-fr-idempotency`), raw query (`cpt-cf-usage-collector-fr-query-raw`), and aggregated query (`cpt-cf-usage-collector-fr-query-aggregation`). There is no separate correction operation: a correction is an ingestion of an invalidation entry. Bulk historical import is a trait operation of its own (`cpt-cf-usage-collector-fr-backfill`): it is the only route reaching past the live past tolerance, for a **Usage Record** and a withdrawal alike. The aggregated query carries no aggregation parameter — the fold is resolved from the queried GTS type (`cpt-cf-usage-collector-fr-aggregation-fold`). It also carries the read side an in-process charging consumer needs: the usage feed with subscription, cursor, and watermark (`cpt-cf-usage-collector-fr-billing-usage-feed`) and point lookup by **Usage Record** identifier (`cpt-cf-usage-collector-fr-record-identity`). Operator operations are intentionally REST-only, including quota configuration and reconciliation metadata. GTS type declaration appears on no Usage Collector surface at all: an in-process gear that needs a new meter registers it with `types-registry` (`cpt-cf-usage-collector-fr-usage-type-declaration`) and then emits against it.
**Consumed / Provided Data**: consumes **Usage Record** submissions, backfill imports, raw and aggregated query requests, feed subscription declarations, cursor-based feed page requests, and **Usage Record** identifier point lookups; provides acceptance acknowledgements, raw usage views, aggregated usage results, and feed pages with cursor and watermark. Operator-only data classes are intentionally not exposed on this trait.
**Availability / Fallback**: in-process trait availability follows the Usage Collector gear and its active storage dependency. The SDK does not provide an alternate persistence path or synthesize usage data.
**Breaking Change Policy**: From the 1.0 release onward, any change to the contract of an existing operation needs a major version bump. Within a major version, only additive changes ship, and existing consumers need not react to them. The platform then supports one previous major version to give consumer gears a migration window, consistent with `cpt-cf-usage-collector-nfr-plugin-contract-stability`.
See DESIGN.md for the interface contract.

<!-- cpt-cf-id-content -->

#### Plugin SPI

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-interface-plugin`

**Actors**: `cpt-cf-usage-collector-actor-storage-backend`

<!-- cpt-cf-id-content -->

**Type**: Storage plugin SPI
**Stability**: pre-1.0 (`V1`)
**Description**: Storage-extension surface implemented by each plugin for persistence of **Usage Records** (`cpt-cf-usage-collector-fr-pluggable-storage`, `cpt-cf-usage-collector-fr-record-invalidation`) and raw and aggregated query (`cpt-cf-usage-collector-fr-query-raw`, `cpt-cf-usage-collector-fr-query-aggregation`). The SPI carries no operation that modifies a persisted **Usage Record**; a plugin's aggregation must honour the withdrawal effect of an accepted invalidation on the **Usage Record** it refers to, which for any pre-computed or materialised aggregate means recomputation over the affected range rather than a further contribution to it. The operator selects the active backend via configuration (see `cpt-cf-usage-collector-fr-pluggable-storage`).
**Consumed / Provided Data**: consumes **Usage Record** persistence requests and raw and aggregated query requests; provides persistence acknowledgements, raw usage views, and aggregated usage results.
**Availability / Fallback**: backend-bound — the SPI's availability tracks the selected storage backend. There is no parallel storage path in the Usage Collector.
**Breaking Change Policy**: The plugin contract is versioned with the gear. From the 1.0 release onward, a breaking contract change needs a coordinated release with every plugin implementation, and the platform supports one previous major version of the Plugin SPI to give plugin authors a migration window.
See DESIGN.md for the interface contract.

<!-- cpt-cf-id-content -->

#### REST API

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-interface-rest-api`

**Actors**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-usage-consumer`, `cpt-cf-usage-collector-actor-platform-operator`, `cpt-cf-usage-collector-actor-tenant-admin`

<!-- cpt-cf-id-content -->

**Type**: HTTP REST API
**Stability**: pre-1.0 (`V1`)
**Description**: HTTP API consumed by remote usage sources, operator tooling, and downstream consumers. This REST surface is the full product operation surface for the gear. Capability categories:

- Ingestion of **Usage Records** — `cpt-cf-usage-collector-fr-ingestion`, `cpt-cf-usage-collector-fr-record-invalidation`, `cpt-cf-usage-collector-fr-idempotency`; subject to per-caller and per-(caller, tenant) quotas — `cpt-cf-usage-collector-fr-rate-limiting`. Corrections have no endpoint of their own: a correction is an ingestion of an invalidation entry, and no endpoint modifies an accepted **Usage Record**
- Raw query — `cpt-cf-usage-collector-fr-query-raw`
- Aggregated query — `cpt-cf-usage-collector-fr-query-aggregation`
- GTS type declarations have no endpoint at all — no read and no write. The gear resolves declarations internally for ingestion and query, and publishes none of that resolution as an operation (`cpt-cf-usage-collector-fr-usage-type-resolution`). A caller that needs to read a declaration, and an operator that needs to declare or withdraw one, goes to `types-registry` (`cpt-cf-usage-collector-fr-usage-type-declaration`)
- Health

Metering and feed capability categories (see [§5.9](#59-billing-integration)):

- Usage feed — per-GTS-type subscription, cursor-paginated, snapshot-consistent, replay-safe reads with watermarks — `cpt-cf-usage-collector-fr-billing-usage-feed`, `cpt-cf-usage-collector-fr-billing-fields-on-read`
- Point lookup of a single **Usage Record** by its identifier — `cpt-cf-usage-collector-fr-record-identity`
- GTS type declaration attributes — the declared aggregation fold, the bound metering unit, the retention policy, and the nominal sampling interval where one is declared — are **not** served by this surface. A metering consumer reads them from `types-registry`, for the GTS type an entry references (`cpt-cf-usage-collector-fr-aggregation-fold`, `cpt-cf-usage-collector-fr-metering-unit-binding`, `cpt-cf-usage-collector-fr-quantity-semantics`, `cpt-cf-usage-collector-fr-billing-retention-floor`)
- Dedicated bulk backfill import, isolated from live ingestion and carrying the backfill origin marker — `cpt-cf-usage-collector-fr-backfill`
- Reconciliation metadata and watermarks per scope — `cpt-cf-usage-collector-fr-reconciliation-metadata`

The API specification (sibling to DESIGN.md) is authoritative for the detailed wire contract, the endpoint enumeration, per-endpoint stability, and the canonical error envelope; the major-version stability contract is declared there as well. Technical API details are intentionally not duplicated here.

**Consumed / Provided Data**: consumes **Usage Record** submissions, backfill imports, raw and aggregated query requests, feed subscription declarations, cursor-based feed page requests, **Usage Record** identifier point lookups, reconciliation-metadata requests, and health requests; provides ingestion acknowledgements, throttle errors with retry guidance, raw usage views, aggregated usage results, feed pages with cursor and watermark, reconciliation metadata and watermarks, health visibility, and platform-standard errors.
**Availability / Fallback**: served behind the platform API gateway; authentication is performed by the platform gateway upstream of the collector, and PDP authorization is on the critical path. Read availability follows `cpt-cf-usage-collector-nfr-availability`.
**Breaking Change Policy**: From the 1.0 release onward, endpoint removal or an incompatible request or response schema change needs a major version bump (v1 → v2). Within v1, only additive changes ship: new endpoints and new optional fields. The platform then supports one previous major version of the REST API to give remote consumers a migration window, consistent with `cpt-cf-usage-collector-nfr-plugin-contract-stability`.
See DESIGN.md for endpoint contracts.

<!-- cpt-cf-id-content -->

### 7.2 External Integration Contracts

The Usage Collector requires three platform dependencies — Platform PDP, `types-registry`, and platform registry/orchestration services for storage extension selection — and provides two outward contracts: a Storage Plugin Contract for storage extensions and a Downstream Usage Reader Contract for billing, quota enforcement, dashboards, and platform monitoring consumers. Caller authentication is performed by the ToolKit gateway upstream of the collector and is not an outbound dependency declared by this gear.

#### Platform PDP Contract

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-contract-authz-resolver`

<!-- cpt-cf-id-content -->

**Direction**: required from `authz-resolver`
**Protocol/Format**: Platform PDP authorization decisions for every ingestion, query, and operator-write operation.
**Consumed / Provided Data**: consumes caller identity and product-level operation context; receives permit/deny decisions and any authorized read-scope constraints.
**Availability / Fallback**: PDP authorization is on the critical path for every ingestion, query, and operator-write call; there is no fallback or cached-decision path. When the PDP is unreachable, all authorized operations fail closed (denied) with a deterministic platform-authorization error; the Usage Collector does not serve cached decisions or invent a permissive fallback.
**Compatibility**: Contract follows the platform authorization protocol; changes require coordinated release.

<!-- cpt-cf-id-content -->

#### Types Registry Contract

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-contract-types-registry`

<!-- cpt-cf-id-content -->

**Direction**: required from `types-registry`
**Protocol/Format**: GTS type resolution. The Usage Collector resolves a GTS type reference to its registered declaration — aggregation fold, canonical metering unit, metadata surface, retention policy, and the optional nominal sampling interval (`cpt-cf-usage-collector-fr-usage-type-declaration`).
**Consumed / Provided Data**: consumes GTS type references; receives the corresponding declarations. The Usage Collector mints, amends, and withdraws no declaration, and sends no usage data to the registry. It makes one temporary write: to recover a declaration the registry lost, it registers that declaration again.
**Availability / Fallback**: resolution is on the ingestion path, so this contract is load-bearing for `cpt-cf-usage-collector-nfr-ingestion-latency` and `cpt-cf-usage-collector-nfr-throughput`. The Usage Collector **MUST** serve the steady state from a local cache of resolved declarations, and cached declarations **MUST** remain usable while the registry is unreachable, so a registry outage blocks the introduction of new GTS types rather than the ingestion of existing ones. Where the registry has restarted and lost a declaration, the Usage Collector recovers it on a best-effort basis. There is no permissive fallback: a GTS type that does not resolve — from the registry, from cache, or by recovery — causes the **Usage Record** to be rejected (`cpt-cf-usage-collector-fr-usage-type-resolution`). Declaration immutability is what makes cached declarations safe to serve indefinitely.
**Compatibility**: The registry owns declaration storage, lifecycle, and authorization; the Usage Collector owns the set of attributes a declaration must carry to be usable as a meter. A change to those required attributes requires a coordinated release with `types-registry` and with every other gear reading the same declarations (notably Quota Enforcement, which resolves the same meters).

<!-- cpt-cf-id-content -->

#### Storage Extension Registry / Orchestration Contract

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-contract-gts-registry`

<!-- cpt-cf-id-content -->

**Direction**: required from platform registry and orchestration services
**Protocol/Format**: Platform registry and orchestration services support operator-selected storage extension resolution and lifecycle.
**Consumed / Provided Data**: consumes the operator-selected storage extension identity; receives the active storage extension needed for persistence and query capability.
**Availability / Fallback**: Storage extension resolution is required for gear readiness. When the required registry or orchestration dependency is unavailable during startup, the Usage Collector does not advertise readiness.
**Compatibility**: Selector identifiers follow the platform registry and orchestration protocols; changes require a coordinated release with the registry, the orchestrator, and every plugin implementation.

<!-- cpt-cf-id-content -->

#### Storage Plugin Contract

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-contract-storage-plugin`

<!-- cpt-cf-id-content -->

**Direction**: provided by library (Plugin SPI offered to plugin authors implementing storage backends)
**Protocol/Format**: Storage Plugin SPI (`cpt-cf-usage-collector-interface-plugin`) implemented by storage backends selected by operators.
**Consumed / Provided Data**: the Usage Collector dispatches persistence, raw query, and aggregated query requests; plugins return acknowledgements and usage results. Plugins **MUST NOT** invent **Usage Records**, and **MUST NOT** modify a persisted **Usage Record**.
**Availability / Fallback**: A plugin's availability is its own concern; when the active plugin is unavailable, affected persistence and query operations fail with platform-standard errors. There is no parallel local storage path in the Usage Collector.
**Compatibility**: The Plugin SPI follows `cpt-cf-usage-collector-nfr-plugin-contract-stability`. From the 1.0 release onward, a plugin built against major version `N` keeps working against every `N.x` release, and a breaking change ships as a new major that coexists with the prior major during a migration window. Plugins ship on release schedules independent of the Usage Collector.

<!-- cpt-cf-id-content -->

#### Downstream Usage Reader Contract

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-contract-downstream-usage-reader`

<!-- cpt-cf-id-content -->

**Direction**: provided by library (read-only usage views consumed by downstream readers: billing, quota enforcement, dashboards, and platform monitoring)
**Protocol/Format**: Public REST API `cpt-cf-usage-collector-interface-rest-api` for out-of-process readers and, for in-process platform gears, the SDK trait `cpt-cf-usage-collector-interface-sdk-client`.
**Consumed / Provided Data**: downstream readers submit raw and aggregated query requests and health requests where applicable; the Usage Collector returns raw usage views, aggregated usage results, and health visibility. This contract also carries the usage feed: readers declare a GTS type subscription, submit cursor-based feed requests and **Usage Record** identifier point lookups, and the Usage Collector returns snapshot-consistent pages with an explicit watermark, the unstripped field set, and correction linkage sufficient to reconstruct correction history (`cpt-cf-usage-collector-fr-billing-usage-feed`, `cpt-cf-usage-collector-fr-billing-fields-on-read`). Business logic (pricing, rating, invoice generation, quota enforcement decisions) **MUST NOT** be performed inside the Usage Collector; it is the responsibility of the downstream reader. This explicitly includes resolving commercial identity (subscription, SKU, payer, seller) and integrating a series of observations into period quantities, which the gear neither performs nor offers a fold for (`cpt-cf-usage-collector-fr-quantity-semantics`, `cpt-cf-usage-collector-fr-aggregation-fold`).
**Availability / Fallback**: Query availability and latency follow `cpt-cf-usage-collector-nfr-query-latency` and `cpt-cf-usage-collector-nfr-availability`. PDP authorization is on the critical path and is fail-closed. Downstream readers **MUST NOT** invent usage state when the Usage Collector is unavailable. Feed staleness is bounded only by the active plugin's published consistency ceiling, which must meet `cpt-cf-usage-collector-nfr-billing-feed-freshness` for a deployment to feed a charging consumer; readers **MUST NOT** assume a tighter bound than the ceiling their deployment publishes. Recovery from a reader outage is by cursor replay, bounded by the retention floor in `cpt-cf-usage-collector-fr-billing-retention-floor`; duplicate delivery across an overlapping replay is expected and is deduplicated downstream by **Usage Record** identifier.
**Compatibility**: Read shapes follow the public versioning policy of the Usage Collector. From the 1.0 release onward, at most one prior major version of the REST API and SDK trait stays supported, which gives downstream readers a migration window. Additive changes within a major version do not break existing readers.

<!-- cpt-cf-id-content -->

### 7.3 Endpoints Summary

The canonical endpoint surface is defined in `usage-collector-v1.yaml` (sibling file) and mirrored in DESIGN.

## 8. Use Cases

#### Emit Usage Records

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-usecase-emit`

**Actor**: `cpt-cf-usage-collector-actor-usage-source`

**Preconditions**:

- Actor is an authenticated usage source
- PDP authorization policies declare which GTS types the source is permitted to emit and for which tenants

**Main Flow**:

1. Usage source emits a **Usage Record** attributed to a tenant, resource, optional subject, and a registered GTS type
2. System authorizes the emission via PDP and validates the **Usage Record** against the registered GTS type — its declared metadata surface and its bound metering unit. Validation does not depend on the declared aggregation fold, and the quantity's sign is not constrained. Any failure is returned immediately to the caller before any **Usage Record** is accepted.
3. System accepts the **Usage Record**
4. The **Usage Record** becomes available for querying in the Usage Collector

**Postconditions**:

- Authorized, valid **Usage Records** are persisted in the storage backend and available for aggregation queries
- An exact-equality re-submission under an already-accepted idempotency key is silently deduplicated (no duplicate **Usage Record**); a same-key submission whose content differs is rejected with an actionable conflict error rather than silently dropped (cross-reference `cpt-cf-usage-collector-fr-idempotency`)

**Alternative Flows**:

- **Authorization denied**: System returns an error immediately; no **Usage Record** is accepted for delivery
- **Validation failed**: System returns an actionable error immediately; no **Usage Record** is accepted for delivery

#### Query Aggregated Usage

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-usecase-query-aggregated`

**Actor**: `cpt-cf-usage-collector-actor-usage-consumer`, `cpt-cf-usage-collector-actor-tenant-admin`

**Preconditions**:

- Actor is authenticated with a valid security context

**Main Flow**:

1. Consumer sends an aggregation query specifying a time range, GTS type, and desired grouping or rollup
2. System authorizes the query via PDP; PDP-returned constraints define the authorization boundary and user-supplied filters are applied in addition, only further narrowing the result set
3. System returns aggregated results scoped to the intersection of PDP-authorized scope and user-supplied filters

**Postconditions**:

- Consumer receives aggregated usage data within the intersection of PDP-authorized scope and user-supplied filters

**Alternative Flows**:

- **No data in range or scope**: System returns empty result set (not an error)
- **PDP denial or empty constraints**: System rejects the query immediately; no data is returned

#### Register a Meter in Types Registry

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-usecase-declare-usage-type`

**Actor**: `cpt-cf-usage-collector-actor-platform-operator`

This use case describes the prerequisite platform flow for making a meter usable by the Usage Collector. The registration write happens in `types-registry`; the Usage Collector only resolves the resulting declaration and validates **Usage Records** against it.

**Preconditions**:

- Actor is authenticated with a valid security context carrying permission to register types in `types-registry`
- The GTS type's identifier is unique across the deployment

**Main Flow**:

1. Operator authors the GTS type, declaring its aggregation fold, canonical metering unit, metadata surface, retention policy, and optionally a nominal sampling interval
2. Operator registers the declaration with `types-registry`, which authorizes and stores it
3. Operator configures PDP authorization policies declaring which calling-gear identities are permitted to emit **Usage Records** referencing this GTS type, and for which tenants (the PDP reads the calling-gear identity from the platform-resolved security context at emit time). These policies may be written as wildcard patterns over the type hierarchy rather than one type at a time
4. The Usage Collector resolves the new declaration on first reference and caches it (`cpt-cf-usage-collector-fr-usage-type-resolution`)
5. **Usage Records** referencing the GTS type are thereafter validated against the declaration and accepted

**Postconditions**:

- The new GTS type is available for ingestion across all tenants with no Usage Collector-side action; calling gears emit **Usage Records** referencing it
- PDP policies are in effect; unauthorized callers are rejected when attempting to emit **Usage Records** referencing this GTS type

**Alternative Flows**:

- **Duplicate identifier**: `types-registry` rejects the registration; no declaration is created
- **Missing or non-canonical metering unit**: **Usage Records** referencing the GTS type are rejected at ingestion (`cpt-cf-usage-collector-fr-metering-unit-binding`)
- **Missing or unrecognised aggregation fold**: the declaration is unusable as a meter — the aggregate query path has no fold to serve, so an aggregation request against the GTS type is rejected (`cpt-cf-usage-collector-fr-aggregation-fold`)
- **PDP denial**: `types-registry` rejects the registration before any change is made

#### Query Raw Usage Records

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-usecase-query-raw`

**Actor**: `cpt-cf-usage-collector-actor-usage-consumer`, `cpt-cf-usage-collector-actor-tenant-admin`

**Preconditions**:

- Actor is authenticated with a valid security context

**Main Flow**:

1. Consumer sends a raw query specifying exactly one GTS type, a mandatory time range, and optional product-level narrowing criteria
2. System authorizes the query via PDP; PDP-returned constraints define the authorization boundary and user-supplied filters are applied in addition, only further narrowing the result set
3. System returns a page of raw **Usage Records** when authorized **Usage Records** exist

**Postconditions**:

- Consumer receives raw **Usage Records** within the intersection of PDP-authorized scope and user-supplied filters
- Additional pages are available through the paging behavior defined by the public contract

**Alternative Flows**:

- **No data in range or scope**: System returns an empty page (not an error)
- **PDP denial or empty constraints**: System rejects the query immediately; no data is returned
- **Invalid paging request**: System returns an actionable error

#### Report a Decrease in Measured Consumption

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-usecase-report-decrease`

**Actor**: `cpt-cf-usage-collector-actor-usage-source`

**Preconditions**:

- The calling gear is an authenticated usage source with PDP authorization to emit **Usage Records** for the target tenant and GTS type

**Trigger**: The calling gear establishes a real give-back of measured consumption — a capacity refund, a partial revocation, a downward adjustment it computed before submission — for a meter declared `SUM`.

**Main Flow**:

1. Calling gear constructs an ordinary **Usage Record** carrying a negative quantity, its own covered period, the usual attribution, and a mandatory idempotency key. It carries **no** reference to any earlier **Usage Record** and **no** reason code: this is a measurement, not a correction (cross-reference [§5.6](#56-corrections)).
2. Calling gear submits it through the ordinary ingestion path.
3. System authorizes the emission via PDP and applies exactly the validation any other **Usage Record** receives. The sign of the quantity is not examined.
4. System accepts the **Usage Record** and appends it to the store.
5. The aggregated `SUM` for the affected scope and range is reduced accordingly, and the entry appears on the feed at its own acceptance position.

**Postconditions**:

- A new **Usage Record** is persisted; no earlier **Usage Record** is referenced, altered, or withdrawn.
- The aggregated `SUM` reflects the decrease, and a consumer summing the records reaches the same figure.

**Alternative Flows**:

- **Net-negative range**: the signed total over a queried range may be negative; the system does not validate a non-negative net and emits no negative-net detection, alerting, or downstream reconciliation. Per-record outstanding balances and lot / FIFO-LIFO tracking are explicit non-goals.
- **Meter is not declared `SUM`**: the record is accepted like any other, but the decrease has no aggregate meaning, since quantities under any other fold are observations rather than accrued amounts (`cpt-cf-usage-collector-fr-aggregation-fold`).
- **Idempotency conflict / retry**: an exact-equality re-submission under the same idempotency key is silently deduplicated; a same-key submission whose content differs is rejected with an actionable conflict error.

#### Invalidate an Erroneous Usage Record

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-usecase-invalidate-record`

**Actor**: `cpt-cf-usage-collector-actor-usage-source`, `cpt-cf-usage-collector-actor-platform-operator`

**Preconditions**:

- The caller is authenticated with PDP authorization to emit **Usage Records** for the target tenant and GTS type; a platform operator correcting bad data acts through this same path rather than through an operator-only correction surface
- A prior **Usage Record** exists for that tenant and GTS type, and no invalidation entry already refers to it
- The caller submits on the path whose bounds admit the target's period: live inside the past tolerance, backfill beyond it (`cpt-cf-usage-collector-fr-live-future-time-bound`, `cpt-cf-usage-collector-fr-backfill`)

**Trigger**: The emitter or an operator establishes that the measurement was never true — a mis-measurement, a mis-attributed record, or an emitter defect.

**Main Flow**:

1. Caller computes the target's identifier from the attributes it already holds (`cpt-cf-usage-collector-fr-record-identity`), with no round-trip.
2. Caller constructs an invalidation entry marked as such, referring to that identifier and copying the target's caller-supplied fields — attribution, covered period, quantity, and metadata — plus a reason code, a mandatory idempotency key distinct from the target's, and the platform-resolved security context.
3. Caller submits it via the **same ingestion path** used for **Usage Records** — live, or backfill where the target's period predates the live past tolerance; no surface offers an operation that retires or otherwise modifies the target in place (cross-reference `cpt-cf-usage-collector-fr-record-invalidation`).
4. System authorizes the emission via PDP and validates the invalidation: the target exists, is not itself an invalidation, is matched field for field by the copy, is not already invalidated, and carries a reason code; the copied period is bounded by the path, as a **Usage Record**'s period is.
5. System accepts the invalidation entry and appends it to the store.
6. Neither the target nor the invalidation entry contributes to any aggregation selecting them — and since they share a covered period, every range selects both or neither — while both stay on the ledger read paths as persisted; the appended invalidation is the signal that withdraws the target.

**Postconditions**:

- The target is unchanged on the ledger and remains queryable; the appended invalidation is what makes it read as withdrawn.
- No aggregation includes the target, and any pre-computed aggregate covering it is recomputed rather than adjusted by a further contribution.
- Both entries appear in the usage feed at their own acceptance positions, so a consumer that already rated the target learns of the withdrawal from the later entry, which names the target it withdraws.

**Alternative Flows**:

- **Already-invalidated target rejected**: an invalidation entry already refers to the target; ingestion rejects the submission with an actionable error, since a **Usage Record** is withdrawn at most once.
- **Exact resubmission absorbed**: a re-submission under the same idempotency key with identical content is silently deduplicated rather than treated as a second invalidation (cross-reference `cpt-cf-usage-collector-fr-idempotency`).
- **Unreferenced submission is not a correction**: the submission copies the target's fields but names no target. Carrying a reason code without a reference, it is malformed and rejected with an actionable error; carrying neither, it is accepted under its own idempotency key as an ordinary — here duplicate — measurement, since no quantity value identifies a correction (`cpt-cf-usage-collector-fr-record-quantity`).
- **Invalid reference rejected**: the target is missing or is itself an invalidation entry; ingestion rejects the submission with an actionable error.
- **Copy mismatch rejected**: a caller-supplied field of the invalidation — attribution, including whether a subject is present, covered period, quantity, or metadata — differs from the target's; ingestion rejects the submission with an actionable error naming the field that differs.
- **Target older than the live past tolerance, on the live path**: ingestion rejects it with an actionable error naming the instant, the bound, and the backfill route, exactly as it rejects an ordinary **Usage Record** of that period (`cpt-cf-usage-collector-fr-live-future-time-bound`). The caller resubmits on that route.
- **Target older than the backfill window**: the backfill path rejects it with an actionable error naming the bound. No surface admits that covered period, so the target can no longer be withdrawn.
- **Missing reason code rejected**: the invalidation carries no reason code; ingestion rejects it with an actionable error (cross-reference `cpt-cf-usage-collector-fr-invalidation-reason-code`).
- **Authorization denied**: PDP denies the emission; the invalidation is rejected immediately and never persisted.
- **Supplying the corrected measurement**: the invalidation copies the withdrawn quantity and carries no replacement for it; the caller emits a fresh **Usage Record** under a new idempotency key, carrying the same attribution and the same covered period, which the system accepts as a new and distinct **Usage Record**.
- **Withdrawing many records**: v1 withdraws one **Usage Record** per invalidation entry; a caller correcting a bulk emitter defect submits many entries through the batched ingestion path, subject to its quotas (`cpt-cf-usage-collector-fr-rate-limiting`). Where those **Usage Records** are older than the live past tolerance — the usual case — the batch travels the backfill path and runs under its workload isolation (`cpt-cf-usage-collector-fr-backfill`).

#### Declare a Meter for Charging

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-usecase-declare-meter`

**Actor**: `cpt-cf-usage-collector-actor-platform-operator`

**Preconditions**:

- Actor is authenticated with a valid security context carrying permission to register types in `types-registry`
- The GTS type's identifier is unique across the deployment
- The active storage plugin publishes a consistency ceiling that meets `cpt-cf-usage-collector-nfr-billing-feed-freshness` and a deployment profile that honours the retention policy the meter will carry (`cpt-cf-usage-collector-fr-billing-retention-floor`)

**Main Flow**:

1. Operator authors a GTS type declaration carrying the aggregation fold, a metering unit drawn from the canonical unit list, the metadata surface, a retention policy, and optionally a nominal sampling interval. A meter intended for charging is declared `SUM`, since no other fold yields an amount consumed over a period (`cpt-cf-usage-collector-fr-aggregation-fold`)
2. Operator registers the declaration with `types-registry`, which authorizes and stores it (`cpt-cf-usage-collector-usecase-declare-usage-type`)
3. The Usage Collector resolves the declaration and validates it for metering completeness: the metering unit is canonical and the declared fold is one the gear serves
4. **Usage Records** referencing this GTS type are thereafter validated against the metering model and appear on the feed for consumers subscribed to it

**Postconditions**:

- The GTS type is declared with a bound unit and exactly one aggregation fold; ingestion enforces covered periods, unit binding, and the read field set for it
- Accepted **Usage Records** appear on the feed with the unstripped field set for any consumer whose subscription includes this GTS type

**Alternative Flows**:

- **Non-canonical or missing metering unit**: the GTS type does not become a usable meter, and every **Usage Record** referencing it is rejected with an actionable error (`cpt-cf-usage-collector-fr-metering-unit-binding`)
- **Level meter declared for charging**: a meter declared `MAX`, `MIN`, or `LATEST` produces no chargeable period quantity; the meter must instead be pre-integrated at the emitter into an accrued unit and declared `SUM` (`cpt-cf-usage-collector-fr-aggregation-fold`)
- **Attempt to rebind a unit on an existing GTS type**: prohibited — a bound unit is immutable, and a meter that must change unit is a new GTS type at a new GTS major version (`cpt-cf-usage-collector-fr-usage-type-declaration`, `cpt-cf-usage-collector-fr-metering-unit-binding`)
- **Deployment cannot honour the meter's retention or freshness**: Surfaced as a readiness failure at storage-plugin readiness review rather than accepted silently
- **PDP denial**: `types-registry` rejects the declaration before any change is made

#### Consume the Billing Usage Feed

- [ ] `p1` - **ID**: `cpt-cf-usage-collector-usecase-consume-billing-feed`

**Actor**: `cpt-cf-usage-collector-actor-usage-consumer`

**Preconditions**:

- Actor is an authenticated billing consumer with PDP authorization for the requested scope
- At least one GTS type is declared and the consumer's subscription includes it

**Main Flow**:

1. Consumer requests a page of the feed for its declared GTS type subscription, a scope, and a time range, supplying either no cursor (first page) or the cursor returned with the previous page
2. System authorizes the request via PDP; PDP-returned constraints define the authorization boundary and consumer-supplied filters only narrow it further
3. System returns a page over a consistent snapshot, excluding GTS types outside the subscription: entries ordered by the monotonic acceptance sequence scoped per (tenant, GTS type), each carrying the unstripped field set of `cpt-cf-usage-collector-fr-billing-fields-on-read`, together with a next cursor and an explicit watermark
4. Consumer processes the page, deduplicating by **Usage Record** identifier, and persists the cursor
5. Consumer repeats from the persisted cursor; **Usage Records** accepted after the snapshot are delivered on subsequent pages, demarcated by the watermark

**Postconditions**:

- Consumer has applied every feed entry in its subscription up to the watermark; delivery is at-least-once, and application is made effectively-once by deduplicating on **Usage Record** identifier and event position, with correction linkage sufficient to reconstruct correction history
- The persisted cursor allows the consumer to resume without rescanning processed ranges

**Alternative Flows**:

- **Consumer outage and replay**: Consumer resumes from its last persisted cursor, or replays from an earlier cursor within the retention floor; overlapping delivery is expected and is resolved by deduplication on **Usage Record** identifier (`cpt-cf-usage-collector-fr-billing-retention-floor`)
- **Correction arrives after processing**: The invalidation entry appears as its own feed entry referring to the withdrawn **Usage Record** by its identifier; the withdrawn record is never mutated in place, and no feed entry represents a change to one already delivered
- **Late-arriving Usage Record**: A **Usage Record** accepted later than the end of the period it covers is delivered in acceptance-sequence order, not period order; the consumer detects lateness by comparing the acceptance instant against the end of the covered period
- **Cursor older than the retention floor**: System returns an actionable error rather than a silently truncated range
- **PDP denial or empty constraints**: System rejects the request immediately; no data is returned

#### Backfill Historical Billing Usage

- [ ] `p2` - **ID**: `cpt-cf-usage-collector-usecase-backfill`

**Actor**: `cpt-cf-usage-collector-actor-platform-operator`, `cpt-cf-usage-collector-actor-usage-source`

**Preconditions**:

- Actor is authenticated and PDP-authorized for the bulk import path
- The target GTS type is declared with a bound metering unit

**Main Flow**:

1. Actor submits historical entries — **Usage Records**, invalidation entries, or both in one batch — to the dedicated backfill path on REST or the SDK trait, with covered periods inside the configured backfill window
2. System authorizes via PDP and applies the same billing validation as the live path (unit binding, metadata, idempotency, and the invalidation rules), replacing the live past tolerance with the backfill window
3. System persists accepted entries carrying the backfill origin marker, isolated from live ingestion workload
4. Those entries become available on the billing feed carrying the same origin marker

**Postconditions**:

- Historical entries are queryable and on the feed, distinguishable from live-path entries by their origin marker — a withdrawal of a closed period included
- Live-path ingestion SLOs are not breached by the backfill workload

**Alternative Flows**:

- **Covered period beyond the configured backfill bound**: System rejects the submission with an actionable error naming the bound; no override is accepted on any surface
- **Same Usage Record already accepted**: Idempotency applies unchanged — exact-equality re-submission is silently deduplicated; a same-key content mismatch is rejected with a conflict error
- **Future-dated or far-past-dated entry on the live path**: Rejected by `cpt-cf-usage-collector-fr-live-future-time-bound`. Anything beyond the live past bound belongs here, which the rejection names — an invalidation entry included, since it carries the period it withdraws (`cpt-cf-usage-collector-fr-record-invalidation`)
- **Withdrawing a historical Usage Record**: the caller submits the invalidation on this path, and it is persisted with the backfill origin marker. A bulk withdrawal after an emitter defect therefore runs under this path's workload isolation
- **Quota exceeded**: System returns an actionable throttle error with retry guidance; no **Usage Records** are silently dropped

## 9. Acceptance Criteria

The following definitions apply to every numeric acceptance criterion in this section that references a load condition or a latency tolerance. They replace the prior informal terms "normal load", "normal operation", and "linear throughput scaling" across the PRD and anchor every test condition on a single, deterministic envelope.

- **Load envelope ("normal load" / "normal operation")** — the steady-state operating envelope defined by `cpt-cf-usage-collector-nfr-throughput-profile`: sustained ingestion ≥ 10,000 **Usage Records**/sec, ≥ 100 concurrent aggregation queries, ≥ 700,000,000 accepted ingestion calls per 24-hour day, with no active burst in progress unless a criterion explicitly references the burst case. The burst case is ≤ 30,000 **Usage Records**/sec for ≤ 5 minutes per 60-minute window.
- **Steady-state measurement window** — a contiguous window of ≥ 30 minutes during which the load envelope above is sustained; p95 figures are computed over this window and the trailing 30-minute window is reported alongside any single-sample p95.
- **Latency tolerance** — every p95 latency criterion in [§9](#9-acceptance-criteria) carries a measurement tolerance of ±10% on the stated p95 value, applied per steady-state measurement window; the trailing 30-minute trend **MUST** remain at or below the stated p95 value.
- **Burst tolerance** — for the burst case of `cpt-cf-usage-collector-nfr-throughput-profile`, the p95 ingestion-latency bound (200ms with ±10% tolerance) applies for the duration of the burst (≤ 5 minutes) and the trailing 60-minute window MUST contain at most one burst event.

The functional and non-functional acceptance bullets below evaluate the requirements defined in [§5](#5-functional-requirements) and [§6](#6-non-functional-requirements) against the load envelope and measurement rules established above.

- [ ] Authenticated usage sources can submit **Usage Records** attributed to a tenant, resource, optional subject, and a registered GTS type; an accepted **Usage Record** becomes durably retained and queryable through the raw and aggregated query surfaces (cross-reference `cpt-cf-usage-collector-fr-ingestion`)
- [ ] **Usage Records** are stored as submitted without monotonicity enforcement and without delta accumulation; consecutive quantities for the same (tenant, GTS type) may rise or fall arbitrarily; idempotent dedup by idempotency key still applies; a raw query returns the persisted quantities rather than any accumulated or derived total (cross-reference `cpt-cf-usage-collector-fr-record-quantity`)
- [ ] An exact-equality re-submission under the same idempotency key results in a single stored **Usage Record** (silent dedup), while a same-key submission whose content differs is rejected with a duplicate-submission conflict signal rather than silently dropped (cross-reference `cpt-cf-usage-collector-fr-idempotency`)
- [ ] **Usage Records** submitted without an idempotency key are rejected with an actionable error
- [ ] No admissible submission falls outside its idempotency horizon: a covered period old enough for the referenced GTS type's retention to have lapsed is refused on the backfill window bound before deduplication is consulted, verified by a test that walks a submission's covered period from inside the window to beyond it and asserts a bound rejection rather than any deduplication outcome (cross-reference `cpt-cf-usage-collector-fr-idempotency`, `cpt-cf-usage-collector-fr-backfill`)
- [ ] Retention runs from the covered period rather than the acceptance instant, so a backfilled **Usage Record** arrives with its idempotency horizon already partly spent — at the far edge of the configured backfill window it retains one replay horizon rather than the whole floor; a re-run of that import is deduplicated by the gear while the window still admits it, and is refused on the bound once it does not, so no re-run reaches a state in which the gear admits a silent duplicate (cross-reference `cpt-cf-usage-collector-fr-backfill`, `cpt-cf-usage-collector-fr-billing-retention-floor`)
- [ ] No ingestion invariant consults the declared fold: an otherwise identical submission is accepted or rejected the same way whichever fold its GTS type declares, verified by a differential test across the fold set, and the sign of a quantity is never among the rejection causes (cross-reference `cpt-cf-usage-collector-fr-aggregation-fold`, `cpt-cf-usage-collector-fr-record-quantity`)
- [ ] A submission of either entry type whose quantity is absent, non-numeric, non-finite, or outside the published range or decimal precision is rejected with an actionable error; the range and precision are published as part of the public contract, and every storage plugin round-trips the full published range — negative half included — and the full precision without loss (cross-reference `cpt-cf-usage-collector-fr-record-quantity`)
- [ ] A negative **Usage Record** carries no reference to another **Usage Record** and no reason code, and reduces the aggregated `SUM` for its scope and range without withdrawing anything (cross-reference `cpt-cf-usage-collector-usecase-report-decrease`, [§5.6](#56-corrections))
- [ ] Incoming **Usage Records** include an explicit tenant attribution; the platform PDP validates that the authenticated caller is authorized to emit **Usage Records** for the specified tenant before the **Usage Record** is accepted, and the gateway independently validates tenant attribution on ingest as a defense-in-depth check
- [ ] Every **Usage Record** includes resource attribution (resource ID and resource type); **Usage Records** omitting either are rejected
- [ ] **Usage Records** can optionally include an explicit subject attribution (subject ID and subject type); when present, the platform PDP validates that the authenticated caller is authorized to emit **Usage Records** attributed to the specified subject before the **Usage Record** is accepted; when absent, PDP subject validation is skipped
- [ ] Authorization failures are surfaced immediately to the caller; no **Usage Record** is persisted on denial
- [ ] Tenant isolation is enforced via PDP: a caller never receives a tenant's usage data — for reads or writes — without an explicit PDP authorization for that tenant; same-tenant, parent→subtenant, and platform-administrative scopes are each authorized independently
- [ ] Aggregation queries require exactly one GTS type and a time range; requests omitting the GTS type or supplying more than one GTS type are rejected with an actionable error
- [ ] Aggregation queries return correct results for the specified GTS type and time range, with correct additional filtering by tenant (optional), subject, and resource when specified
- [ ] Aggregation results can be grouped by tenant, subject, and resource
- [ ] Raw **Usage Record** queries require exactly one GTS type and a mandatory time range, and optionally narrow by tenant, subject, and resource
- [ ] Query authorization is enforced via PDP decision and constraint enforcement; unauthorized queries are rejected and PDP-returned constraints narrow the result scope
- [ ] The gear works with any registered plugin (e.g., ClickHouse, TimescaleDB) without code changes to the core gear
- [ ] Metadata attached to a **Usage Record** is persisted as-is and returned in query results without modification
- [ ] **Usage Records** with metadata exceeding the configured size limit are rejected with an actionable error
- [ ] No surface exposes an operation that modifies an accepted **Usage Record**: there is no deactivation, retirement, reactivation, or amendment on the REST API, the SDK trait, or the Plugin SPI, and every correction is an appended invalidation entry submitted through the ordinary ingestion path (cross-reference [§5.6](#56-corrections), [§4.2](#42-out-of-scope))
- [ ] There is exactly one correction primitive, and it withdraws a whole **Usage Record**: no surface accepts a signed compensating entry, and no correction adjusts a quantity (cross-reference [§5.6](#56-corrections), [§4.2](#42-out-of-scope))
- [ ] An invalidation entry is identified by the explicit reference it carries to the record it withdraws, never by the value or sign of its quantity, which is a copy of the withdrawn quantity rather than a negation of it; a submission carrying a reference without a reason code, or a reason code without a reference, is rejected with an actionable error (cross-reference `cpt-cf-usage-collector-fr-record-invalidation`)
- [ ] The reference an invalidation entry carries is validated at ingestion: the referenced **Usage Record** MUST exist, MUST NOT itself be an invalidation entry, and MUST share the full attribution identity — tenant, GTS type, resource, and subject, where presence against absence of a subject is a mismatch; any failure rejects the invalidation with an actionable error (cross-reference `cpt-cf-usage-collector-fr-record-invalidation`)
- [ ] An accepted invalidation makes both the withdrawn **Usage Record** and the invalidation entry contribute nothing to any aggregation that selects them, and affects nothing else: both are returned as persisted by raw query and point lookup, both appear on the feed at their own acceptance positions and are never removed from it, and every invalidation names the record it withdraws on every one of those paths (cross-reference `cpt-cf-usage-collector-fr-record-invalidation`, `cpt-cf-usage-collector-fr-query-raw`, `cpt-cf-usage-collector-fr-billing-fields-on-read`)
- [ ] A **Usage Record** is withdrawn at most once: an invalidation entry referring to an already-invalidated **Usage Record** is rejected at ingestion with an actionable error, while an exact-equality resubmission under the same idempotency key is absorbed as a duplicate rather than treated as a second invalidation (cross-reference `cpt-cf-usage-collector-fr-record-invalidation`, `cpt-cf-usage-collector-fr-idempotency`)
- [ ] An invalidation entry is a faithful copy of the **Usage Record** it withdraws: tenant, GTS type, resource, subject, covered period (zero-length where the target is a point event), quantity, and metadata all equal the target's, and a submission differing in any of them is rejected with an actionable error naming the field; the only caller-supplied departures are its own idempotency key, the reference, and the reason code, and the gear assigns it an identifier, an acceptance instant, and an origin marker in its own right (cross-reference `cpt-cf-usage-collector-fr-record-invalidation`, `cpt-cf-usage-collector-fr-record-identity`, `cpt-cf-usage-collector-fr-record-quantity`, `cpt-cf-usage-collector-fr-record-metadata`, `cpt-cf-usage-collector-fr-usage-windows`)
- [ ] Because the copy carries the target's period and metadata, a range-scoped raw read that returns a withdrawn **Usage Record** returns the invalidation withdrawing it as well, and so does a read narrowed or grouped by any declared metadata property that selected the target (cross-reference `cpt-cf-usage-collector-fr-query-raw`, `cpt-cf-usage-collector-fr-record-metadata`)
- [ ] No aggregation selects a withdrawn **Usage Record** without also selecting its invalidation, because both carry one period end — so an aggregate over any range excludes the withdrawn quantity and no covered period the invalidation could have carried would produce a different figure; verifiable by aggregating ranges that hold, exclude, and abut the withdrawn period (cross-reference `cpt-cf-usage-collector-fr-query-aggregation`, `cpt-cf-usage-collector-fr-usage-windows`)
- [ ] An aggregate over a period entirely older than the configured backfill window cannot be changed by any invalidation, since no ingestion path admits an entry whose covered period ends that far back and v1 exposes no override reaching past that bound (cross-reference `cpt-cf-usage-collector-fr-query-aggregation`, `cpt-cf-usage-collector-fr-record-invalidation`, `cpt-cf-usage-collector-fr-backfill`)
- [ ] The covered period an invalidation copies is validated against the bounds of the path it arrived on, exactly as an ordinary **Usage Record**'s period is, with no wider reach conferred by the entry kind (cross-reference `cpt-cf-usage-collector-fr-record-invalidation`, `cpt-cf-usage-collector-fr-live-future-time-bound`, `cpt-cf-usage-collector-fr-backfill`)
- [ ] An invalidation whose target predates the live past tolerance is rejected on the live path with an actionable error naming the instant, the bound, and the backfill route, and is accepted on that route within its window carrying the backfill origin marker (cross-reference `cpt-cf-usage-collector-fr-record-invalidation`, `cpt-cf-usage-collector-fr-backfill`)
- [ ] Correcting a quantity is an invalidation followed by a fresh emission under a new idempotency key carrying the same attribution and the same covered period; a re-emission changing either is accepted as a distinct measurement rather than as a correction (cross-reference `cpt-cf-usage-collector-fr-record-invalidation`)
- [ ] Every invalidation ingestion call carries a mandatory idempotency key: an exact-equality re-submission is silently deduplicated and a same-key content mismatch is rejected with a duplicate-submission conflict signal (cross-reference `cpt-cf-usage-collector-fr-record-invalidation`, `cpt-cf-usage-collector-fr-idempotency`)
- [ ] The ledger stays append-only under invalidation — no persisted **Usage Record** is modified — while any pre-computed or materialised aggregate covering a withdrawn **Usage Record** is recomputed over the affected range rather than adjusted by a further contribution, since `MAX`, `MIN`, and `LATEST` admit no reversing term (cross-reference `cpt-cf-usage-collector-fr-record-invalidation`, `cpt-cf-usage-collector-nfr-aggregate-freshness`)
- [ ] **Usage Records** whose referenced GTS type does not resolve to a registered declaration are rejected immediately with an actionable error naming the unresolved identifier, before any **Usage Record** is accepted for delivery
- [ ] GTS types become available for ingestion across all tenants without Usage Collector code changes, redeployment, or any Usage Collector-side write; the gear exposes no GTS type operation of any kind — read or write — on any of its three surfaces, and holds no GTS type catalog of its own (cross-reference `cpt-cf-usage-collector-fr-usage-type-declaration`)
- [ ] A declaration carries exactly one aggregation fold from the closed set `{SUM, COUNT, MAX, MIN, LATEST}` as a type-level property — not inferred from the shape of the GTS type identifier — together with its canonical metering unit, metadata surface, retention policy, and an optional nominal sampling interval admissible under any fold; ingest rejects **Usage Records** carrying any metadata key the declaration does not declare, with an unknown metadata key signal
- [ ] Declaration attributes that give persisted **Usage Records** their meaning — aggregation fold, metering unit, metadata surface — are immutable; changing any of them requires a new GTS type rather than an edit in place (cross-reference `cpt-cf-usage-collector-fr-usage-type-declaration`)
- [ ] The aggregate query path serves the queried GTS type's declared fold and no other; a request carrying an aggregation function of its own is rejected with an actionable validation error rather than served (cross-reference `cpt-cf-usage-collector-fr-query-aggregation`, `cpt-cf-usage-collector-fr-aggregation-fold`)
- [ ] `LATEST` is determinate where two **Usage Records** of one meter share a covered period: it reports the one with the greatest covered-period end, ties broken by the greatest acceptance sequence, so repeated queries over the same range return the same quantity; verifiable by accepting two records with an identical period and differing quantities and confirming the later-accepted one is reported (cross-reference `cpt-cf-usage-collector-fr-aggregation-fold`)
- [ ] A `COUNT` meter's record carries a quantity that the fold does not read; ingestion applies the same acceptance rules whatever the value, since it never consults the declared fold, and the `SHOULD`-send-`1` convention is stated for emitters rather than enforced (cross-reference `cpt-cf-usage-collector-fr-aggregation-fold`, `cpt-cf-usage-collector-fr-record-quantity`)
- [ ] Resolution fails closed: a GTS type that does not resolve — from `types-registry`, from the local declaration cache, or by recovery — causes the **Usage Record** to be rejected and never persisted, with no permissive fallback that admits an unvalidated **Usage Record** to protect ingestion availability; the steady state is served from cache, and cached declarations remain usable while the registry is unreachable (cross-reference `cpt-cf-usage-collector-fr-usage-type-resolution`)
- [ ] Ingestion for a GTS type already in use survives a `types-registry` restart: the gear recovers a declaration the registry has lost and serves the next **Usage Record** against it. Recovery is best-effort — where it does not apply, the reference does not resolve and the **Usage Record** is rejected fail-closed rather than admitted unvalidated (cross-reference `cpt-cf-usage-collector-fr-usage-type-resolution`)
- [ ] The system maintains 99.95% monthly availability for ingestion endpoints
- [ ] The system sustains ingestion of at least 10,000 **Usage Records**/sec sample-mean under the `cpt-cf-usage-collector-nfr-throughput-profile` load envelope, with every 1-minute sample-mean ≥ 9,500 **Usage Records**/sec, measured over a ≥ 30-minute steady-state window
- [ ] **Usage Record** ingestion completes within 200ms at p95 under the `cpt-cf-usage-collector-nfr-throughput-profile` load envelope, with the ±10% tolerance defined in §9.0 (single-window p95 ≤ 220ms accepted only when the trailing 30-minute p95 remains ≤ 200ms)
- [ ] Aggregation queries over a 30-day range for a single tenant complete within 500ms at p95 under the `cpt-cf-usage-collector-nfr-throughput-profile` load envelope, with the ±10% tolerance defined in §9.0 (single-window p95 ≤ 550ms accepted only when the trailing 30-minute p95 remains ≤ 500ms)
- [ ] Ingestion p95 latency remains within the bound from `cpt-cf-usage-collector-nfr-ingestion-latency` (p95 ≤ 200ms with the §9.0 ±10% tolerance) while ≥ 100 concurrent aggregation queries are executing inside the `cpt-cf-usage-collector-nfr-throughput-profile` envelope
- [ ] **Usage Records** submitted by a `cpt-cf-usage-collector-actor-usage-source` are accepted only after PDP authorizes the authenticated caller (calling-gear identity from the platform-resolved security context) for the supplied tenant, resource, subject (if any), and referenced GTS type; unauthenticated or unauthorized submissions are rejected immediately with no partial persistence
- [ ] From the 1.0 release of each surface onward, the Plugin SPI, SDK trait, and REST API stay stable within a major version. A consumer compiled or wired against major version `N` **MUST** keep working unchanged across every `N.x` release, at most one prior major version stays supported per surface, and only additive changes ship within a major version (cross-reference `cpt-cf-usage-collector-nfr-plugin-contract-stability`)
- [ ] All authentication is performed by the ToolKit gateway upstream of the collector; the gear does not implement local credential validation, MFA, SSO/federation, session management, or credential issuance, does not consume any credential-resolution contract, and rejects every REST or SDK call that arrives without a platform-resolved security context
- [ ] Persisted gear data is limited to opaque platform identifiers, operational telemetry, and opaque caller-supplied metadata; the gear performs no decoding of identifiers to natural persons, and integrator-facing documentation states the prohibition on placing PII, payment, health, or credential data in metadata
- [ ] Every API operation contributes a correlation identifier that reconciles gear activity with platform gateway access logs and platform audit infrastructure; no gear-local audit log is maintained in v1
- [ ] Every accepted ingestion and query operation is attributable to an authenticated caller identity recorded in the platform audit trail; anonymous and synthesized identities are rejected. GTS type lifecycle operations are audited by `types-registry`, which performs them
- [ ] Privacy by Design principles are applied at PRD level (data minimization, purpose limitation, storage limitation delegated to plugin, privacy by default through PDP, pseudonymization via opaque identifiers) and documented for downstream review
- [ ] Data-ownership model is recorded: tenant administrator owns tenant usage data, platform operator stewards storage-plugin selection and the GTS type declarations held in `types-registry`, and the Usage Collector gear acts as custodian; third-party access flows exclusively through PDP-authorized public read surfaces
- [ ] Data-quality guarantees are verifiable: declaration-based validation of the metadata surface and unit binding, mandatory attribution, ingestion-acknowledgement latency bounded by `cpt-cf-usage-collector-nfr-ingestion-latency`, queryability governed separately by `cpt-cf-usage-collector-nfr-query-freshness` (plugin-bound; no read-your-writes assumption against the query surfaces; the acknowledgement is the surface for same-request outcome), gateway-level validation, and absence of in-gear amendment (corrections expressed as appended invalidation entries, and a corrected quantity as an invalidation plus a fresh emission)
- [ ] The query-freshness consistency contract is verifiable: the gear floor publishes ingestion-acknowledgement durability and dedup-identity visibility on the ingestion path, declares the raw and aggregated query surfaces eventually consistent with no upper bound at the gear floor, and obliges every active plugin's deployment guide to publish its actual consistency profile (`cpt-cf-usage-collector-nfr-query-freshness`); plugin-specific ceilings are verified against each plugin's published profile separately
- [ ] The dedup identity of an accepted **Usage Record** remains visible to subsequent ingestion attempts for as long as the referenced GTS type's retention policy retains the **Usage Record**, and no longer: the window is a per-meter and bounded floor, a deployment is not obliged to retain identities beyond the data they protect, and a retry arriving after the target has aged out draws no guaranteed outcome (cross-reference `cpt-cf-usage-collector-nfr-query-freshness`, `cpt-cf-usage-collector-fr-billing-retention-floor`)
- [ ] Aggregate freshness is verified as a plugin readiness gate, not a gear bound: the gear floor remains eventually consistent with no upper bound, and a deployment is fit to serve the aggregate path to a consumer that acts on the result only where the active plugin publishes a finite acceptance → aggregate-visibility ceiling, at p95 ≤ 5 minutes for such a consumer, together with a published bound on how an accepted invalidation reaches any materialised representation (cross-reference `cpt-cf-usage-collector-nfr-aggregate-freshness`, `cpt-cf-usage-collector-fr-record-invalidation`)
- [ ] Data-lifecycle delegation is documented: retention, archival, purging, migration, and historical access are governed by the active storage plugin's deployment profile and the platform governance layer; the gear's surface preserves historical query access within the plugin-provided retention window
- [ ] Standards, legal, and compliance applicability is declared at PRD level: alignment with the platform security baseline and OpenAPI 3 interoperability; PCI DSS, HIPAA, and SOX explicitly not applicable; consent management, data-subject-rights, terms-of-service, and privacy-policy duties delegated to the platform identity, legal, and governance layers; data residency delegated to platform topology and operator-selected plugin deployment profile
- [ ] Sustained ingestion of ≥ 10,000 **Usage Records**/sec and burst ingestion of ≥ 30,000 **Usage Records**/sec for ≤ 5 minutes per 60-minute window are sustainable without breaching ingestion p95 latency; ≥ 100 concurrent aggregation queries are sustainable without breaching query p95 latency or degrading ingestion p95; ≥ 700,000,000 accepted ingestion calls per 24-hour day are sustainable at the sustained rate
- [ ] Usage Collector domain metrics are integrated into shared platform dashboards and alert routing, with operator treatment for ingestion latency, ingestion error rate, query latency, PDP error rate, and storage-plugin readiness; every accepted and rejected API operation emits a structured log entry carrying the inbound correlation identifier unchanged

**Metering contract for charging.** The criteria below evaluate [§5.9](#59-billing-integration) and the metering NFRs in [§6.1](#61-gear-specific-nfrs). They apply to every GTS type.

- [ ] Every **Usage Record** carries exactly one emitter-supplied time attribution — the half-open period it covers, start inclusive and end exclusive, with the start no later than the end — and carries no other emitter-supplied time attribution; timestamps without offset information are rejected and non-UTC offsets are normalized to UTC (cross-reference `cpt-cf-usage-collector-fr-usage-windows`)
- [ ] A **Usage Record** is selected by a query when the end of its covered period falls in the requested range — `from <= window_end < to` — whatever the length of that period (cross-reference `cpt-cf-usage-collector-fr-usage-windows`)
- [ ] Boundary behaviour is verified by explicit tests: a point event sitting exactly on a range's lower bound — a **Usage Record** at midnight on the first of the month, queried for that month — is returned; a **Usage Record** whose period ends exactly on the range's upper bound is not returned by that range and is returned by the next one; and a **Usage Record** whose covered period is wider than the range is returned by neither (cross-reference `cpt-cf-usage-collector-fr-usage-windows`)
- [ ] The acceptance instant is assigned by the gear on every **Usage Record** and exposed on read, and cannot be set or overridden by the emitter; a submission attempting to supply it is rejected or has the supplied value ignored in favour of the gear-assigned instant (cross-reference `cpt-cf-usage-collector-fr-usage-windows`)
- [ ] The **Usage Record** identifier is server-derived, stable across an exact-equality retry, unique per tenant, GTS type, idempotency key, and covered period, reproducible offline by the emitter from those same attributes with no round-trip, and addressable by point lookup and by the reference an invalidation entry carries (cross-reference `cpt-cf-usage-collector-fr-record-identity`)
- [ ] Two **Usage Records** covering different periods are distinct even under a single stable per-meter idempotency key, and an offline-computed correction reference resolves to the intended **Usage Record** (cross-reference `cpt-cf-usage-collector-fr-record-identity`, `cpt-cf-usage-collector-fr-idempotency`)
- [ ] On the live ingestion path a **Usage Record** whose covered period ends further into the future than the configured future tolerance (default 5 minutes) is rejected with an actionable error naming the offending instant and the bound (cross-reference `cpt-cf-usage-collector-fr-live-future-time-bound`)
- [ ] On the live ingestion path **every** entry — an ordinary **Usage Record** and an invalidation entry alike — whose covered period **ends** further into the past than the configured past tolerance (default 48 hours) is rejected with an actionable error naming the offending instant, the bound, and the dedicated backfill route, while the backfill path still admits that period within its own window (cross-reference `cpt-cf-usage-collector-fr-live-future-time-bound`, `cpt-cf-usage-collector-fr-backfill`)
- [ ] Every GTS type declaration binds a canonical metering unit; ingestion rejects a **Usage Record** whose GTS type has no bound unit; the unit is resolved from the GTS type rather than carried per **Usage Record**; and an attempt to rebind the unit of an existing GTS type is rejected (cross-reference `cpt-cf-usage-collector-fr-metering-unit-binding`)
- [ ] The canonical unit list is published and includes at minimum `bytes`, `byte-hours`, `count`, and `seconds`; stored and emitted quantities are never converted, scaled, or rounded by the gear, and a quantity read back equals the quantity submitted (cross-reference `cpt-cf-usage-collector-fr-canonical-units`)
- [ ] Quantity semantics over a covered period are published normatively per declared fold: under `SUM` quantities accrue over the period and are additive across disjoint periods, under every other fold they are single observations that are not additive, no fold converts a series of observations into a period quantity, and the gear performs no integration, differentiation, interpolation, or re-windowing in either direction (cross-reference `cpt-cf-usage-collector-fr-quantity-semantics`, `cpt-cf-usage-collector-fr-aggregation-fold`)
- [ ] A meter whose consumption must be charged is declared `SUM`; a level-valued meter is pre-integrated at the emitter into an accrued unit rather than charged from `MAX`, `MIN`, or `LATEST`, and a declared nominal sampling interval is served by `types-registry` and never acted on by the gear (cross-reference `cpt-cf-usage-collector-fr-aggregation-fold`, `cpt-cf-usage-collector-fr-quantity-semantics`)
- [ ] A caller may group and equality-filter on any metadata property the queried GTS type's resolved declaration declares, in any combination and order, alongside the fixed **Usage Record** fields; a request naming a property outside that set is rejected with an actionable validation error **before** dispatch to the storage plugin, rather than silently yielding an empty or absent dimension (cross-reference `cpt-cf-usage-collector-fr-record-metadata`, `cpt-cf-usage-collector-fr-query-aggregation`)
- [ ] Every invalidation entry requires a non-empty reason code, submissions without one are rejected, and the code is returned on every read path exposing the correction; an ordinary **Usage Record** carries none, including one whose quantity is negative (cross-reference `cpt-cf-usage-collector-fr-invalidation-reason-code`, [§5.6](#56-corrections))
- [ ] Raw query, point lookup, and the usage feed return, unstripped, exactly the field set enumerated in `cpt-cf-usage-collector-fr-billing-fields-on-read` — that FR is the single normative enumeration — and point lookup by **Usage Record** identifier returns the exact persisted fact; the aggregate path is out of scope of that field set (cross-reference `cpt-cf-usage-collector-fr-billing-fields-on-read`)
- [ ] The feed is deterministic and replay-safe: **Usage Records** are ordered by a monotonic acceptance sequence scoped per (tenant, GTS type) with no cross-tenant or cross-GTS-type ordering claimed; a paginated scan over a consistent snapshot never observes **Usage Records** appearing, disappearing, or mutating mid-scan other than append-only arrivals demarcated by the returned watermark; corrections enter the sequence as appended invalidation entries at their own acceptance positions, with no feed entry representing a change to an already-delivered **Usage Record** and no accepted invalidation removing either the withdrawn **Usage Record** or the invalidation itself from the feed; and correction linkage is present so a reader can reconstruct correction history (cross-reference `cpt-cf-usage-collector-fr-billing-usage-feed`)
- [ ] A consumer can declare the set of GTS types it consumes, and the feed excludes every other GTS type from its pages, watermark, and cursor (cross-reference `cpt-cf-usage-collector-fr-billing-usage-feed`)
- [ ] A consumer resuming from a persisted cursor, and a consumer replaying from an earlier cursor within the retention floor, both reach the same processed set after deduplication by **Usage Record** identifier; a cursor older than the retention floor produces an actionable error rather than a silently truncated range (cross-reference `cpt-cf-usage-collector-fr-billing-usage-feed`, `cpt-cf-usage-collector-fr-billing-retention-floor`)
- [ ] A GTS type carries a retention policy honoured by the storage deployment, and for every GTS type that policy is at least the configured backfill window plus the operational replay horizon — 125 days at the launch defaults — so that every accepted **Usage Record**, however old the period it covers, retains at least one full replay horizon from the moment it first becomes readable; a deployment below the floor is a readiness failure; the gear itself enforces no retention and mandates no aggregate; and long-term dispute and audit evidence is a downstream obligation, keeping the [§6.2](#62-nfr-exclusions) financial-reporting-source exclusion intact (cross-reference `cpt-cf-usage-collector-fr-billing-retention-floor`, `cpt-cf-usage-collector-fr-backfill`)
- [ ] The dedicated backfill path applies validation identical to the live path but for the covered-period bounds it replaces, accepts **Usage Records** and invalidation entries alike, marks every persisted entry with the backfill origin on storage and on every read path, enforces a configurable bounded backfill window that a deployment cannot widen past the point where any GTS type's retention would fall below that window plus one operational replay horizon, rejects a covered period beyond that window with an actionable error naming the bound on REST and on the SDK trait alike, and runs without breaching live-path ingestion SLOs (cross-reference `cpt-cf-usage-collector-fr-backfill`)
- [ ] The backfill path is reachable on the REST API and on the in-process SDK trait, so an in-process emitter can import and withdraw over a period the live past tolerance rejects (cross-reference `cpt-cf-usage-collector-fr-backfill`, `cpt-cf-usage-collector-interface-sdk-client`)
- [ ] Configurable ingestion quotas are enforced per calling gear and per (calling gear, tenant) pair across SDK and REST; over-quota submissions are rejected with an actionable throttle error carrying retry guidance and are never silently dropped (cross-reference `cpt-cf-usage-collector-fr-rate-limiting`)
- [ ] Reconciliation metadata is exposed via API at (tenant, GTS type) granularity — accepted **Usage Record** counts for every GTS type irrespective of its declared fold, a fold-appropriate quantity summary (the accrued sum under `SUM`; observation count and latest observation otherwise), the latest acceptance-instant watermark, the latest covered period end, and the latest acceptance-sequence watermark — and is sufficient to compare gear-side totals against a consumer's processed totals for a time range without a full raw scan (cross-reference `cpt-cf-usage-collector-fr-reconciliation-metadata`)
- [ ] Reconciliation metadata is exposed at (calling gear) and (calling gear, tenant) granularity — deferred beyond v1, and blocked until the tenant plane carries a calling-gear identity (cross-reference `cpt-cf-usage-collector-fr-reconciliation-caller-scopes`)
- [ ] A silently stopped emitter is detectable by comparing a scope's acceptance-instant watermark against an expected cadence — the GTS type's declared nominal sampling interval where one is declared, the consumer's own expectation where none is — and the gear performs no part of that comparison: it exposes the watermarks on read, leaves the declared interval to `types-registry`, and emits no stalled-emitter signal (cross-reference `cpt-cf-usage-collector-fr-reconciliation-metadata`, `cpt-cf-usage-collector-fr-usage-type-declaration`, `cpt-cf-usage-collector-fr-quantity-semantics`)
- [ ] Feed freshness is verified as a plugin readiness gate, not a gear bound: the gear floor remains eventually consistent with no upper bound, and a deployment is fit to feed a charging consumer only where the active plugin's published consistency profile bounds acceptance → feed visibility at p95 ≤ 5 minutes under the `cpt-cf-usage-collector-nfr-throughput-profile` envelope (cross-reference `cpt-cf-usage-collector-nfr-billing-feed-freshness`, `cpt-cf-usage-collector-nfr-query-freshness`)
- [ ] The feed meets its recovery objective: a consumer 24 hours behind returns to the live watermark within 6 hours while **Usage Records** continue to arrive, with ingestion p95 latency remaining within `cpt-cf-usage-collector-nfr-ingestion-latency` throughout and the §9.0 tolerance applied; the test is run against the subscription under test and the observed read rate is at least five times the subscribed arrival rate (cross-reference `cpt-cf-usage-collector-nfr-replay-throughput`, `cpt-cf-usage-collector-nfr-workload-isolation`)

## 10. Dependencies

| Dependency     | Description                                                                            | Criticality |
| -------------- | -------------------------------------------------------------------------------------- | ----------- |
| authz-resolver | Platform PDP; authorizes every ingestion, query, and operator-write operation          | p1          |
| types-registry | Holds the GTS type declarations resolved and validated against on every ingestion ([§5.7](#57-usage-record-typing)); owns their registration and withdrawal | p1          |
| gts-registry   | Platform registry/orchestration dependency used for active storage extension selection | p1          |

## 11. Assumptions

| Assumption                                                                                                                                                                                                                                                                                                                                                                                                                              | Owner                                                                      | Validation                                                                                                                                                           |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| At least one plugin (e.g., a ClickHouse or TimescaleDB storage backend) is deployed alongside the gear                                                                                                                                                                                                                                                                                                                                  | Platform Infrastructure / Operator                                         | Verified at gear startup via platform storage-extension resolution; readiness fails if no active plugin resolves                                                     |
| `types-registry` supports the GTS type declaration lifecycle this PRD depends on — PDP-authorized registration and withdrawal of derived types, uniqueness of type identifiers over the lifetime of the deployment including no reuse of a withdrawn identifier for a different declaration, and resolution of a declaration by identifier. Its published PRD currently scopes it to read-only schema lookup for a different consumer, so this capability is a prerequisite the Usage Collector does not itself deliver                              | Types Registry gear owners / Platform Architecture                         | Verified by a `types-registry` PRD covering the declaration lifecycle, and by end-to-end declare → emit → withdraw tests against both gears before release candidate |
| `types-registry` resolution latency and availability are sufficient to keep GTS type declarations cacheable within the Usage Collector's ingestion NFRs; the registry publishes no latency obligation of its own, so the gear's steady state does not depend on one                                                                                                                                                                     | Types Registry gear owners / Usage Collector Maintainers                   | Verified by ingestion load tests with the declaration cache cold and with the registry unreachable                                                                   |
| Platform documentation and operations channels are available for publishing Usage Collector quickstarts, API references, and support runbooks before release candidate                                                                                                                                                                                                                                                                  | Usage Collector Maintainers / Platform Documentation / Platform Operations | Verified during release-readiness review                                                                                                                             |
| The gateway delivers an authenticated security context to the usage-collector gear on every call; the gear rejects any request that arrives without a platform-resolved security context                                                                                                                                                                                                                                                | Platform Identity / Platform Security                                      | Verified by gateway integration tests against the usage-collector gear                                                                                               |
| The platform will carry a calling-gear identity on the tenant plane, so the gear can attribute an accepted entry to the gear that sent it. Today that name lives on the platform plane only, which never carries a tenant | Platform Identity / Platform Security | Verified when the tenant security context carries a calling-gear identity; `cpt-cf-usage-collector-fr-reconciliation-caller-scopes` stays deferred until then |
| Platform gateway access logs and platform audit infrastructure are available to record authentication, authorization, ingestion, query, and operator-write outcomes and accept correlation identifiers emitted by the Usage Collector                                                                                                                                                                                                   | Platform Operations / Platform Audit Owner                                 | Verified by end-to-end correlation between gear logs and platform audit records before release candidate                                                             |
| Operator-selected storage plugin deployment topology meets the deployment's data residency, sovereignty, retention, and disaster-recovery obligations for tenant usage data                                                                                                                                                                                                                                                             | Platform Operator / Plugin Authors                                         | Verified during operator onboarding and at storage-plugin readiness review                                                                                           |
| Initial release establishes the launch capacity baseline (10,000 **Usage Records**/sec sustained, 30,000 **Usage Records**/sec burst, 100 concurrent aggregation queries, 10,000 tenants, 10,000 registered GTS types); no prior historical workload data exists at launch                                                                                                                                                              | Usage Collector Maintainers / Platform Operations                          | Validated by launch load tests against representative plugin backends                                                                                                |
| Platform monitoring and log infrastructure are available to host the observable signals expected by the operational visibility NFR                                                                                                                                                                                                                                                                                                      | Platform Operations                                                        | Verified during operations readiness review before production release candidate                                                                                      |
| The §9.0 load and measurement definitions (load envelope anchored on `cpt-cf-usage-collector-nfr-throughput-profile`, ≥ 30-minute steady-state measurement window, ±10% latency tolerance) are the single source of truth for every numeric acceptance criterion in [§9](#9-acceptance-criteria) and supersede the prior informal terms "normal load" and "normal operation" wherever they appeared in earlier PRD revisions            | Usage Collector Maintainers / Platform Operations                          | Verified during load-test plan review and release-readiness review                                                                                                   |
| Downstream charging consumers persist, per rated charge, the usage identity they rated (the set of **Usage Record** identifiers, or the aggregated source detail with its covered period and metadata breakdown), so invoice-dispute and audit evidence is reconstructable downstream and does not require raw retention inside the Usage Collector beyond the operational floor in `cpt-cf-usage-collector-fr-billing-retention-floor` | BSS Rating owner / Usage Collector Maintainers                             | Verified against the Rating gear's charge-detail retention requirement before the release candidate                                                                  |
| A charging consumer deduplicates on **Usage Record** identifier across feed delivery, which is at-least-once: an overlapping replay, a resumed cursor, and a redelivered page each present an identifier the consumer may already have rated (`cpt-cf-usage-collector-fr-billing-usage-feed`). Ingestion contributes no such case, since the backfill window bound keeps every admissible submission inside its idempotency horizon (`cpt-cf-usage-collector-fr-idempotency`), so the consumer's dedup state need span only its own replay reach | BSS Rating owner / Usage Collector Maintainers | Verified against the Rating gear's deduplication design, and exercised by an overlapping-replay test before the release candidate |
| A charging consumer's feed subscription covers a subset of the gear-wide ingestion envelope, assumed at ≤ 10,000,000 **Usage Records**/hour/region at launch; the bulk read rate required by `cpt-cf-usage-collector-nfr-replay-throughput` scales directly with this figure                                                                                                                                                            | Usage Collector Maintainers / BSS Rating owner                             | Revalidated as billing meters are onboarded, and at load-test plan review before the release candidate                                                               |
| Emitters derive the bounds of a covered period deterministically, so that a retry of the same measurement reproduces the same period and therefore the same **Usage Record** identifier. The gear does not quantize periods before they enter that identifier (`cpt-cf-usage-collector-fr-record-identity`), so an emitter that recomputes its period on retry both defeats deduplication and loses the ability to address its own **Usage Record** for invalidation | Usage Collector Maintainers / integrating gear owners                      | Stated in the integration guide and exercised by a retry test in each calling gear's integration suite before its onboarding is accepted                              |

## 12. Risks

| Risk                                                                                                                                                                                                                                                                                                                           | Impact                                                                                                                                 | Mitigation                                                                                                                                                                                                                                                                                                                                                  |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| High-cardinality aggregation exceeds 500ms p95 query latency                                                                                                                                                                                                                                                                   | Slow dashboard/billing queries                                                                                                         | See DESIGN.md for storage-extension acceleration and workload-isolation strategy                                                                                                                                                                                                                                                                            |
| v1 lacks gear-emitted audit events for operator-write paths (GTS type registration, GTS type deletion, operator-submitted invalidation entries); reliance is on platform gateway access logs and platform audit infrastructure with gear-emitted correlation identifiers until the deferred audit-emission capability is delivered | Reduced gear-local forensic detail for operator writes; downstream compliance reporting depends on platform-level audit completeness | Document the deferral, surface correlation identifiers, and track the deferred audit-emission capability against the [§4.2](#42-out-of-scope) Audit Events item for a future phase                                                                                                                                                                          |
| Data residency or sovereignty obligations could be violated if the operator-selected storage plugin is deployed outside the permitted region or topology                                                                                                                                                                       | Compliance and contractual breach for tenants subject to residency commitments                                                         | Operator onboarding documents the residency expectations; plugin deployment profile reviewed at readiness; cross-reference [§4.2](#42-out-of-scope) deferred Multi-Region Replication                                                                                                                                                                       |
| PDP authorization is performed per attribution tuple on the ingestion path with no cached-decision path (`cpt-cf-usage-collector-contract-authz-resolver`), so a batch of heterogeneous tenants yields one decision per **Usage Record**                                                                                        | The PDP round-trip, rather than storage, may be the binding constraint on `cpt-cf-usage-collector-nfr-throughput`                       | Measure PDP cost on the ingestion path before optimising storage; if confirmed as the ceiling, relaxing it is a change to this PRD's PDP contract rather than a DESIGN decision, and would be recorded as such                                                                                                                                                |
| An accepted invalidation obliges any materialised aggregate to recompute rather than absorb an appended correction, and storage engines differ in whether they can observe the obligation at all                                                                                                                                | A plugin serving aggregates from a materialised representation could return stale `MAX`, `MIN`, or `LATEST` values after a withdrawal   | `cpt-cf-usage-collector-fr-record-invalidation` states the obligation normatively; `cpt-cf-usage-collector-nfr-aggregate-freshness` requires each plugin to publish its invalidation-propagation bound, verified at storage-plugin readiness review                                                                                                          |
| Moving the catalog of usage GTS types to `types-registry` places a second gear on the ingestion path, and that gear publishes no latency or availability obligation                                                                                                                                                                     | A registry outage or latency regression could degrade ingestion for existing meters, not merely block new ones                        | The declaration cache is a requirement rather than an optimization (`cpt-cf-usage-collector-fr-usage-type-resolution`), cached declarations stay usable while the registry is unreachable, and declaration immutability makes indefinite cache validity safe; verified by the cold-cache and registry-unreachable load tests in [§11](#11-assumptions) |
| A GTS type still referenced by persisted **Usage Records** is removed from `types-registry`                                                                                                                                                                                                                                    | Every operation over the affected **Usage Records** that depends on resolving the declaration is rejected; the usage stays persisted but the gear can no longer interpret it | The attributes that give a **Usage Record** its meaning exist only in the declaration, so the gear fails closed rather than substituting a default (`cpt-cf-usage-collector-fr-usage-type-resolution`); removal is a `types-registry` decision this gear does not constrain, and rated history stays reconstructable downstream under the charge-detail retention assumption in [§11](#11-assumptions) |

## 13. Open Questions

- **Namespace ownership across gears.** Quota Enforcement resolves the same meters under its own provisional `gts.cf.qe.metric.type.v1~` base, pending platform-wide alignment. Whether Quota Enforcement metrics and the types Usage Collector meters against are one family or two with a mapping between them is a platform-level decision this PRD does not settle.
- **Metadata value typing.** [§5.1](#51-usage-ingestion) keeps all metadata values string-typed on the wire and at rest even though a declaration's schema can express richer types. Widening this is deferred; the trigger for revisiting it is a consumer that needs typed grouping or filtering.
- **A cumulative fold.** The fold set admits accrued amounts and single observations, and nothing else ([§5.2](#52-aggregation-fold)). An emitter able to report only a running total — a meter reading rather than a delta or a level — fits neither, and today the gear requires it to difference its own readings before submission. Adding a cumulative fold would mean relaxing the prohibition on differentiating (`cpt-cf-usage-collector-fr-quantity-semantics`) and taking on reset detection, so the exclusion is deliberate and is a divergence from the OpenTelemetry data model that the gear should keep stating as such. The trigger for revisiting it is an emitter that genuinely cannot difference.
- **Bulk and range invalidation.** v1 withdraws one **Usage Record** per invalidation entry ([§4.2](#42-out-of-scope)). The case this under-serves is the one that matters most in practice — an emitter defect producing a large number of wrong **Usage Records** — and a predicate-shaped bulk operation interacts with ingestion quotas, with the scope of the recomputation it forces, and with the feed contract, since a consumer cannot deduplicate a predicate by **Usage Record** identifier and the withdrawal would therefore still have to reach the feed as one entry per record.
- **Import and correction beyond the backfill window.** The configured window is a hard bound in v1 ([§4.2](#42-out-of-scope)), so usage older than it can be neither imported nor withdrawn. Earlier revisions of this document reserved an elevated-authorization override reaching past it; that reservation is withdrawn. It was never given a permission, an error, or a bound of its own, and it was the only construct able to place a submission outside its own idempotency horizon — which is a contract question rather than an authorization one, and is not answered by granting a role. Reintroducing it means designing all three and settling whether re-importing already-charged history is legitimate at all. The trigger for revisiting it is a migration or a dispute that genuinely needs to place usage older than the window, and cannot be served by widening the window ahead of the import.
- **Downstream documents still describe the superseded model.** DECOMPOSITION, the feature specifications, and the TimescaleDB plugin documents still describe counter/gauge semantics, a caller-chosen aggregation operation, and signed compensation. Updating them is outstanding.

## 14. Traceability

**Design**: [DESIGN.md](./DESIGN.md)

**ADRs**: see the Key ADRs inventory in DESIGN §1.2
