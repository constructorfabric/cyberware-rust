<!-- Created: 2026-08-26 by Constructor Tech -->

# Feature: Quota Lifecycle & Metadata

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-featstatus-quota-lifecycle-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-quota-enforcement-feature-quota-lifecycle`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Quota Create](#quota-create)
  - [Quota Update](#quota-update)
  - [Quota Deactivation Cascade](#quota-deactivation-cascade)
  - [Quota Read and List](#quota-read-and-list)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Quota Draft Validation](#quota-draft-validation)
  - [Metric Identity Validation](#metric-identity-validation)
  - [Quota Metadata Validation](#quota-metadata-validation)
  - [Validity-Window Computation](#validity-window-computation)
- [4. States (CDSL)](#4-states-cdsl)
  - [Quota State Machine](#quota-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Quota Management Service and CRUD Surface](#quota-management-service-and-crud-surface)
  - [Metric Identity Validation](#metric-identity-validation-1)
  - [Quota Metadata Contract Enforcement](#quota-metadata-contract-enforcement)
  - [Deactivation Cascade](#deactivation-cascade)
  - [Rate Quota-Type Rejection](#rate-quota-type-rejection)
  - [Lifecycle Telemetry Gauges](#lifecycle-telemetry-gauges)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Implements the Quota entity and its declarative lifecycle: create, update, deactivate, and read, through
`QuotaManagementService` and the transactional storage primitives. Covers metric validation against `types-registry`,
cap and validity-window semantics, opaque size-capped Quota Metadata, the closed `enforcement_mode` and `source`
enums, `rate` quota-type rejection, and the deactivation cascade that resolves active leases atomically.

### 1.2 Purpose

Every enforcement operation evaluates against Quota records; this feature is the write path that puts those records in
place and keeps them consistent. It turns misconfiguration into actionable create-time or update-time errors: an
unknown metric, a negative cap, thresholds on an unbounded cap, or metadata that violates the owner's constraint
contract never reaches storage. It also guarantees that deactivation never strands held lease capacity: the cascade
resolves every active lease in the same transaction.

**Scope**: `QuotaManagementService` with transactional CRUD via the storage plugin; metric existence, kind, and mode
validation through `TypesRegistryClient` with an in-process LRU cache, fail-closed; cap semantics
(`CAP_MUST_BE_NON_NEGATIVE`, `cap = 0`, `cap = null`, commit-time `CAP_BELOW_CONSUMED`); Quota Metadata validation
against the owner's constraint contract at write time only; validity-window storage and
`currently_within_window` computation; the deactivation cascade marking leases resolved-by-deactivation; `rate`
quota-type rejection with canonical `Unimplemented`.

**Out of scope**: counter mutation of any kind (the consumption-operations and lease-operations features), bulk Quota
CRUD (the bulk-quota-crud feature, P2), notification dispatch (the notifications feature; this feature only enqueues
events in the same transaction as its state mutation, invariant I11), the catalogue-membership check itself (defined
by the projection-contracts feature and invoked here), and any breaking projection-version activation procedure (out
of P1 per PRD §4.2).

**Requirements**: `cpt-cf-quota-enforcement-fr-quota-lifecycle`, `cpt-cf-quota-enforcement-fr-quota-metadata`,
`cpt-cf-quota-enforcement-fr-metric-identity-validation`, `cpt-cf-quota-enforcement-fr-enforcement-mode`,
`cpt-cf-quota-enforcement-fr-quota-type-rate-rejection`

**Principles**: none of its own; the feature applies the foundation and projection-contracts principles
(per DECOMPOSITION §2.3).

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-quota-enforcement-actor-platform-operator` | Creates, updates, and deactivates Quotas directly; owns cap and metadata choices |
| `cpt-cf-quota-enforcement-actor-quota-manager` | Drives Quota CRUD on behalf of tenant administrators or the licensing layer, within the original caller's tenant scope |
| `cpt-cf-quota-enforcement-actor-types-registry` | Authoritative source of metric identity, registry-reported classifications, and the owner's constraint contract |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) (§5.2 Quota lifecycle and metadata, §5.3 quota types, §5.11 enforcement mode)
- **Design**: [DESIGN.md](../DESIGN.md) (`QuotaManagementService`, REST and SDK interfaces, storage-plugin Quota CRUD
  group, error model, telemetry surface)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) (§2.3)
- **ADR**: [ADR-0007 Declarative GTS projection contracts](../ADR/0007-cpt-cf-quota-enforcement-adr-projection-contracts.md)
  (`cpt-cf-quota-enforcement-adr-projection-contracts`, status: accepted; the constraint contract shape and
  the write-time-only validation rule follow that ADR)
- **Dependencies**: `cpt-cf-quota-enforcement-feature-foundation` (storage plugin, Gateway admission, telemetry
  conventions), `cpt-cf-quota-enforcement-feature-projection-contracts` (catalogue-membership check, constraint
  contract resolution)

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-quota-enforcement-usecase-create-quota`

### Quota Create

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-quota-create`

