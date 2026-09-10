<!-- Created: 2026-08-26 by Constructor Tech -->

# Feature: Bulk Quota CRUD

- [ ] `p2` - **ID**: `cpt-cf-quota-enforcement-featstatus-bulk-quota-crud-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-quota-enforcement-feature-bulk-quota-crud`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Bulk Create Quotas](#bulk-create-quotas)
  - [Bulk Update Quotas](#bulk-update-quotas)
  - [Bulk Deactivate Quotas](#bulk-deactivate-quotas)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Bulk Envelope Execution](#bulk-envelope-execution)
- [4. States (CDSL)](#4-states-cdsl)
  - [Bulk Envelope State Machine](#bulk-envelope-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Bulk Endpoints and Envelope Validation](#bulk-endpoints-and-envelope-validation)
  - [All-or-Nothing Envelope Atomicity and Replay](#all-or-nothing-envelope-atomicity-and-replay)
  - [Bulk Deactivation Lease Resolution](#bulk-deactivation-lease-resolution)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Implements the three transactional bulk Quota endpoints (`bulk_create_quotas`, `bulk_update_quotas`,
`bulk_deactivate_quotas`) as envelope wrappers over the single-item Quota CRUD semantics owned by the quota-lifecycle
feature: envelope idempotency, all-or-nothing application, the operator-configurable maximum batch size with
`BULK_TOO_LARGE` enforcement, and per-item failure attribution by index and reason.

### 1.2 Purpose

Quota Manager workflows materialize multiple Quotas from a single logical event (license-pack provisioning, plan
migration, redistribution batches, tenant offboarding). With only per-Quota CRUD, those workflows must compose
individual calls and carry their own compensation logic for partial failures (PRD §5.2 rationale). This feature pushes
the transactional atomicity into Quota Enforcement: the entire batch either commits or rolls back, and a failure names
the offending item(s) by index and reason so the caller retries with corrections. Per DECOMPOSITION §2.12 this feature
extends `QuotaManagementService` (owned by the quota-lifecycle feature) through the same contract; the per-item
create, update, and deactivate semantics, including the deactivation lease cascade, are consumed unchanged and never
re-specified here.

**Scope**: the three bulk endpoints with envelope idempotency keys and `BULK_TOO_LARGE` enforcement (default maximum
50 items per batch, operator-configurable); all-or-nothing application in a single storage transaction; per-item
failure attribution; atomic lease resolution across every Quota deactivated in a batch, with the entire batch's
lease-resolution events emitted atomically with the deactivation transaction.

**Out of scope**: any partial-success mode (all-or-nothing is the contract, per DECOMPOSITION §2.10); the single-item
CRUD semantics, validation chain, and deactivation cascade (the quota-lifecycle feature, consumed unchanged); the
lease state machine and lease operations (the lease-operations feature; this document only relies on the recorded
`ResolvedByDeactivation` transition); notification dispatch (the notifications feature; this feature only enqueues
events in the same transaction as its state mutation, invariant I11); the idempotency replay mechanism (established by
the consumption-operations feature and consumed here unchanged).

**Requirements**: `cpt-cf-quota-enforcement-fr-bulk-quota-crud`

**Principles**: none of its own; the feature extends the quota-lifecycle semantics unchanged (per DECOMPOSITION
§2.10).

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-quota-enforcement-actor-quota-manager` | Submits bulk create, update, and deactivate envelopes for materialization flows, with an envelope idempotency key per operation |
| `cpt-cf-quota-enforcement-actor-platform-operator` | Uses the same endpoints for operator-driven batch changes and configures the maximum batch size |
| `cpt-cf-quota-enforcement-actor-storage-backend` | Serializes the envelope transaction and provides the all-or-nothing commit or rollback across every item |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) (§5.2 Bulk Quota CRUD, §5.8 idempotency, §3.4 trust boundary)
- **Design**: [DESIGN.md](../DESIGN.md) (`QuotaManagementService`, §3.3 error model with `DomainError::BulkTooLarge`,
  the REST inventory deferral note for the P2 bulk endpoints, and the §4.3 future-considerations table entry for
  Bulk Quota CRUD)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) (§2.10)
- **ADR**: [ADR-0002 Acquisition ordering](../ADR/0002-cpt-cf-quota-enforcement-adr-acquisition-ordering.md)
  (`cpt-cf-quota-enforcement-adr-acquisition-ordering`; the ordering applies uniformly to every mutation primitive)
- **Dependencies**: `cpt-cf-quota-enforcement-feature-quota-lifecycle` (the single-item CRUD semantics, validation
  chain, and deactivation cascade this feature wraps), `cpt-cf-quota-enforcement-feature-lease-operations` (the lease
  state machine whose `ResolvedByDeactivation` transition the bulk deactivation drives at batch scale), plus
  transitively `cpt-cf-quota-enforcement-feature-consumption-operations` (the idempotency replay mechanism),
  `cpt-cf-quota-enforcement-feature-projection-contracts`, and `cpt-cf-quota-enforcement-feature-foundation`

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-quota-enforcement-usecase-create-quota` (the bulk endpoints wrap the same lifecycle use case at
batch scale; no dedicated bulk use case exists in the PRD)

### Bulk Create Quotas

- [ ] `p2` - **ID**: `cpt-cf-quota-enforcement-flow-bulk-create`

No dedicated sequence exists: per DECOMPOSITION §2.10 the bulk endpoints follow the single-item sequences
(`cpt-cf-quota-enforcement-seq-quota-create`) with an envelope wrapper.

**Actor**: `cpt-cf-quota-enforcement-actor-quota-manager` (and
`cpt-cf-quota-enforcement-actor-platform-operator` through the same endpoint)

**Success Scenarios**:
- Every draft is valid: all listed Quotas are created in one transaction, each with the full single-item create side
  effects, and the caller receives the created Quota identities
- A replay of the same envelope idempotency key returns the original outcome without re-applying

**Error Scenarios**:
- More items than the configured maximum: `BULK_TOO_LARGE` before any item is validated
- Any draft fails the single-item validation chain (unknown metric, negative cap, `type = rate`, metadata violation,
  projection outside the catalogue, a `subject_id` that violates the declared scope discriminator): the envelope
  fails with that item's canonical error, attributed by index and reason, and no Quota from the batch is persisted

**Steps**:
1. [ ] - `p2` - Caller sends `POST /v1/quota-enforcement/quotas/bulk-create` with an envelope carrying the envelope
   idempotency key and the list of Quota drafts; each draft names its explicit target `(projection_type, subject_id)`
   under PDP scope exactly as the single-item create, and foundation admission
   (`cpt-cf-quota-enforcement-flow-authorized-admission`) has attached `SecurityContext` and `AccessScope`
   (`cpt-cf-quota-enforcement-fr-bulk-quota-crud`) - `inst-qbc-request`
2. [ ] - `p2` - Run `cpt-cf-quota-enforcement-algo-bulk-envelope` with the create items; an exact replay
   short-circuits to the stored outcome - `inst-qbc-envelope`
3. [ ] - `p2` - **FOR EACH** draft in submission order, outside the storage transaction: run the quota-lifecycle
   validation chain unchanged (`cpt-cf-quota-enforcement-algo-quota-draft-validation`,
   `cpt-cf-quota-enforcement-algo-metric-validation`, the projection-contracts membership check invoked by the
   single-item create flow, the subject-scope validation of the single-item create flow, and
   `cpt-cf-quota-enforcement-algo-quota-metadata-validation` when the draft carries `metadata`) - `inst-qbc-validate`
4. [ ] - `p2` - DB: apply every create inside the single envelope transaction of
   `cpt-cf-quota-enforcement-algo-bulk-envelope`; each item produces the same persisted effects as the single-item
   create (`cpt-cf-quota-enforcement-flow-quota-create`): the `quotas` row with a server-assigned UUIDv7 `quota_id`
   and status `active`, the allocation-counter materialization for allocation type, the
   `quota-changed (change_kind='created')` event (I11), and the operation-log entry (I1) - `inst-qbc-apply`
5. [ ] - `p2` - **RETURN** the outcome identifying every created Quota; the response is the stored envelope outcome
   that later replays of the same key return verbatim - `inst-qbc-return`

### Bulk Update Quotas

- [ ] `p2` - **ID**: `cpt-cf-quota-enforcement-flow-bulk-update`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-manager` (and
`cpt-cf-quota-enforcement-actor-platform-operator` through the same endpoint)

**Success Scenarios**:
- Every `{id, patch}` item passes the single-item update rules: all patches are applied in one transaction, each with
  the single-item side effects, and every quota ID and subject reference is preserved

**Error Scenarios**:
- Any item hits a single-item update rejection (`type = rate`, a breaking change to metric, type, period, or subject,
  `QUOTA_DEACTIVATED`, `THRESHOLDS_REQUIRE_BOUNDED_CAP`, unknown quota ID): the envelope fails with that item's
  canonical error, attributed by index and reason, and no patch from the batch is applied
- Any item's cap reduction lands below the consumed or in-flight amount at commit time: the envelope rolls back with
  `CAP_BELOW_CONSUMED` attributed to the item

**Steps**:
1. [ ] - `p2` - Caller sends `POST /v1/quota-enforcement/quotas/bulk-update` with an envelope carrying the envelope
   idempotency key and the `{id, patch}` items (`cpt-cf-quota-enforcement-fr-bulk-quota-crud`) - `inst-qbu-request`
2. [ ] - `p2` - Run `cpt-cf-quota-enforcement-algo-bulk-envelope` with the update items; an exact replay
   short-circuits to the stored outcome - `inst-qbu-envelope`
3. [ ] - `p2` - **FOR EACH** item in submission order, outside the storage transaction: apply the single-item update
   gates unchanged (`cpt-cf-quota-enforcement-flow-quota-update`): the `rate` rejection before the breaking-change
   gate, the breaking-change rejection for metric, type, period, or subject, then
   `cpt-cf-quota-enforcement-algo-quota-draft-validation`, `cpt-cf-quota-enforcement-algo-metric-validation`, and
   `cpt-cf-quota-enforcement-algo-quota-metadata-validation` when the patch carries `metadata` - `inst-qbu-validate`
4. [ ] - `p2` - DB: apply every patch inside the single envelope transaction of
   `cpt-cf-quota-enforcement-algo-bulk-envelope`; each item keeps the single-item commit-time semantics: the
   cap-vs-consumed guard is evaluated at the moment the envelope transaction commits, in-tx with a row-level lock
   (I6), and each item appends its operation-log entry (I1) and enqueues `quota-changed (change_kind='updated')`
   (I11) - `inst-qbu-apply`
5. [ ] - `p2` - **IF** any item trips a commit-time guard such as `CAP_BELOW_CONSUMED` - `inst-qbu-guard-if`
   1. [ ] - `p2` - Roll back the entire envelope and **RETURN** the item's canonical error attributed by index and
      reason; no patch from the batch survives - `inst-qbu-guard`
6. [ ] - `p2` - **RETURN** the committed outcome; every quota ID and subject reference is preserved exactly as under
   the single-item update - `inst-qbu-return`

### Bulk Deactivate Quotas

- [ ] `p2` - **ID**: `cpt-cf-quota-enforcement-flow-bulk-deactivate`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-manager` (and
`cpt-cf-quota-enforcement-actor-platform-operator` through the same endpoint)

**Success Scenarios**:
- Every listed Quota is marked deactivated in one transaction; each affected Quota's active leases are
  resolved-by-deactivation per the quota-lifecycle deactivation rules, and the entire batch's lease-resolution events
  are emitted atomically with the deactivation transaction

**Error Scenarios**:
- Any listed quota ID is unknown: the envelope fails with canonical `NotFound` attributed by index; no Quota is
  deactivated
- Any listed Quota is already deactivated: the envelope fails with `QUOTA_DEACTIVATED` attributed by index; the other
  Quotas stay active and no second cascade runs

**Steps**:
1. [ ] - `p2` - Caller sends `POST /v1/quota-enforcement/quotas/bulk-deactivate` with an envelope carrying the
   envelope idempotency key and the list of quota IDs (`cpt-cf-quota-enforcement-fr-bulk-quota-crud`) - `inst-qbd-request`
2. [ ] - `p2` - Run `cpt-cf-quota-enforcement-algo-bulk-envelope` with the deactivate items; an exact replay
   short-circuits to the stored outcome - `inst-qbd-envelope`
3. [ ] - `p2` - DB: apply every deactivation inside the single envelope transaction of
   `cpt-cf-quota-enforcement-algo-bulk-envelope`; each item runs the single-item cascade semantics unchanged
   (`cpt-cf-quota-enforcement-flow-quota-deactivate`): mark the Quota deactivated, mark every active lease against it
   resolved-by-deactivation, decrement the lease-capacity counters, return held capacity to the acquisition-period
   counters, and append the operation-log entry (I1) - `inst-qbd-cascade`
4. [ ] - `p2` - Enqueue `quota-changed (change_kind='deactivated')` per Quota plus one
   `lease-resolved-by-deactivation` event per affected lease, for every Quota in the batch, in the same transaction
   (I11); the entire batch's lease-resolution events are emitted atomically with the deactivation transaction
   (`cpt-cf-quota-enforcement-fr-bulk-quota-crud`) - `inst-qbd-events`
5. [ ] - `p2` - The batch-level cascade never partially completes: either every listed Quota is deactivated with
   every one of its active leases resolved, or none is; the resolved leases follow the
   `cpt-cf-quota-enforcement-state-lease` transition to `ResolvedByDeactivation` recorded by the lease-operations
   feature - `inst-qbd-atomic`
6. [ ] - `p2` - **RETURN** the outcome carrying the per-Quota resolved-lease summaries (the single-item
   `DeactivateOutcome { resolved_leases }` information for each item) so the gateway can attribute telemetry exactly
   as for the single-item deactivation - `inst-qbd-return`

## 3. Processes / Business Logic (CDSL)

### Bulk Envelope Execution

- [ ] `p2` - **ID**: `cpt-cf-quota-enforcement-algo-bulk-envelope`

**Input**: one bulk envelope (operation kind `bulk_create_quotas`, `bulk_update_quotas`, or `bulk_deactivate_quotas`)
with its required envelope idempotency key, optional per-item idempotency keys, and the items; the `SecurityContext`
and `AccessScope` attached by foundation admission

**Output**: a committed all-or-nothing outcome with the envelope idempotency record persisted, or a canonical error
(`Problem`) with per-item attribution and no persisted change

**Steps**:
1. [ ] - `p2` - DB: `lookup_idempotency` on the envelope key under the idempotency machinery established by the
   consumption-operations feature (`cpt-cf-quota-enforcement-algo-idempotency-replay`, consumed unchanged for the
   three bulk operation types); on an exact replay **RETURN** the stored outcome without re-applying
   (`cpt-cf-quota-enforcement-fr-bulk-quota-crud`); a divergent payload under the same envelope scope returns
   `IDEMPOTENCY_PAYLOAD_MISMATCH` (409) leaving the original record untouched; the replay short-circuit precedes the
   size check, so an exact replay returns the stored outcome even after an operator lowers the maximum batch size
   (section 7 note) - `inst-qbe-idem`
2. [ ] - `p2` - **IF** the item count exceeds the operator-configurable maximum batch size (default 50 items per
   batch) - `inst-qbe-size-if`
   1. [ ] - `p2` - **RETURN** `BULK_TOO_LARGE` (`DomainError::BulkTooLarge`, canonical `InvalidArgument`, 400) as an
      actionable error before any item is validated - `inst-qbe-size`
3. [ ] - `p2` - Accept per-item idempotency keys when supplied: they identify items individually in outcomes and
   diagnostics; the PRD assigns them individual identification only, so no per-item replay semantics exist
   (`cpt-cf-quota-enforcement-fr-bulk-quota-crud`) - `inst-qbe-item-keys`
4. [ ] - `p2` - Enforce the same PDP authorization, tenant-isolation, and trust-boundary rules as the single-item
   counterparts (`cpt-cf-quota-enforcement-fr-authorization`, `cpt-cf-quota-enforcement-fr-tenant-isolation`, PRD
   §3.4): admission runs on the envelope request through the foundation Gateway, every item's explicit target
   identity must fall inside the caller's `AccessScope`, and the `AccessScope` is forwarded into every storage call
   for in-transaction consumption - `inst-qbe-authz`
5. [ ] - `p2` - **FOR EACH** item in submission order: run the per-item validation of the single-item counterpart, as
   referenced by the calling flow, outside the storage transaction (the quota-lifecycle minimum-lock-window rule) - `inst-qbe-validate`
6. [ ] - `p2` - **IF** any item fails validation - `inst-qbe-invalid-if`
   1. [ ] - `p2` - **RETURN** the failing item's canonical error with the offending item(s) identified by index and
      reason, carried as `errors[].reason` tokens inside the RFC 9457 `Problem` envelope per the DESIGN error model;
      nothing is persisted, so the caller can retry with corrections under a new envelope key - `inst-qbe-invalid`
7. [ ] - `p2` - DB: **TRY** apply every item in a single storage transaction: per-item mutation with its single-item
   side effects (operation-log entry I1, outbox events I11), row locks on the affected Quota set taken in ascending
   lexicographic `quota_id` order (`cpt-cf-quota-enforcement-adr-acquisition-ordering`, which applies uniformly to
   every mutation primitive), and the envelope idempotency record persisted inside the same transaction; commit - `inst-qbe-apply`
8. [ ] - `p2` - **CATCH** any per-item failure inside the transaction (commit-time guards, storage errors) - `inst-qbe-catch`
   1. [ ] - `p2` - Roll back the entire batch, persist nothing (no envelope idempotency record survives a rollback),
      and **RETURN** the canonical error attributing the offending item(s) by index and reason; a retry under the
      same envelope key is re-executed because the failed envelope applied nothing - `inst-qbe-rollback`
9. [ ] - `p2` - **RETURN** the committed outcome; the bulk endpoints are pure-CRUD surfaces, so they never produce a
   Decision shape, and every failure surfaces as a `Problem` (DESIGN §3.3); partial success is not a permitted
   outcome in any branch - `inst-qbe-return`

## 4. States (CDSL)

### Bulk Envelope State Machine

- [ ] `p2` - **ID**: `cpt-cf-quota-enforcement-state-bulk-envelope`

The lifecycle of one bulk envelope; the Quota entity's own state machine is owned by the quota-lifecycle feature
(`cpt-cf-quota-enforcement-state-quota-lifecycle`) and is not re-specified here.

**States**: Validating, Applying, Committed, RolledBack

**Initial State**: Validating

**Transitions**:
1. [ ] - `p2` - **FROM** Validating **TO** Applying **WHEN** the envelope idempotency lookup misses, the size check
   passes, and every item passes its single-item validation chain; an exact replay short-circuits to the stored
   outcome without entering Applying - `inst-qbs-enter`
2. [ ] - `p2` - **FROM** Applying **TO** Committed **WHEN** every item's mutation succeeds inside the single
   transaction; the per-item side effects, the batch's events, and the envelope idempotency record commit together - `inst-qbs-commit`
3. [ ] - `p2` - **FROM** Applying **TO** RolledBack **WHEN** any item's mutation fails inside the transaction; the
   entire batch rolls back, no counter, Quota, or lease change survives, and no envelope idempotency record persists - `inst-qbs-rollback`

Committed and RolledBack are terminal for the envelope; the idempotency record persisted by the Committed outcome then
follows the idempotency-record lifecycle owned by the consumption-operations feature.

## 5. Definitions of Done

### Bulk Endpoints and Envelope Validation

- [ ] `p2` - **ID**: `cpt-cf-quota-enforcement-dod-bulk-endpoints`

The system **MUST** deliver the three bulk endpoints `POST /v1/quota-enforcement/quotas/bulk-create`,
`POST /v1/quota-enforcement/quotas/bulk-update`, and `POST /v1/quota-enforcement/quotas/bulk-deactivate` as an
extension of `QuotaManagementService` (`cpt-cf-quota-enforcement-component-quota-management-service`, owned by the
quota-lifecycle feature and extended through the same contract per DECOMPOSITION §2.12). Each bulk operation **MUST**
carry a single envelope idempotency key; per-item idempotency keys **MAY** also be supplied for individual
identification. Batches over the operator-configurable maximum size (default 50 items per batch) **MUST** be rejected
with an actionable `BULK_TOO_LARGE` error before any item is validated. Failures **MUST** identify the offending
item(s) by index and reason, carried per the DESIGN error model inside the RFC 9457 `Problem` envelope. The bulk
operations **MUST** be subject to the same PDP authorization, tenant-isolation, and trust-boundary rules as their
single-item counterparts (`cpt-cf-quota-enforcement-fr-authorization`,
`cpt-cf-quota-enforcement-fr-tenant-isolation`, PRD §3.4).

**Implements**:
- `cpt-cf-quota-enforcement-flow-bulk-create`
- `cpt-cf-quota-enforcement-flow-bulk-update`
- `cpt-cf-quota-enforcement-flow-bulk-deactivate`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`,
`cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: `POST /v1/quota-enforcement/quotas/bulk-create`, `POST /v1/quota-enforcement/quotas/bulk-update`,
  `POST /v1/quota-enforcement/quotas/bulk-deactivate`
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: `Quota`, `QuotaDraft`, `QuotaPatch`, `QuotaId` (quota-lifecycle entities, reused per item)

### All-or-Nothing Envelope Atomicity and Replay

- [ ] `p2` - **ID**: `cpt-cf-quota-enforcement-dod-bulk-atomicity`

The system **MUST** apply every bulk envelope all-or-nothing in a single storage transaction: atomically create every
listed Quota or none, atomically apply every patch or none, and atomically deactivate every listed Quota or none;
partial failure rolls back the entire batch, and partial success is not a permitted outcome. Each item **MUST**
produce the same persisted side effects as its single-item counterpart (the quota-lifecycle validation chain outside
the transaction, the operation-log entry per I1, the `quota-changed` event per I11, and the commit-time cap guard per
I6), consumed unchanged and never re-specified. Row locks on the affected Quota set **MUST** follow the ascending
lexicographic `quota_id` ordering (`cpt-cf-quota-enforcement-adr-acquisition-ordering`). An exact replay of a
committed envelope key **MUST** return the original outcome without re-applying, with the envelope idempotency record
persisted in the same transaction as the batch; a divergent payload under the same envelope scope **MUST** return
`IDEMPOTENCY_PAYLOAD_MISMATCH`, and the replay machinery is the one established by the consumption-operations feature,
consumed unchanged.

**Implements**:
- `cpt-cf-quota-enforcement-algo-bulk-envelope`
- `cpt-cf-quota-enforcement-state-bulk-envelope`

**Constraints**: `cpt-cf-quota-enforcement-constraint-single-storage-plugin`,
`cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: no new endpoint (the semantics behind the three bulk endpoints)
- DB: `cpt-cf-quota-enforcement-db-schema` (`quotas`, counter tables, `operation_log`, `idempotency_records`,
  `notification_outbox`)
- Entities: `IdempotencyRecord`

### Bulk Deactivation Lease Resolution

- [ ] `p2` - **ID**: `cpt-cf-quota-enforcement-dod-bulk-deactivate-cascade`

The system **MUST** resolve leases atomically across every Quota deactivated in a batch: each affected Quota's active
leases are resolved-by-deactivation per the quota-lifecycle deactivation rules
(`cpt-cf-quota-enforcement-fr-quota-lifecycle`, `cpt-cf-quota-enforcement-flow-quota-deactivate`, consumed unchanged),
and the entire batch's lease-resolution events **MUST** be emitted atomically with the deactivation transaction: one
`quota-changed (deactivated)` event per Quota plus one `lease-resolved-by-deactivation` event per affected lease,
enqueued in the same transaction (I11). A failure on any item **MUST** leave every Quota in the batch active and every
lease untouched. The resolved leases follow the lease-operations state machine's `ResolvedByDeactivation` transition;
no lease semantics are re-specified here.

**Implements**:
- `cpt-cf-quota-enforcement-flow-bulk-deactivate`
- `cpt-cf-quota-enforcement-algo-bulk-envelope`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`,
`cpt-cf-quota-enforcement-constraint-single-storage-plugin`

**Touches**:
- API: `POST /v1/quota-enforcement/quotas/bulk-deactivate`
- DB: `cpt-cf-quota-enforcement-db-schema` (`quotas`, `leases`, `lease_holds`, `lease_capacity_counters`,
  `notification_outbox`)
- Entities: `DeactivateOutcome` (per item), `Lease` (lease-operations entity, transitioned only)

## 6. Acceptance Criteria

- [ ] A bulk-create of N valid drafts (N at most the configured maximum) persists all N Quotas in one transaction,
  each with a server-assigned quota ID, status `active`, its allocation-counter materialization where applicable, its
  `quota-changed (created)` event, and its operation-log entry, plus the envelope idempotency record; a crash or
  injected failure mid-batch leaves no Quota, counter row, event, or idempotency record from the batch
- [ ] A bulk-create in which one draft names an unregistered metric fails the envelope with `METRIC_NOT_REGISTERED`
  identifying the offending item index and reason in the `Problem` envelope; no Quota from the batch is persisted
- [ ] A batch of 51 items under the default limit fails with `BULK_TOO_LARGE` before any item is validated; after the
  operator changes the configured maximum, the new bound is enforced
- [ ] An exact replay of a committed envelope key returns the original outcome without re-applying: no second create,
  patch, or cascade occurs and no counter moves; the replay returns the stored outcome even after the operator lowers
  the maximum batch size below the original batch's item count; a divergent payload under the same envelope key fails
  with `IDEMPOTENCY_PAYLOAD_MISMATCH` leaving the original record untouched
- [ ] A bulk-update applies every patch or none: an item that trips the commit-time `CAP_BELOW_CONSUMED` guard under
  concurrent counter mutation rolls back the entire batch, and the error names the item index and reason
- [ ] A bulk-update item touching metric, type, period, or subject fails the envelope via the single-item
  breaking-change gate; an item referencing `type = rate` fails the envelope with canonical `Unimplemented` (501); in
  both cases no patch from the batch is applied
- [ ] A bulk-deactivate of K Quotas with active leases marks all K deactivated and every active lease against them
  resolved-by-deactivation in one transaction, decrements the lease-capacity counters, returns held capacity to the
  acquisition-period counters, and enqueues one `quota-changed (deactivated)` event per Quota plus one
  `lease-resolved-by-deactivation` event per lease in the same transaction; an injected failure mid-cascade leaves
  every Quota active and every lease untouched
- [ ] A bulk-deactivate containing one unknown quota ID fails the envelope with canonical `NotFound` attributed by
  index; one containing an already-deactivated Quota fails with `QUOTA_DEACTIVATED` attributed by index; in both
  cases the remaining Quotas stay active
- [ ] An item whose explicit target identity falls outside the caller's tenant or `AccessScope` fails the envelope
  under the same authorization and tenant-isolation rules as the single-item endpoints, with no partial application
  (adversarial integration test)
- [ ] Interleaved bulk envelopes over overlapping Quota sets observe no deadlock (ADR-0002 ordering) and no partial
  application (concurrency integration test)
- [ ] No request parameter or mode selects partial success on any of the three endpoints; the responses never carry a
  Decision shape, and every failure is an RFC 9457 `Problem`
- [ ] The feature adds no metric instrument and no label dimension; the scrape is unchanged against the closed PRD
  §5.16 catalogue

## 7. Additional Context (optional)

- **Extension boundary** (DECOMPOSITION §2.12): `QuotaManagementService` is owned by the quota-lifecycle feature;
  this feature extends it through the same contract. The per-item validation chain
  (`cpt-cf-quota-enforcement-algo-quota-draft-validation`, `cpt-cf-quota-enforcement-algo-metric-validation`,
  `cpt-cf-quota-enforcement-algo-quota-metadata-validation`), the deactivation cascade, and the lease state machine
  are consumed unchanged and never re-specified here. The idempotency replay mechanism is the consumption-operations
  feature's; this feature only registers the three bulk operation types with it.
- **Upstream alignment items (tracked upstream prerequisites)**:
  - The DESIGN storage-plugin trait names only single-item Quota CRUD primitives; the DESIGN §4.3
    future-considerations table says
    the storage plugin "already exposes transactional batch primitives via the `apply_batch_debit` precedent" for
    this P2 surface, but no bulk Quota primitive is named. This document requires the single-transaction semantics
    and does not invent a plugin method name; the concrete primitive shape (one new batch primitive following the
    `apply_batch_debit` precedent, or the existing single-item primitives composed under one transaction) is a
    tracked upstream DESIGN item.
  - The DESIGN SDK trait `QuotaManagerClientV1` carries no bulk methods, and the DESIGN REST inventory defers the
    three endpoints as P2. This document uses the PRD operation names (`bulk_create_quotas`, `bulk_update_quotas`,
    `bulk_deactivate_quotas`) as logical operation names only; the SDK trait extension and the request/response DTO
    definitions are tracked upstream DESIGN items, and no new Rust type or method name is introduced here.
  - PRD §5.8 defines the idempotency scope `(tenant_id, idempotency_subject_key, operation_type, key)` and its subject-slot rules for
    the consumption write operations; Quota CRUD operations carry no single-item idempotency key, and the PRD does
    not define the subject slot for a management envelope whose items can span multiple subjects. This document
    reuses the established machinery under the three bulk operation types and leaves the envelope scope's
    subject-slot rule as a tracked upstream PRD/DESIGN item; it does not invent one.
- **Deliberately unpinned behavior**: the PRD pins neither the ordering of the replay lookup against the size check
  nor validation coverage after the first failing item. This document places the replay short-circuit first, matching
  the batch-debit precedent, so a committed envelope stays replayable after an operator lowers the size limit; whether
  validation continues past the first failing item to attribute several items at once is left to the implementation
  ("item(s)" in the PRD permits both), and no test may depend on it. The PRD likewise does not order the size check
  against per-item validation; this document places the size check before any item is validated, and the flow,
  algorithm, and DoD statements follow that placement. The PRD is also silent on duplicate quota IDs or
  identical drafts within one envelope; that behavior is left to the implementation. The PRD text "replay returns the
  original outcome without re-applying" is read here as covering committed envelopes: a rolled-back envelope persists
  no idempotency record and applied nothing, so a retry under the same key is re-executed, matching the
  batch-debit canonical-error precedent.
- **No NFR ownership**: the DECOMPOSITION NFR allocation assigns no NFR to this feature, and DECOMPOSITION §2.10
  lists none; this document adds no latency, throughput, or availability promise. The blast radius of a misconfigured
  caller is bounded by the maximum batch size per the PRD rationale.
- **Rust contract notes**: the envelope handler is stateless above the storage plugin; per-item validation runs
  outside the storage transaction and holds no in-process lock across an await point, and row serialization inside
  the envelope transaction is delegated to the storage plugin under the ADR-0002 ordering. The envelope payloads are
  plain data reusing the quota-lifecycle entity shapes (`QuotaDraft`, `QuotaPatch`, `QuotaId`), which are Send + Sync
  compatible. Error attribution maps onto the closed `DomainError` enum and the canonical `Problem` envelope; no new
  error variant is introduced (`DomainError::BulkTooLarge` already exists in the DESIGN error model).
- **Rollout / rollback**: the endpoints are additive REST surface over the existing schema major version; no
  migration accompanies them, and disabling the endpoints returns callers to per-Quota CRUD with operator-side
  compensation, which is the documented P1 fallback per the PRD rationale.
- **Test layering**: the size limit, replay short-circuit, and attribution shape get unit tests; all-or-nothing
  application, the commit-time guard rollback, the bulk deactivation cascade, and the authorization parity get
  integration and adversarial tests against the storage plugin; the interleaved-envelope deadlock check is the
  concurrency integration test named in section 6.
- **Non-applicable review domains**: UX/accessibility is not applicable; there is no user-facing surface. Data
  protection inherits the quota-lifecycle rules (Quota Metadata is Platform Operational Data per PRD §6.2); this
  feature adds no new data category.