Realises `cpt-cf-quota-enforcement-seq-quota-create`.

**Actor**: `cpt-cf-quota-enforcement-actor-quota-manager` (and
`cpt-cf-quota-enforcement-actor-platform-operator` through the same endpoint)

**Success Scenarios**:
- A valid draft is persisted with a server-assigned quota ID, its counter row materialized, and a
  `quota-changed (created)` event enqueued in the same transaction
- A draft on a `Direct`-classified metric is accepted; the Quota is inert until the metric flips to `QuotaGated`

**Error Scenarios**:
- Unknown metric: `METRIC_NOT_REGISTERED` (`DomainError::MetricNotRegistered`)
- `types-registry` unreachable: actionable error, fail-closed; nothing is persisted
- Projection registered but outside the configured catalogue: `PROJECTION_NOT_RESOLVABLE`
- Negative cap: `CAP_MUST_BE_NON_NEGATIVE`; thresholds on `cap = null`: `THRESHOLDS_REQUIRE_BOUNDED_CAP`
- `type = rate`: canonical `Unimplemented` (`NOT_YET_IMPLEMENTED`)
- Metadata over the size limit or violating the owner's contract: rejected before persistence

**Steps**:
1. [ ] - `p1` - Caller sends `POST /v1/quota-enforcement/quotas` with a `QuotaDraft` naming the explicit target
   `(projection_type, subject_id)` under PDP scope (management DTOs retain explicit target identity); foundation
   admission (`cpt-cf-quota-enforcement-flow-authorized-admission`) has attached `SecurityContext` and `AccessScope` - `inst-qcr-request`
2. [ ] - `p1` - Run `cpt-cf-quota-enforcement-algo-quota-draft-validation` on the draft - `inst-qcr-validate`
3. [ ] - `p1` - Run `cpt-cf-quota-enforcement-algo-metric-validation`; the same lookup resolves the metric owner's
   subject projection, metric request contract, and its attached constraint contract through the bounded LRU - `inst-qcr-metric`
4. [ ] - `p1` - Invoke `cpt-cf-quota-enforcement-algo-catalog-membership` (projection-contracts feature): the
   referenced projection must be registered, concrete, derived from the QE subject base, inside the configured
   catalogue (`PROJECTION_NOT_RESOLVABLE` otherwise), and must admit the draft's metric - `inst-qcr-membership`
5. [ ] - `p1` - Validate the draft's explicit `subject_id` against the declared scope discriminator of the resolved
   contract (`gts.cf.core.qe.scope.v1~`, P1 well-known instances `user` and `tenant`, per ADR-0007); a
   `subject_id` that violates the declared scope is rejected before persistence
   (`cpt-cf-quota-enforcement-fr-quota-lifecycle`) - `inst-qcr-subject-scope`
6. [ ] - `p1` - Run `cpt-cf-quota-enforcement-algo-quota-metadata-validation` when the draft carries `metadata`;
   snapshot the accepted contract id/version - `inst-qcr-metadata`
7. [ ] - `p1` - All validation above completes outside the storage transaction, so the database lock is held for the
   minimum window - `inst-qcr-outside-tx`
8. [ ] - `p1` - DB: `create_quota` in a single transaction: insert the `quotas` row (server-assigned UUIDv7
   `quota_id`, status `active`), insert the `quota_allocation_counters` row for allocation type (consumption counter
   rows are created lazily on first evaluate), enqueue `quota-changed (change_kind='created')` in the outbox (I11),
   append the `operation_log` entry, and commit the transaction - `inst-qcr-persist`
9. [ ] - `p1` - **IF** the registry-reported metric mode is `Direct` - `inst-qcr-direct-if`
   1. [ ] - `p1` - Accept the Quota anyway (a metric's mode can flip over time, PRD §3.2); it is inert until the flip
      and is surfaced through the `quota_for_direct_metric_total` gauge - `inst-qcr-direct`
10. [ ] - `p1` - **RETURN** `201` with the Quota body; the SDK path returns `QuotaId` from
    `QuotaManagerClientV1::create_quota` - `inst-qcr-return`

### Quota Update

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-quota-update`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-manager` (and
`cpt-cf-quota-enforcement-actor-platform-operator` through the same endpoint)

**Success Scenarios**:
- A non-breaking patch (cap, thresholds, validity window, metadata, failure-mode hint) is applied; the quota ID and
  subject reference are preserved and a `quota-changed (updated)` event is enqueued same-tx
- A cap raise, including numeric to `null`, is applied without the consumed guard

**Error Scenarios**:
- `type = rate` in the patch: canonical `Unimplemented` (HTTP 501, `NOT_YET_IMPLEMENTED`), checked before the
  breaking-change gate
- Patch touches metric, type, period, or subject: rejected; the caller must deactivate and create a new Quota
- Cap reduction below the current consumed or in-flight amount at commit time: `CAP_BELOW_CONSUMED`
- Thresholds added while cap is or becomes `null`: `THRESHOLDS_REQUIRE_BOUNDED_CAP`
- The Quota is already deactivated: `QUOTA_DEACTIVATED` (`DomainError::QuotaDeactivated`)

**Steps**:
1. [ ] - `p1` - Caller sends `PATCH /v1/quota-enforcement/quotas/{id}` with a `QuotaPatch` - `inst-qup-request`
2. [ ] - `p1` - **IF** the patch references `type = rate` - `inst-qup-rate-if`
   1. [ ] - `p1` - **RETURN** `DomainError::NotYetImplemented`, canonicalized as `Unimplemented` (HTTP 501,
      `NOT_YET_IMPLEMENTED`); the rate check runs before the breaking-change gate, so an update referencing `rate`
      returns `Unimplemented` per the DESIGN rejection rule
      (`cpt-cf-quota-enforcement-fr-quota-type-rate-rejection`) - `inst-qup-rate`
3. [ ] - `p1` - **IF** the patch changes metric, type, period, or subject - `inst-qup-breaking-if`
   1. [ ] - `p1` - **RETURN** rejection; breaking changes are performed by deactivating the original Quota and
      creating a new one (`cpt-cf-quota-enforcement-fr-quota-lifecycle`) - `inst-qup-breaking`
4. [ ] - `p1` - Run `cpt-cf-quota-enforcement-algo-quota-draft-validation` on the patched shape (cap non-negative,
   thresholds-require-bounded-cap) and `cpt-cf-quota-enforcement-algo-metric-validation` (metric identity is
   revalidated at update time per `cpt-cf-quota-enforcement-fr-metric-identity-validation`) - `inst-qup-validate`
5. [ ] - `p1` - **IF** the patch carries `metadata` - `inst-qup-meta-if`
   1. [ ] - `p1` - Run `cpt-cf-quota-enforcement-algo-quota-metadata-validation`; metadata changes never invalidate
      the quota ID - `inst-qup-meta`
6. [ ] - `p1` - DB: `update_quota(quota_id, patch, events)` in a single transaction that also appends the
   `operation_log` entry (I1); the cap-vs-consumed comparison is evaluated at the moment the transaction commits,
   in-tx with a row-level lock (I6), never at request-receipt time, so concurrent debits cannot race the guard - `inst-qup-persist`
7. [ ] - `p1` - **IF** the reduced or newly numeric cap is strictly below the active period's consumed amount
   (consumption type) or the in-flight count (allocation type) - `inst-qup-guard-if`
   1. [ ] - `p1` - **RETURN** `CAP_BELOW_CONSUMED` (`DomainError::CapBelowConsumed`); the operator first issues
      credits, then reduces the cap - `inst-qup-guard`
8. [ ] - `p1` - Enqueue `quota-changed (change_kind='updated')` in the same transaction (I11) - `inst-qup-event`
9. [ ] - `p1` - **RETURN** success; the SDK path is `QuotaManagerClientV1::update_quota(id, patch)` returning `()` - `inst-qup-return`

### Quota Deactivation Cascade

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-quota-deactivate`

Realises `cpt-cf-quota-enforcement-seq-quota-deactivate-cascade`.

**Actor**: `cpt-cf-quota-enforcement-actor-platform-operator` (and
`cpt-cf-quota-enforcement-actor-quota-manager` through the same endpoint)

**Success Scenarios**:
- The Quota is marked deactivated and every active lease against it is resolved-by-deactivation in one transaction;
  the caller receives the resolved-lease summary

**Error Scenarios**:
- Unknown quota ID: canonical `NotFound`
- The Quota is already deactivated: `QUOTA_DEACTIVATED`; no second cascade runs

**Steps**:
1. [ ] - `p1` - Caller sends `POST /v1/quota-enforcement/quotas/{id}/deactivate` - `inst-qde-request`
2. [ ] - `p1` - DB: `deactivate_quota(quota_id, events)` in a single transaction: lock the `quotas` row while its
   status is still `active`, mark the Quota deactivated, lock the active leases on this quota, mark each such lease
   resolved-by-deactivation, decrement `lease_capacity_counters`, return held capacity to the acquisition-period
   counters, and append the `operation_log` entry (I1) - `inst-qde-cascade`
3. [ ] - `p1` - Enqueue `quota-changed (change_kind='deactivated')` plus one `lease-resolved-by-deactivation` event
   per affected lease, carrying the lease ID, owning subject context, held amount, and the deactivated `quota_id`,
   all in the same transaction (I11); commit the transaction - `inst-qde-events`
4. [ ] - `p1` - The cascade never partially completes: either every active lease for the Quota is resolved or none
   is; subsequent `commit` or `release` calls against a resolved lease return `LEASE_NOT_ACTIVE` (lease operations
   are owned by the lease-operations feature); the deactivation timestamp serves as the implicit lease-resolve event - `inst-qde-atomic`
5. [ ] - `p1` - **RETURN** `200` with the `DeactivateOutcome { resolved_leases }` summary so the gateway can
   attribute telemetry - `inst-qde-return`

### Quota Read and List

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-quota-read`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-manager` (and
`cpt-cf-quota-enforcement-actor-platform-operator` through the same endpoints)

**Success Scenarios**:
- A single Quota or a filtered, paginated page is returned within the caller's PDP scope, with the full metadata
  object, the `validity_window`, and the server-computed `currently_within_window`

**Error Scenarios**:
- Unknown quota ID: canonical `NotFound` with `kind: "quota"`
- Rows outside the caller's tenant or `AccessScope` are unreachable by construction; they are absent, not errors

**Steps**:
1. [ ] - `p1` - Caller sends `GET /v1/quota-enforcement/quotas/{id}` or `GET /v1/quota-enforcement/quotas` with
   filter and page parameters - `inst-qrd-request`
2. [ ] - `p1` - DB: `read_quotas(filter, page)` under the caller's `AccessScope` per
   `cpt-cf-quota-enforcement-algo-pdp-constraint-composition` (foundation); reads are read-only (I3) - `inst-qrd-read`
3. [ ] - `p1` - Deactivated Quotas remain readable; deactivation retains the record for read access - `inst-qrd-deactivated`
4. [ ] - `p1` - Compute `currently_within_window` per `cpt-cf-quota-enforcement-algo-validity-window` for each
   returned Quota - `inst-qrd-window`
5. [ ] - `p1` - **RETURN** the Quota body or `PageResult<Quota>` including the full `metadata` object (subject to PDP
   scoping) so callers can inspect the operator's gating intent - `inst-qrd-return`

## 3. Processes / Business Logic (CDSL)

### Quota Draft Validation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-quota-draft-validation`

**Input**: `QuotaDraft` (create) or the patched Quota shape (update)

**Output**: a validated draft, or a canonical error before any storage call

**Steps**:
1. [ ] - `p1` - Require `quota_type` to be a GTS instance under `gts.cf.qe.quota.type.v1~` - `inst-qdv-type`
2. [ ] - `p1` - **IF** `quota_type` is the reserved `rate` instance (`gts.cf.qe.quota.type.v1~cf.qe.quota.rate.v1`) - `inst-qdv-rate-if`
   1. [ ] - `p1` - **RETURN** `DomainError::NotYetImplemented`, canonicalized as `Unimplemented` (HTTP 501,
      `NOT_YET_IMPLEMENTED`); the identifier and data-model slot stay reserved so P3 activation needs no migration of
      existing `allocation`/`consumption` Quotas (`cpt-cf-quota-enforcement-fr-quota-type-rate-rejection`) - `inst-qdv-rate`
3. [ ] - `p1` - **IF** `quota_type` is `allocation` and any period field is present - `inst-qdv-period-if`
   1. [ ] - `p1` - **RETURN** rejection; allocation Quotas must reject any period field, while consumption Quotas
      carry the period specification (period semantics themselves are owned by
      `cpt-cf-quota-enforcement-fr-period-semantics` through the consumption-operations feature) - `inst-qdv-period`
4. [ ] - `p1` - Require `enforcement_mode` to be a GTS instance under `gts.cf.qe.enforcement.type.v1~`; P1 accepts
   only `gts.cf.qe.enforcement.type.v1~cf.qe.enforcement.hard.v1`; future modes arrive as new GTS instances without
   API breakage (`cpt-cf-quota-enforcement-fr-enforcement-mode`) - `inst-qdv-mode`
5. [ ] - `p1` - Require `source` to be a GTS instance under `gts.cf.qe.source.type.v1~`; P1 seeds `licensing`
   (default) and `operator`; mutation rules are uniform across both values in P1, and a stored `source` never changes
   silently - `inst-qdv-source`
6. [ ] - `p1` - **IF** `cap` is numeric and negative - `inst-qdv-cap-if`
   1. [ ] - `p1` - **RETURN** `CAP_MUST_BE_NON_NEGATIVE` (`DomainError::CapMustBeNonNegative`, canonical
      `InvalidArgument`); `cap = 0` (deny-everything) and `cap = null` (unbounded, always satisfiable) are both
      explicitly valid and are never auto-rejected - `inst-qdv-cap`
7. [ ] - `p1` - **IF** `notification_thresholds` are present and `cap` is `null` - `inst-qdv-thresh-if`
   1. [ ] - `p1` - **RETURN** `THRESHOLDS_REQUIRE_BOUNDED_CAP` (`DomainError::ThresholdsRequireBoundedCap`);
      percentages of `null` are meaningless - `inst-qdv-thresh`
8. [ ] - `p1` - Accept the optional validity window (start and end timestamps; absent means no time bounds) and the
   optional failure-mode hint (`fail-closed` default, `fail-open` opt-in; informational metadata for callers) - `inst-qdv-optional`
9. [ ] - `p1` - Never reject on the basis that another active Quota exists for the same `(subject, metric)` pair;
   multiple Quotas per pair are resolved at evaluation time under the active Policy
   (`cpt-cf-quota-enforcement-fr-multi-quota-evaluation`, owned by the resolution-policy-engine feature) - `inst-qdv-multi`
10. [ ] - `p1` - **RETURN** the validated draft - `inst-qdv-return`

### Metric Identity Validation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-metric-validation`

**Input**: the draft's `metric_name`, `TypesRegistryClient` (platform `types-registry-sdk`, obtained from ClientHub)

**Output**: confirmed metric identity with the registry-reported classifications, or an actionable error

**Steps**:
1. [ ] - `p1` - Consult the in-process LRU cache of metric-name lookups inside `QuotaManagementService`; on a miss,
   call `TypesRegistryClient`; this lookup runs outside the storage transaction - `inst-qmv-lookup`
2. [ ] - `p1` - **IF** the metric is not registered in `types-registry` - `inst-qmv-unknown-if`
   1. [ ] - `p1` - **RETURN** `METRIC_NOT_REGISTERED` (`DomainError::MetricNotRegistered`, HTTP 400), an actionable
      creation-time error (`cpt-cf-quota-enforcement-fr-metric-identity-validation`) - `inst-qmv-unknown`
3. [ ] - `p1` - **IF** the registry is unreachable and the cache cannot answer - `inst-qmv-unreach-if`
   1. [ ] - `p1` - **RETURN** an actionable error; Quota create/update fails closed rather than accepting an
      unverifiable metric reference - `inst-qmv-unreach`
4. [ ] - `p1` - Record the registry-reported classifications (kind `counter`/`gauge`, enforcement-mode classification
   `QuotaGated`/`Direct`) for downstream consumers; `Direct` is not a create-time rejection criterion, and
   admission-time rejection of operations against `Direct`-metric Quotas belongs to the evaluation paths of the
   consumption-operations feature - `inst-qmv-classify`
5. [ ] - `p1` - A persisted Quota whose metric is later removed from `types-registry` is flagged via operational
   telemetry but never auto-deactivated (see the catalogue-gap note in section 7) - `inst-qmv-removal`
6. [ ] - `p1` - **RETURN** the confirmed metric identity plus the resolved owner projection, request contract, and constraint
   contract references from the same bounded LRU - `inst-qmv-return`

### Quota Metadata Validation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-quota-metadata-validation`

**Input**: the draft or patch `metadata` JSON object, the owner's constraint contract resolved from
`types-registry`

**Output**: validated metadata with the snapshotted contract id/version, or rejection before persistence

**Steps**:
1. [ ] - `p1` - **IF** the canonical JSON serialization exceeds the single operator-configurable size limit
   (default 4 KB per Quota) - `inst-qmd-size-if`
   1. [ ] - `p1` - **RETURN** rejection at validation time (`cpt-cf-quota-enforcement-fr-quota-metadata`) - `inst-qmd-size`
2. [ ] - `p1` - Validate the object against the constraint contract attached to the metric request contract (derived from
   `gts.cf.core.qe.constraint.v1~`, published per the projection-contracts feature); the contract defines keys,
   requiredness, types, enums, and nesting - `inst-qmd-contract`
   1. [ ] - `p1` - Wrap the wire object into the contract envelope `{type, metadata}` and validate the whole document,
      never the inner subschema alone (ADR-0007 envelope rule) - `inst-qmd-envelope`
3. [ ] - `p1` - **IF** the object violates the contract - `inst-qmd-mismatch-if`
   1. [ ] - `p1` - **RETURN** `DomainError::ConstraintContractMismatch` (canonical `FailedPrecondition`);
      increment `contract_validation_failures_total` with the closed `arbitration` surface - `inst-qmd-mismatch`
4. [ ] - `p1` - Snapshot the accepted contract id/version alongside the stored metadata; a stored value is not
   revalidated during evaluation (write-time only, per ADR-0003/0007) - `inst-qmd-snapshot`
5. [ ] - `p1` - Treat the content as semantically opaque: QE never interprets business meaning and never indexes
   metadata for direct query; validated metadata is stored and forwarded verbatim to the active Engine as
   the Engine's `arbitration` object (Engine consumption is owned by the resolution-policy-engine
   feature) - `inst-qmd-opaque`
6. [ ] - `p1` - Metadata must not carry PII or other regulated data (Platform Operational Data per PRD §6.2);
   operators are responsible for the content respecting this classification - `inst-qmd-pii`
7. [ ] - `p1` - **RETURN** the validated metadata; the full object is returned by every Quota read API subject to PDP
   scoping - `inst-qmd-return`

### Validity-Window Computation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-validity-window`

**Input**: a stored Quota's optional `validity_window` (start and end timestamps), the server clock

**Output**: the stored window plus the server-computed boolean `currently_within_window`

**Steps**:
1. [ ] - `p1` - Store the `validity_window` verbatim as a structural field; when absent, the Quota has no time bounds
   and remains evaluable until explicitly deactivated - `inst-qvw-store`
2. [ ] - `p1` - The core never auto-deactivates a Quota when `now() > validity_end`; lifecycle (active vs
   deactivated) and validity-window bounds are independent dimensions, and window behavior at evaluation time is
   Engine-side (the default exclusion belongs to the resolution-policy-engine feature) - `inst-qvw-no-auto`
3. [ ] - `p1` - Compute `currently_within_window` at read time as "the server clock falls inside
   `[validity_start, validity_end]`", treating an absent bound as unbounded on that side, so callers render expiry
   state without recomputing the comparison - `inst-qvw-compute`
4. [ ] - `p1` - **RETURN** the window and the computed boolean on Quota reads; the snapshot-reads feature surfaces
   the same two fields on Quota Snapshots per `cpt-cf-quota-enforcement-fr-quota-snapshot-read` - `inst-qvw-return`

## 4. States (CDSL)

### Quota State Machine

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-state-quota-lifecycle`

**States**: Active, Deactivated

**Initial State**: Active

**Transitions**:
1. [ ] - `p1` - **FROM** Active **TO** Active **WHEN** a non-breaking update commits; the quota ID and subject
   reference are preserved and a `quota-changed (updated)` event is enqueued same-tx - `inst-qst-update`
2. [ ] - `p1` - **FROM** Active **TO** Deactivated **WHEN** `deactivate_quota` commits; the atomic cascade of
   `cpt-cf-quota-enforcement-flow-quota-deactivate` resolves every active lease in the same transaction - `inst-qst-deactivate`

Deactivated is terminal in P1: no reactivation endpoint exists in the DESIGN interface inventory, and a breaking
change is expressed as deactivate-plus-create. A Deactivated Quota stops accepting new debits or leases, remains
readable, and is retained until the P2 audit-aware purge (`cpt-cf-quota-enforcement-fr-quota-lifecycle`,
DESIGN table inventory). Counter mutation against either state is owned by the consumption-operations and
lease-operations features.

## 5. Definitions of Done

### Quota Management Service and CRUD Surface

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-quota-crud`

The system **MUST** deliver `QuotaManagementService`
(`cpt-cf-quota-enforcement-component-quota-management-service`) with transactional create, update, deactivate, and
read over the storage plugin's Quota CRUD group, exposed as the five REST endpoints and the
`QuotaManagerClientV1` methods `create_quota`, `update_quota`, `deactivate_quota`, and `read_quotas`. Every draft
runs the full validation chain outside the storage transaction; every mutation enqueues its `quota-changed` event in
the same transaction (I11) and appends the operation-log entry (I1); the cap-vs-consumed guard is evaluated at commit
time in-tx (I6). Updates preserve the quota ID and the subject reference; breaking changes are rejected.

**Implements**:
- `cpt-cf-quota-enforcement-flow-quota-create`
- `cpt-cf-quota-enforcement-flow-quota-update`
- `cpt-cf-quota-enforcement-flow-quota-read`
- `cpt-cf-quota-enforcement-algo-quota-draft-validation`
- `cpt-cf-quota-enforcement-algo-validity-window`
- `cpt-cf-quota-enforcement-state-quota-lifecycle`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`,
`cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: `POST /v1/quota-enforcement/quotas`, `GET /v1/quota-enforcement/quotas/{id}`,
  `PATCH /v1/quota-enforcement/quotas/{id}`, `POST /v1/quota-enforcement/quotas/{id}/deactivate`,
  `GET /v1/quota-enforcement/quotas`; `QuotaManagerClientV1` Quota methods
- DB: `cpt-cf-quota-enforcement-db-schema` (`quotas` table plus the allocation-counter materialization)
- Entities: `Quota`, `QuotaDraft`, `QuotaPatch`, `QuotaFilter`, `QuotaId`, `DeactivateOutcome`

### Metric Identity Validation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-metric-validation`

The system **MUST** validate at Quota create and update time that the referenced metric exists in `types-registry`
via `TypesRegistryClient` with an in-process LRU cache inside `QuotaManagementService`, reporting an unknown metric
as `METRIC_NOT_REGISTERED` and failing closed with an actionable error when the registry is unreachable. A `Direct`
metric mode **MUST NOT** reject creation; the inert Quota is surfaced through `quota_for_direct_metric_total`. A
persisted Quota whose metric is later removed **MUST** be flagged via operational telemetry and **MUST NOT** be
auto-deactivated.

**Implements**:
- `cpt-cf-quota-enforcement-algo-metric-validation`

**Constraints**: `cpt-cf-quota-enforcement-constraint-types-registry-delegation`

**Touches**:
- API: `TypesRegistryClient` (platform `types-registry-sdk`, obtained from ClientHub; never called inside the
  storage transaction)
- Entities: registry-reported classifications (kind, `QuotaGated`/`Direct` mode)

### Quota Metadata Contract Enforcement

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-quota-metadata`

The system **MUST** validate the operator-authored `metadata` object at create and update time only: the
operator-configurable canonical-JSON size limit (default 4 KB) and conformance to the metric owner's separate
constraint contract, snapshotting the accepted contract id/version and never revalidating the stored value
during evaluation. Metadata **MUST** stay semantically opaque (no interpretation, no direct-query indexing), **MUST**
be returned in full by every Quota read API subject to PDP scoping, and metadata changes **MUST** keep the quota ID
stable while emitting a `quota-changed` event.

**Implements**:
- `cpt-cf-quota-enforcement-algo-quota-metadata-validation`

**Constraints**: `cpt-cf-quota-enforcement-constraint-types-registry-delegation`

**Touches**:
- API: no new endpoint (rides the create/update/read paths)
- DB: `cpt-cf-quota-enforcement-db-schema` (`quotas` metadata storage with the snapshotted contract id/version)
- Entities: `ConstraintContract` (projection-contracts feature)

### Deactivation Cascade

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-deactivation-cascade`

The system **MUST** implement `deactivate_quota` as one atomic transaction that marks the Quota deactivated, marks
every active lease against it resolved-by-deactivation, decrements the active-lease counters, returns held capacity
to the acquisition-period counters, and enqueues the `quota-changed (deactivated)` event plus one
`lease-resolved-by-deactivation` event per affected lease (I11), returning
`DeactivateOutcome { resolved_leases }`. The record stays readable; new debits and leases against it are rejected by
the evaluation paths.

**Implements**:
- `cpt-cf-quota-enforcement-flow-quota-deactivate`
- `cpt-cf-quota-enforcement-state-quota-lifecycle`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`

**Touches**:
- API: `POST /v1/quota-enforcement/quotas/{id}/deactivate`; `QuotaManagerClientV1::deactivate_quota`
- DB: `cpt-cf-quota-enforcement-db-schema` (`quotas`, `leases`, `lease_holds`, `lease_capacity_counters`,
  `notification_outbox`)
- Entities: `DeactivateOutcome`

### Rate Quota-Type Rejection

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-rate-rejection`

The system **MUST** reserve the `rate` GTS instance (`gts.cf.qe.quota.type.v1~cf.qe.quota.rate.v1`) and reject Quota
create and update requests referencing it with `DomainError::NotYetImplemented`, canonicalized as `Unimplemented`
(HTTP 501, `NOT_YET_IMPLEMENTED`), leaving the data model and API surface able to add rate semantics in P3 without
breaking changes and without migrating persisted `allocation`/`consumption` Quotas.

**Implements**:
- `cpt-cf-quota-enforcement-algo-quota-draft-validation`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`

**Touches**:
- API: the Quota create/update endpoints (rejection behavior only; no new route)
- Entities: `quota_type` GTS instances

### Lifecycle Telemetry Gauges

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-lifecycle-gauges`

The system **MUST** surface the label-free gauges `quota_cap_zero_total`, `quota_cap_unbounded_total`, and
`quota_for_direct_metric_total` from the closed PRD §5.16 catalogue through the foundation telemetry conventions,
counting active `cap = 0` Quotas, active `cap = null` Quotas, and Quotas declared on `Direct`-classified metrics
respectively. No high-cardinality identifier (`quota_id`, `tenant_id`, metric, projection type) appears as a label.

**Implements**:
- `cpt-cf-quota-enforcement-flow-quota-create`
- `cpt-cf-quota-enforcement-flow-quota-update`

**Constraints**: `cpt-cf-quota-enforcement-constraint-bounded-cardinality`

**Touches**:
- API: platform observability stack (`tracing` + `toolkit` `otel` feature, per the foundation telemetry conventions)
- Entities: gear-specific gauges per PRD §5.16

## 6. Acceptance Criteria

- [ ] Creating a Quota persists the row with a server-assigned quota ID and status `active`, materializes the
  allocation counter for allocation type, and enqueues `quota-changed (created)` plus the operation-log entry in the
  same transaction; a crash between validation and commit leaves no partial state
- [ ] A create or update naming an unregistered metric fails with `METRIC_NOT_REGISTERED`; with `types-registry`
  unreachable and the LRU unable to answer, the write fails with an actionable error and nothing is persisted
  (fail-closed, verified by fault injection on the registry dependency)
- [ ] A create referencing a registered projection outside the configured catalogue fails with
  `PROJECTION_NOT_RESOLVABLE`; one referencing a projection that does not admit the metric is rejected before
  persistence (via the projection-contracts membership check); a `subject_id` violating the contract's declared
  scope is rejected before persistence by this feature's subject-scope validation (`inst-qcr-subject-scope`)
- [ ] Negative `cap` is rejected with `CAP_MUST_BE_NON_NEGATIVE`; `cap = 0` and `cap = null` are both accepted, and
  the `quota_cap_zero_total` / `quota_cap_unbounded_total` gauges reflect the active counts
- [ ] `notification_thresholds` on a `cap = null` Quota are rejected with `THRESHOLDS_REQUIRE_BOUNDED_CAP`, at create
  and at update
- [ ] Creating a second Quota for the same `(subject, metric)` pair succeeds; QE never rejects on that basis
- [ ] A create or update with `type = rate` returns canonical `Unimplemented` (HTTP 501, `NOT_YET_IMPLEMENTED`); on
  update, the rate check runs before the breaking-change gate; `enforcement_mode` values other than the `hard` GTS
  instance and `source` values outside the seeded `licensing`/`operator` instances are rejected
- [ ] An update reducing `cap` (or moving `null` to numeric) strictly below the consumed or in-flight amount fails
  with `CAP_BELOW_CONSUMED` under concurrent counter mutation driven through the storage plugin counter primitives
  (foundation dependency), proving commit-time in-tx evaluation; a cap raise and a numeric-to-`null` update bypass
  the guard
- [ ] An update touching metric, type, period, or subject is rejected; the quota ID and subject reference survive
  every accepted update
- [ ] Metadata over the configured size limit or violating the owner's constraint contract is rejected before
  persistence and increments `contract_validation_failures_total` with the `arbitration` surface; an accepted
  write snapshots the contract id/version, and a metadata-only update keeps the quota ID stable while emitting
  `quota-changed`
- [ ] Deactivating a Quota with N active leases marks all N resolved-by-deactivation, decrements the lease-capacity
  counters, returns held capacity to the acquisition-period counters, enqueues one
  `lease-resolved-by-deactivation` event per lease plus `quota-changed (deactivated)`, and appends the operation-log
  entry (I1), all in one transaction;
  `DeactivateOutcome.resolved_leases` lists the N leases; a failure mid-cascade resolves none
- [ ] A deactivated Quota remains readable through both read endpoints; reads return the full metadata object, the
  `validity_window`, and a `currently_within_window` value consistent with the server clock; list reads are
  PDP-scoped so cross-tenant rows never appear
- [ ] A Quota created on a `Direct`-classified metric is accepted and increments `quota_for_direct_metric_total`
- [ ] A Quota whose `validity_end` has passed is not auto-deactivated; its state stays `active` and only
  `currently_within_window` flips to false

## 7. Additional Context (optional)

- **Bootstrap compatibility query (tracked here)**: the projection-contracts feature checks the configured catalogue
  against `QuotaEnforcementStoragePluginV1::read_active_projection_bindings()` at bootstrap, the distinct
  `(metric, projection_type)` pairs of active Quotas, implemented so far by the in-memory storage double only. This
  feature adds the storage plugin's database query over the Quota table and its integration tests. Together with
  Policy compatibility from the resolution-policy-engine feature, that closes
  `cpt-cf-quota-enforcement-dod-projection-catalog` of the projection-contracts feature.
- **Upstream catalogue gaps (tracked upstream prerequisites)**: PRD `cpt-cf-quota-enforcement-fr-quota-metadata`
  requires telemetry on metadata size distribution, and
  `cpt-cf-quota-enforcement-fr-metric-identity-validation` requires flagging Quotas whose metric was later removed;
  the closed PRD §5.16 instrument catalogue names no instrument for either. This feature adds no instrument beyond
  the catalogue; both signals are tracked as upstream catalogue additions and are carried on structured diagnostics
  in the interim (size and removal flags are not high-cardinality identifiers, and metadata content itself is never
  logged).
- **Read surface for `currently_within_window`**: DECOMPOSITION §2.3 assigns validity-window storage and the
  `currently_within_window` computation to this feature, and the Quota read and list endpoints are its only read
  surface, so the boolean is surfaced there. PRD grants the field to the Quota Snapshot read APIs
  (`cpt-cf-quota-enforcement-fr-quota-snapshot-read`); the DESIGN Quota read response does not yet name the field,
  and that read-contract alignment is a tracked upstream prerequisite.
- **ADR dependency**: the constraint contract shape and the write-time-only validation rule follow
  `cpt-cf-quota-enforcement-adr-projection-contracts` (ADR-0007, status accepted).
- **Shared component**: `cpt-cf-quota-enforcement-component-quota-management-service` is owned here and later
  extended by the bulk-quota-crud feature (P2) through the same contract, per DECOMPOSITION §2.12; the P2 bulk
  endpoints are deliberately absent from this document.
- **Event ownership boundary**: this feature only enqueues `quota-changed` and `lease-resolved-by-deactivation`
  events in the same transaction as the state mutation (I11); dispatch, retries, and dead-lettering are owned by the
  notifications feature, and the event payload catalogue lives with
  `cpt-cf-quota-enforcement-fr-notification-plugin`.
- **Rollout / rollback**: the write path is stateless above the storage plugin; rollout is a rolling update under
  the same schema major version. Deactivation is not reversible in P1, so operational runbooks treat it as
  deactivate-plus-create, matching the breaking-change rule.
- **Test layering**: draft validation, cap semantics, and window computation get unit tests; metric validation
  fail-closed and metadata contract enforcement get integration tests with registry fault injection; the cascade
  atomicity and the commit-time cap guard get concurrency integration tests (kill mid-transaction, concurrent
  counter mutation through the storage plugin).
- **Non-applicable review domains**: UX/accessibility is not applicable; there is no user-facing surface. Data
  protection: Quota Metadata is Platform Operational Data per PRD §6.2 and must not carry PII; no further
  feature-specific handling is added.
