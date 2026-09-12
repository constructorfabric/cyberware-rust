<!-- Created: 2026-08-26 by Constructor Tech -->

# Feature: Consumption Operations & Idempotency

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-featstatus-consumption-operations-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-quota-enforcement-feature-consumption-operations`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Debit](#debit)
  - [Credit](#credit)
  - [Rollback](#rollback)
  - [Evaluate Preview](#evaluate-preview)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Evaluation Orchestrator Pipeline](#evaluation-orchestrator-pipeline)
  - [Idempotency Lookup and Replay](#idempotency-lookup-and-replay)
  - [Period Rollover and Settlement](#period-rollover-and-settlement)
  - [Idempotency and Operation-Log Retention Sweep](#idempotency-and-operation-log-retention-sweep)
- [4. States (CDSL)](#4-states-cdsl)
  - [Consumption Period Counter State Machine](#consumption-period-counter-state-machine)
  - [Idempotency Record State Machine](#idempotency-record-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Consumption Operation Endpoints](#consumption-operation-endpoints)
  - [Evaluation Orchestrator](#evaluation-orchestrator)
  - [Idempotency Guarantee](#idempotency-guarantee)
  - [Counter Shapes and Period Semantics](#counter-shapes-and-period-semantics)
  - [Retention Sweeper](#retention-sweeper)
  - [Hot-Path NFR Verification](#hot-path-nfr-verification)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Establishes the `EvaluationOrchestrator` and its canonical pipeline, and implements the four single-shot counter
operations: debit, credit, rollback, and read-only preview. Covers the allocation and consumption counter shapes with
lazy period-row materialization, calendar-aligned UTC periods with deterministic rollover and settlement semantics, and
the typed idempotency scope `(tenant_id, subject_key, operation_type, idem_key)` that makes every write replay-safe.

### 1.2 Purpose

This is the hot path: every guarded operation in a consuming service turns into a debit, and every retry of that debit
must land exactly once. This feature joins the Quota records of the quota-lifecycle feature with the Decision contract
of the resolution-policy-engine feature into one atomic transaction, so a caller never observes a partially applied
Debit Plan, a double-counted retry, or a counter that changed on a denial. It also owns the period machinery that
resets consumption counters at calendar boundaries, and the retention sweep that keeps idempotency and operation-log
storage bounded. Behavior is fail-closed: when the pipeline cannot produce a Decision, it surfaces a canonical error
and mutates nothing.

**Scope**: S2S `QuotaEnforcementService` entry points for debit, credit, rollback, and preview;
the canonical orchestrator pipeline (resolution, idempotency lookup, locked read, Policy lookup, Engine, invariant
check, mutation, idempotency persist, outbox enqueue, commit); allocation and consumption counter shapes with lazy
period-row materialization and the threshold-marker reset (I13); debit applying Engine Debit Plans atomically; credit
against a named Quota; rollback by original idempotency key with settlement-keyed closure; preview with no persisted
state; the `INVALID_AMOUNT` fail-fast ordering and `IDEMPOTENCY_PAYLOAD_MISMATCH`; period rollover event ordering and
the credit/rollback closure asymmetry; idempotency and operation-log retention via `RetentionSweeper`.

**Out of scope**: two-phase holds (the lease-operations feature), multi-item envelopes (the batch-debit feature),
subject resolution itself (defined by the projection-contracts feature as
`cpt-cf-quota-enforcement-algo-subject-resolution` and consumed here), Policy selection, Engine invocation, and the
Debit-Plan invariant boundary (defined by the resolution-policy-engine feature as
`cpt-cf-quota-enforcement-algo-engine-boundary` and invoked here), and notification dispatch (the notifications
feature; this feature only enqueues events in the same transaction as its counter mutation, invariant I11).

**Requirements**: `cpt-cf-quota-enforcement-fr-debit`, `cpt-cf-quota-enforcement-fr-credit`,
`cpt-cf-quota-enforcement-fr-rollback`, `cpt-cf-quota-enforcement-fr-evaluate-preview`,
`cpt-cf-quota-enforcement-fr-idempotency`, `cpt-cf-quota-enforcement-fr-period-semantics`,
`cpt-cf-quota-enforcement-fr-period-rollover`, `cpt-cf-quota-enforcement-fr-quota-type-allocation`,
`cpt-cf-quota-enforcement-fr-quota-type-consumption`, `cpt-cf-quota-enforcement-nfr-evaluation-latency`,
`cpt-cf-quota-enforcement-nfr-throughput`, `cpt-cf-quota-enforcement-nfr-subject-scale`,
`cpt-cf-quota-enforcement-nfr-quota-density`, `cpt-cf-quota-enforcement-nfr-availability`,
`cpt-cf-quota-enforcement-nfr-fault-tolerance`, `cpt-cf-quota-enforcement-nfr-idempotency-guarantee`

**Principles**: none of its own; the feature executes the strict-engine-boundary and fail-closed principles owned by
earlier features (per DECOMPOSITION §2.5).

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-quota-enforcement-actor-quota-consumer` | Sends debit, rollback, and preview operations with client-supplied idempotency keys on the write paths |
| `cpt-cf-quota-enforcement-actor-quota-manager` | Uses the same S2S credit and preview surface for authorized tenant-management workflows |
| `cpt-cf-quota-enforcement-actor-quota-reader` | Uses preview as a read-only "what would happen" affordance |
| `cpt-cf-quota-enforcement-actor-storage-backend` | Serializes concurrent counter mutations and provides the durable-commit guarantee behind RPO = 0 |
| `cpt-cf-quota-enforcement-actor-monitoring-system` | Scrapes `denial_total` and the pipeline stage traces |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) (§5.3 quota types, §5.4 period and reset semantics, §5.5 debit/credit/rollback/preview,
  §5.8 idempotency, §3.4 Decision contract, §6.1 gear NFRs)
- **Design**: [DESIGN.md](../DESIGN.md) (`QuotaEnforcementService`, `EvaluationOrchestrator`, `IdempotencyCache`,
  `RetentionSweeper`, storage-plugin counter-mutation group, §3.3 error model, §3.6 sequences, §4.1 telemetry, §4.2
  performance verification)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) (§2.5)
- **ADR**: [ADR-0002 Acquisition ordering](../ADR/0002-cpt-cf-quota-enforcement-adr-acquisition-ordering.md),
  [ADR-0003 Metadata snapshot timing](../ADR/0003-cpt-cf-quota-enforcement-adr-metadata-snapshot-timing.md),
  [ADR-0004 Settlement window emit](../ADR/0004-cpt-cf-quota-enforcement-adr-settlement-window-emit.md),
  [ADR-0007 Declarative GTS projection contracts](../ADR/0007-cpt-cf-quota-enforcement-adr-projection-contracts.md)
  (`cpt-cf-quota-enforcement-adr-projection-contracts`, status: accepted; the ingress and resolution contracts this
  pipeline consumes follow that ADR)
- **Dependencies**: `cpt-cf-quota-enforcement-feature-quota-lifecycle` (Quota records and the allocation-counter
  materialization), `cpt-cf-quota-enforcement-feature-resolution-policy-engine` (Policy selection, Engine invocation,
  and the Debit-Plan invariant boundary), plus transitively
  `cpt-cf-quota-enforcement-feature-projection-contracts` (ingress validation and the subject-resolution extension)
  and `cpt-cf-quota-enforcement-feature-foundation` (storage plugin, admission, coordination, telemetry conventions)

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-quota-enforcement-usecase-debit` (the debit body; admission and ingress prefixes are owned by
the foundation and projection-contracts features), `cpt-cf-quota-enforcement-usecase-cascade-via-cel` and
`cpt-cf-quota-enforcement-usecase-region-gated-via-metadata` (the debit that exercises them; the Policy authoring side
is owned by the resolution-policy-engine feature)

### Debit

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-debit`

Realises `cpt-cf-quota-enforcement-seq-debit`.

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Success Scenarios**:
- An `Allowed` Decision whose Debit Plan is applied atomically across every named Quota, with the idempotency record,
  operation-log entry, and outbox events committed in the same transaction
- A `Denied` Decision returned as an HTTP 200 verdict with every counter unchanged
- A replay of the same idempotency key returning the original Decision verbatim with no second counter effect

**Error Scenarios**:
- `amount <= 0`: `INVALID_AMOUNT` before any pipeline step; nothing is persisted
- Same key, different payload: `IDEMPOTENCY_PAYLOAD_MISMATCH` (409); the original record is untouched
- Target metric classified `Direct`: `METRIC_NOT_QUOTA_GATED`; no counter mutation, no idempotency, operation-log, or
  lease record
- Engine timeout, invariant violation, or mid-flight storage failure: platform-canonical error, fail-closed, no
  counter mutation

**Steps**:
1. [ ] - `p1` - Caller sends `POST /v1/quota-enforcement/operations/debit` with a `DebitRequest` carrying `tenant_id`,
   additional subjects, one operation-level `metadata` object, optional resource, metric, positive integer `amount`,
   and the client-supplied idempotency key; foundation
   admission (`cpt-cf-quota-enforcement-flow-authorized-admission`) and projection-contracts ingress validation
   (`cpt-cf-quota-enforcement-flow-ingress-validation`) have already run - `inst-deb-request`
2. [ ] - `p1` - **IF** `amount <= 0` - `inst-deb-amount-if`
   1. [ ] - `p1` - **RETURN** `INVALID_AMOUNT` (`DomainError::InvalidAmount`, canonical `InvalidArgument`) before
      idempotency lookup, subject resolution, or any other pipeline step; no idempotency record, operation-log entry,
      or counter mutation occurs (`cpt-cf-quota-enforcement-fr-debit`) - `inst-deb-amount`
3. [ ] - `p1` - Run the pipeline `cpt-cf-quota-enforcement-algo-evaluation-pipeline`; on `Allowed`, the storage plugin
   applies the Engine's Debit Plan atomically: for every entry, an allocation Quota increments its in-flight counter
   and a consumption Quota increases the current-period consumed amount by `entry.amount`; Quotas not named in the
   plan are never mutated - `inst-deb-pipeline`
4. [ ] - `p1` - Decision-shaped request fields are silently ignored per the PRD §3.4 trust boundary; the Decision is
   server-derived and response-only - `inst-deb-trust`
5. [ ] - `p1` - **IF** the Decision is `Denied` - `inst-deb-denied-if`
   1. [ ] - `p1` - **RETURN** the Decision as an HTTP 200 verdict with an empty `debit_plan` and no counter mutation;
      increment `denial_total` by the closed `reason` kind - `inst-deb-denied`
6. [ ] - `p1` - **RETURN** the `Decision` body (`result`, `debit_plan`, `diagnostics`); the SDK path is
   `QuotaEnforcementClientV1::debit(req)` returning `Decision`; a Decision and a `Problem` are mutually exclusive
   outcomes - `inst-deb-return`

### Credit

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-credit`

Realises `cpt-cf-quota-enforcement-seq-credit`.

**Actor**: `cpt-cf-quota-enforcement-actor-quota-manager`

**Success Scenarios**:
- The named Quota's remaining capacity increases by the credited amount, with the `quota-counter-adjusted` event and
  the operation-log entry committed in the same transaction
- A replay of the same idempotency key returns the original outcome without a second counter effect

**Error Scenarios**:
- `amount <= 0`: `INVALID_AMOUNT` before idempotency lookup or the row-locked read
- Unknown `quota_id`: canonical `NotFound` (404, `UNKNOWN_QUOTA`)
- Quota outside the caller's tenant: `PdpDenied` (403, storage defense-in-depth)
- Deactivated Quota: `QUOTA_DEACTIVATED` (400)
- Consumption Quota whose calendar window has elapsed: `PERIOD_CLOSED` (400)

**Steps**:
1. [ ] - `p1` - Quota Manager sends `POST /v1/quota-enforcement/operations/credit` with a `CreditRequest` naming an
   explicit `quota_id`, a positive integer `amount`, and an idempotency key; credit invokes no subject resolution and
   no Engine (`cpt-cf-quota-enforcement-fr-credit`) - `inst-cre-request`
2. [ ] - `p1` - **IF** `amount <= 0` - `inst-cre-amount-if`
   1. [ ] - `p1` - **RETURN** `INVALID_AMOUNT` before idempotency lookup, the row-locked Quota read, or any other
      pipeline step; nothing is persisted - `inst-cre-amount`
3. [ ] - `p1` - DB: `lookup_idempotency` inside the transaction; on an exact replay **RETURN** the stored outcome
   verbatim per `cpt-cf-quota-enforcement-algo-idempotency-replay` - `inst-cre-idem`
4. [ ] - `p1` - DB: read the Quota row under a row lock, so the four rejection arms and the mutation share atomic
   semantics - `inst-cre-lock`
5. [ ] - `p1` - **IF** the row is absent, cross-tenant, deactivated, or a consumption Quota whose calendar window has
   elapsed (`time >= period_end` at the moment the transaction is evaluated) - `inst-cre-guard-if`
   1. [ ] - `p1` - **RETURN** the matching rejection before any mutation: `StorageError::QuotaNotFound` lifts to
      `NotFound { kind: "quota" }` (404), `StorageError::SubjectOutOfScope` lifts to `PdpDenied` (403),
      `StorageError::QuotaDeactivated` lifts to `QUOTA_DEACTIVATED` (400), and `StorageError::PeriodClosed` lifts to
      `PERIOD_CLOSED` (400); credit closure is calendar-keyed, so the rejection fires immediately at the boundary even
      while the settlement window is still draining cross-period lease commits - `inst-cre-guard`
6. [ ] - `p1` - DB: `apply_credit(quota_id, amount, idem_key, events)` in the same transaction: an allocation Quota
   decrements its in-flight counter and a consumption Quota decreases the current-period consumed amount, both with a
   floor of zero; the idempotency record uses an `IdempotencySubjectKey` fingerprint of the owning Quota's
   `(projection_type, subject_id)` read under the same row lock, per `cpt-cf-quota-enforcement-fr-idempotency`, and
   the operation-log entry are persisted, and the `quota-counter-adjusted` event carrying the credited amount, the
   target `quota_id`, and the authenticated service principal from `SecurityContext` is enqueued (I11); commit - `inst-cre-apply`
7. [ ] - `p1` - **RETURN** the outcome; the SDK path is `QuotaEnforcementClientV1::credit(req)` returning `Decision` - `inst-cre-return`

### Rollback

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-rollback`

Realises `cpt-cf-quota-enforcement-seq-rollback`.

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Success Scenarios**:
- The original debit's counter effect is reversed against its attribution period, with the `quota-rollback-applied`
  event committed in the same transaction
- A rollback replay is a no-op returning the stored Decision

**Error Scenarios**:
- Original-debit key not found: `UNKNOWN_OPERATION` (canonical `NotFound`)
- Attribution period already settled (`period-rollover` emitted): `PERIOD_CLOSED`; no mutation and no event
- Rollback targeting a credit: rejected; credits are not reversible via rollback

**Steps**:
1. [ ] - `p1` - Caller sends `POST /v1/quota-enforcement/operations/rollback` with a `RollbackRequest` carrying the
   original debit's idempotency key and the rollback's own idempotency key - `inst-rlb-request`
2. [ ] - `p1` - DB: `lookup_idempotency` on the rollback's own key; on an exact replay **RETURN** the stored Decision
   per `cpt-cf-quota-enforcement-algo-idempotency-replay` - `inst-rlb-idem`
3. [ ] - `p1` - DB: look up the original committed debit by the original idempotency key; lease-commit-derived debits
   are addressable through the commit call's idempotency key exactly like direct debits
   (`cpt-cf-quota-enforcement-fr-rollback`) - `inst-rlb-lookup`
4. [ ] - `p1` - **IF** no committed debit exists under the original key - `inst-rlb-unknown-if`
   1. [ ] - `p1` - **RETURN** `UNKNOWN_OPERATION` (canonical `NotFound`); a rollback against a credit is likewise
      rejected, since credits are corrective and not reversible via rollback - `inst-rlb-unknown`
5. [ ] - `p1` - **IF** the debit's attribution period has been fully settled, meaning its `period-rollover` event has
   been emitted - `inst-rlb-settled-if`
   1. [ ] - `p1` - **RETURN** `PERIOD_CLOSED` before any mutation, with no event; rollback closure is
      settlement-keyed, intentionally asymmetric with credit's calendar-keyed closure, so cross-period lease commits
      stay reversible during the settlement window - `inst-rlb-settled`
6. [ ] - `p1` - DB: `apply_rollback(original_idem_key, idem_key, events)` in one transaction: lock the affected counter
   rows, reverse the original mutation against the debit's `acquisition_period_id` (I5), never the wall-clock current
   period, persist the rollback's idempotency record using the `IdempotencySubjectKey` fingerprint of the owning
   Quota's `(projection_type, subject_id)`, append the operation-log entry, and enqueue the `quota-rollback-applied` event
   carrying the original idempotency key, the rolled-back amount, the target Quota, and the consumer identity from
   `SecurityContext` (I11); commit - `inst-rlb-apply`
7. [ ] - `p1` - **RETURN** the Decision; the SDK path is `QuotaEnforcementClientV1::rollback(req)`; replay of the same
   rollback is a no-op after the first invocation - `inst-rlb-return`

### Evaluate Preview

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-evaluate-preview`

Realises `cpt-cf-quota-enforcement-seq-evaluate-preview`.

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer` (also
`cpt-cf-quota-enforcement-actor-quota-manager` and `cpt-cf-quota-enforcement-actor-quota-reader`)

**Success Scenarios**:
- The full evaluation pipeline runs read-only and returns the would-be Decision with an explicit `preview: true`
  marker and Policy attribution in diagnostics

**Error Scenarios**:
- PDP denial or unreachability: canonical error, identical to the corresponding write (fail-closed)
- Ingress-validation failures: canonical `InvalidArgument` per the projection-contracts feature
- Debit-Plan invariant violation: canonical `Internal` with the `INVARIANT_VIOLATION` sub-token, identical to the
  corresponding write (fail-closed)

**Steps**:
1. [ ] - `p1` - Caller sends `POST /v1/quota-enforcement/operations/preview` with the same attribution, metric,
   amount, metadata, and optional resource as the corresponding write but no idempotency key; preview requires the same PDP authorization as
   the corresponding write (`cpt-cf-quota-enforcement-fr-evaluate-preview`) - `inst-prv-request`
2. [ ] - `p1` - Consume the PDP-authorized, catalogue-mapped subject set (`cpt-cf-quota-enforcement-algo-subject-resolution`), then DB:
   `read_quota_snapshot` as a transactional snapshot read of current counter state with no read-modify hold and no
   contention against concurrent debits (I3) - `inst-prv-read`
3. [ ] - `p1` - DB: `read_policy(scope)`; invoke the Engine boundary
   (`cpt-cf-quota-enforcement-algo-engine-boundary`) unchanged: an invariant violation on the returned Decision
   surfaces the same canonical `Internal` error as the corresponding write (fail-closed) and increments
   `debit_plan_invariant_violations_total` per the engine boundary - `inst-prv-engine`
4. [ ] - `p1` - Persist nothing: no counter mutation, no held capacity, no idempotency record, no operation-log entry,
   and no outbox event; only ephemeral telemetry counters move; the single permitted persisted side effect is the I3
   lazy materialization of a fresh period row with `consumed = 0`, which is not a counter mutation - `inst-prv-nopersist`
5. [ ] - `p1` - **RETURN** `DecisionPreview` carrying the full Decision shape plus `preview: true`, with `policy_id`
   and `policy_version` in diagnostics; the SDK path is `QuotaEnforcementClientV1::evaluate_preview(req)`; the verdict
   can be invalidated by concurrent debits, and callers that need held capacity use the lease flow instead
   (lease-operations feature) - `inst-prv-return`

## 3. Processes / Business Logic (CDSL)

### Evaluation Orchestrator Pipeline

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-evaluation-pipeline`

**Input**: a validated and mapped write request (authorized tenant/subjects, metadata, optional resource, metric,
amount, idempotency key), `SecurityContext`, and the `AccessScope` returned by `PolicyEnforcer`

**Output**: a committed Decision with all side effects applied atomically, or a platform-canonical error with no
counter mutation

**Steps**:
1. [ ] - `p1` - Consume the complete PDP-authorized, catalogue-mapped subject set from
   `cpt-cf-quota-enforcement-algo-subject-resolution`; `EvaluationOrchestrator` never re-authorizes or re-maps it - `inst-pipe-resolve`
2. [ ] - `p1` - Canonicalize the complete resolved set into `IdempotencySubjectKey`, construct the typed
   `IdempotencyScope { tenant_id, subject_key, operation_type, idem_key }`, then DB: `lookup_idempotency(scope)`; **IF**
   a record exists, short-circuit
   to `cpt-cf-quota-enforcement-algo-idempotency-replay` without opening the evaluation transaction - `inst-pipe-idem`
3. [ ] - `p1` - DB: begin the single backend transaction and run the applicable-Quotas locked read
   (`read_quota_snapshot` with lock); `EvaluationContext` metadata is captured at this locked-read step per ADR-0003,
   and lazy period-row materialization runs here when a boundary was crossed
   (`cpt-cf-quota-enforcement-algo-period-rollover`) - `inst-pipe-lockread`
4. [ ] - `p1` - **IF** the operation targets a Quota whose metric is classified `Direct` - `inst-pipe-gated-if`
   1. [ ] - `p1` - **RETURN** `METRIC_NOT_QUOTA_GATED` (`StorageError::MetricNotQuotaGated` lifted to canonical
      `FailedPrecondition`) with no counter mutation and no idempotency, operation-log, or lease record; this
      admission-time rejection applies to every write and preview entry point (PRD §3.2) - `inst-pipe-gated`
5. [ ] - `p1` - DB: `read_policy(scope = metric, else global)`, then invoke the Engine boundary
   (`cpt-cf-quota-enforcement-algo-engine-boundary`, resolution-policy-engine feature): Policy selection, the
   synchronous no-I/O `evaluate` call, the per-Policy timeout, `EngineError` lifting, and the closed Debit-Plan
   invariant set are all specified there and invoked here unchanged - `inst-pipe-engine`
6. [ ] - `p1` - **IF** the boundary surfaces a canonical error (timeout, cost cap, invariant violation, internal) - `inst-pipe-fail-if`
   1. [ ] - `p1` - **RETURN** the canonical error and roll the transaction back; fail-closed, no counter mutation,
      no idempotency record - `inst-pipe-fail`
7. [ ] - `p1` - DB: `apply_debit_plan(applicable, plan, idem_scope, events)`: mutate every plan entry's counter, persist
   the idempotency record, append the operation-log entry, and enqueue the outbox events, all inside the same
   transaction (I1, I2, I11); a `Denied` Decision changes no counter and persists its idempotency record with an empty
   plan so an exact replay returns the original verdict (PRD §5.8), while the operation log records only successful
   mutating operations per the DESIGN `OperationLog` definition; a `NO_APPLICABLE_QUOTA` denial persists no idempotency
   record and no operation-log entry per the PRD acceptance criteria (the tension with §5.8 replay durability is a
   tracked upstream PRD item, section 7); concurrent multi-Quota mutations follow the deterministic lexicographic
   `quota_id` acquisition ordering of ADR-0002 - `inst-pipe-apply`
8. [ ] - `p1` - Forward the `AccessScope` unmodified into every storage read and write, where `SecureConn` compiles it
   into query filters (phase-2 defense-in-depth per
   `cpt-cf-quota-enforcement-algo-pdp-constraint-composition`); the orchestrator never calls the PDP, holds no
   `TypesRegistryClient`, and performs no schema resolution, so registry latency cannot enter the transaction - `inst-pipe-scope`
9. [ ] - `p1` - Emit the pipeline stage spans (`subject_resolution`, `applicable_quotas_fetch`, `policy_lookup`,
   `engine_evaluate`, `invariant_check`, `storage.apply_debit_plan`, `notification.enqueue`) per DESIGN §4.1 through
   the foundation telemetry conventions - `inst-pipe-spans`
10. [ ] - `p1` - **RETURN** the committed Decision; orchestrator instances are not singletons, join no
    cluster election, and delegate all synchronization between concurrent instances to the storage plugin's
    serialization of row mutations (I9) - `inst-pipe-return`

### Idempotency Lookup and Replay

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-idempotency-replay`

Realises `cpt-cf-quota-enforcement-seq-idempotency-replay`.

**Input**: a write operation's typed `IdempotencyScope`, the operation payload, `SecurityContext`

**Output**: the stored outcome verbatim, an `IDEMPOTENCY_PAYLOAD_MISMATCH` rejection, or a cache miss that proceeds to
full evaluation

**Steps**:
1. [ ] - `p1` - Construct `IdempotencySubjectKey` as the SHA-256 fingerprint of the canonical sorted, deduplicated
   subject set: debit uses the complete authorized, catalogue-mapped applicable set; credit and rollback use the owning Quota's
   persisted `(projection_type, subject_id)` pair read under the mutation row lock. The caller never supplies or
   narrows the subject key, and `quota_id` is not part of it (`cpt-cf-quota-enforcement-fr-idempotency`) - `inst-idem-scope`
2. [ ] - `p1` - Different tenants, subject keys, or operation types using the same key string create independent records;
   they are never cross-matched - `inst-idem-independent`
3. [ ] - `p1` - Consult the `IdempotencyCache` in-process LRU of recent records (operator-tunable TTL, P1 reference
   default 5 s) for the most contended keys, then DB: `lookup_idempotency(scope)` with the typed full scope - `inst-idem-lookup`
4. [ ] - `p1` - **IF** a record exists and the canonical SHA-256 hash of the sorted-JSON payload matches the stored
   `payload_hash` - `inst-idem-replay-if`
   1. [ ] - `p1` - **RETURN** the stored `decision_blob` verbatim, including `result`, `debit_plan`, `diagnostics`,
      and the original identifiers and amounts; the Engine is never re-invoked, counters are never touched, and the
      non-deterministic `EvaluationContext` fields (notably `time`) are never re-bound, so a replay at a different
      wall-clock time still produces the original verdict; replay diagnostics surface the recorded `engine_id`,
      `policy_id`, and `policy_version` verbatim - `inst-idem-replay`
5. [ ] - `p1` - **IF** a record exists and the payload hash diverges - `inst-idem-mismatch-if`
   1. [ ] - `p1` - **RETURN** `IDEMPOTENCY_PAYLOAD_MISMATCH` (`DomainError::IdempotencyPayloadMismatch`, canonical
      `Aborted`, 409) without touching the original record - `inst-idem-mismatch`
6. [ ] - `p1` - On a miss, proceed with the full pipeline; the record is persisted implicitly by the mutating storage
   primitive in the same transaction as the mutation (I1, I2), capturing the full Decision blob plus the `engine_id`,
   `policy_id`, and `policy_version` under which it was produced; the `decision_blob` is JSON-typed and
   schema-versioned (top-level `__version`) so additive P2/P3 shape changes need no migration - `inst-idem-persist`
7. [ ] - `p1` - **RETURN** replays attempted after the retention window has expired for the key are treated as new
   operations and re-evaluated against current state (`cpt-cf-quota-enforcement-algo-retention-sweep` owns the
   reclamation) - `inst-idem-window`

### Period Rollover and Settlement

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-period-rollover`

Realises `cpt-cf-quota-enforcement-seq-period-rollover`.

**Input**: a consumption Quota's counter row with its persisted period boundary timestamp, the transaction commit time

**Output**: deterministic period attribution, the new period row, and the `period-rollover` event for the closing
period

**Steps**:
1. [ ] - `p1` - Periods are drawn from the five GTS instances under `gts.cf.qe.period.type.v1~` (`day`, `week`,
   `month`, `year`, `one_time`), all UTC and calendar-aligned by default; the current period boundary timestamp is
   persisted with each consumption counter for deterministic detection
   (`cpt-cf-quota-enforcement-fr-period-semantics`) - `inst-per-spec`
2. [ ] - `p1` - Attribute every single-shot mutation to the period whose half-open interval `[start, end)` contains
   the mutation transaction's commit timestamp; a debit committing at exactly `boundary_at` is accounted to period
   `P+1`; leases are attributed to their acquisition period, per the lease-operations feature and I5 - `inst-per-attr`
3. [ ] - `p1` - **IF** any evaluate observes `now() >= period_end` for a consumption Quota (lazy detection; the single
   permitted I3 exception) - `inst-per-lazy-if`
   1. [ ] - `p1` - DB: atomically materialize the new `quota_consumption_counters` row with `consumed = 0` and
      `highest_crossed_threshold_pct = NULL` (I13), so `threshold-crossed` notifications can fire again in the new
      period per the notifications feature's emission rule - `inst-per-lazy`
4. [ ] - `p1` - Mark the closing-period row settled only after every active lease with the closing acquisition period
   has resolved, and enqueue the `period-rollover` event carrying the closing-period consumed amount, the
   closing-period cap, and the new period boundary; the event is emitted strictly after the last commit attributed to
   the closing period, so it signals settlement completion, not the calendar transition, and may lag the boundary by
   up to `max_lease_ttl` - `inst-per-settle`
5. [ ] - `p1` - During the settlement window, cross-period lease commits and releases mutate the closing-period
   counter and emit no `quota-counter-adjusted` or `threshold-crossed` events
   (`cpt-cf-quota-enforcement-adr-settlement-window-emit`) - `inst-per-window`
6. [ ] - `p1` - Unused capacity is forfeited at every boundary; QE models no carry-over, and downstream consumers may
   derive unused-capacity figures from the `period-rollover` payload - `inst-per-forfeit`
7. [ ] - `p1` - **RETURN** the current-period row; a `one_time` period never auto-resets and its Quota is deactivated
   explicitly when exhausted or expired; for Quotas with no operations in the new period the closing event fires only
   on the next operation, a known P1 limitation with a P2 active-scheduler hook (DESIGN §4.3) - `inst-per-return`

### Idempotency and Operation-Log Retention Sweep

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-retention-sweep`

**Input**: the `idempotency_retention_config` table (default 24 h, per-`(tenant, metric)` overrides), the
operation-log retention setting (default 30 days), the coordination adapter's `retention-sweeper` election

**Output**: physically reclaimed expired `idempotency_records` and `operation_log` rows

**Steps**:
1. [ ] - `p1` - API: `RetentionSweeper` hands its reclamation body to the coordination adapter for
   `SingletonScope::RetentionSweeper`; the body runs only while this replica is elected, on a child
   `CancellationToken` that leadership loss cancels; the resolved cluster backend renews the claim and re-election is
   automatic (foundation `cpt-cf-quota-enforcement-dod-coordination-adapter` owns the election semantics) - `inst-ret-elect`
2. [ ] - `p1` - DB: `reclaim_expired_idempotency` in operator-configured batches, honoring the per-`(tenant, metric)`
   retention window from `idempotency_retention_config`; the window bounds the replay guarantee of
   `cpt-cf-quota-enforcement-fr-idempotency` - `inst-ret-idem`
3. [ ] - `p1` - DB: `reclaim_operation_log` in operator-configured batches under the 30-day default (PRD §6.2);
   consumption-counter partition retention (default 13 months) is handled by the storage plugin's partition
   reclamation, not by this sweeper - `inst-ret-oplog`
4. [ ] - `p1` - **RETURN** after each cycle; sweeper outage never affects counter correctness, only storage growth,
   and a killed holder's lock becomes acquirable by a survivor within one TTL - `inst-ret-return`

## 4. States (CDSL)

### Consumption Period Counter State Machine

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-state-consumption-period`

**States**: Absent, Current, SettlementWindow, Settled

**Initial State**: Absent (consumption counter rows are created lazily on first evaluate)

**Transitions**:
1. [ ] - `p1` - **FROM** Absent **TO** Current **WHEN** the first evaluate touching the Quota in the period
   materializes the row with `consumed = 0` and `highest_crossed_threshold_pct = NULL` (I13) - `inst-perst-create`
2. [ ] - `p1` - **FROM** Current **TO** SettlementWindow **WHEN** the calendar boundary passes (`now() >= period_end`);
   new single-shot mutations attribute to the successor period, credits against this period are rejected with
   `PERIOD_CLOSED` (calendar-keyed), while cross-period lease commits and rollbacks still land against this period's
   counter - `inst-perst-boundary`
3. [ ] - `p1` - **FROM** SettlementWindow **TO** Settled **WHEN** every active lease acquired in the period has
   resolved and the `period-rollover` event is enqueued; the closing consumed amount is final and rollbacks against
   the period are rejected with `PERIOD_CLOSED` (settlement-keyed) - `inst-perst-settle`

Settled is terminal: rows are retained for the operator-configured historical window (default 13 months) and reclaimed
via partition drop by the storage plugin. Allocation counters have no period dimension and no state machine here; their
single in-flight counter persists across calendar boundaries until explicitly modified.

### Idempotency Record State Machine

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-state-idempotency-record`

**States**: Absent, Active, Reclaimed

**Initial State**: Absent

**Transitions**:
1. [ ] - `p1` - **FROM** Absent **TO** Active **WHEN** a mutating storage primitive persists the record in the same
   transaction as its operation's commit (the counter mutation for an applied plan; no counter change for a persisted
   `Denied` verdict) (I1, I2), capturing the payload hash, the full Decision blob, and the Engine/Policy
   attribution - `inst-idemst-persist`
2. [ ] - `p1` - **FROM** Active **TO** Reclaimed **WHEN** the record's retention window elapses and
   `cpt-cf-quota-enforcement-algo-retention-sweep` physically reclaims it; a later submission of the same key is a new
   operation - `inst-idemst-reclaim`

While Active, an exact replay returns the stored outcome verbatim and a divergent payload returns
`IDEMPOTENCY_PAYLOAD_MISMATCH`; no transition mutates or re-issues an Active record.

## 5. Definitions of Done

### Consumption Operation Endpoints

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-consumption-endpoints`

The system **MUST** deliver `QuotaEnforcementService`
(`cpt-cf-quota-enforcement-component-quota-enforcement-service`) as the S2S entry point for debit, credit, rollback,
and preview, exposed as the four REST endpoints and `QuotaEnforcementClientV1` SDK methods. Debit and credit requests with
`amount <= 0` **MUST** be rejected with `INVALID_AMOUNT` before idempotency lookup (and, for debit, subject resolution)
or any other pipeline step, persisting nothing. Operations targeting a Quota on a `Direct`-classified metric **MUST** be rejected with
`METRIC_NOT_QUOTA_GATED` with no counter mutation and no idempotency, operation-log, or lease record. Every evaluation
operation returns either a `Decision` (HTTP 200, including `Denied` verdicts) or a `Problem`, never both;
Decision-shaped request fields are silently ignored; `denial_total` increments by closed `reason` kind on every
denial.

**Implements**:
- `cpt-cf-quota-enforcement-flow-debit`
- `cpt-cf-quota-enforcement-flow-credit`
- `cpt-cf-quota-enforcement-flow-rollback`
- `cpt-cf-quota-enforcement-flow-evaluate-preview`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`,
`cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: `POST /v1/quota-enforcement/operations/debit`, `POST /v1/quota-enforcement/operations/credit`,
  `POST /v1/quota-enforcement/operations/rollback`, `POST /v1/quota-enforcement/operations/preview`
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: `DebitRequest`, `CreditRequest`, `RollbackRequest`, `PreviewRequest`, `Decision`, `DecisionPreview`

### Evaluation Orchestrator

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-evaluation-orchestrator`

The system **MUST** establish `EvaluationOrchestrator`
(`cpt-cf-quota-enforcement-component-evaluation-orchestrator`) implementing the canonical pipeline order (authorized
subject-set intake, idempotency lookup, applicable-Quotas locked read, Policy lookup, Engine evaluate, Debit-Plan invariant
check, mutation, idempotency persist, outbox enqueue, commit) with everything after the idempotency short-circuit
inside one backend transaction. It **MUST** consume the subject-resolution extension of the projection-contracts
feature and invoke the Engine boundary of the resolution-policy-engine feature without re-specifying either; it
**MUST** capture `EvaluationContext` metadata at the locked-read step (ADR-0003), apply Debit Plans through
`apply_debit_plan` under the ADR-0002 lexicographic acquisition ordering, forward the `AccessScope` unmodified into
every storage call for `SecureConn` compilation, and emit the DESIGN §4.1 stage spans. The orchestrator **MUST NOT**
call the PDP, hold a `TypesRegistryClient`, or join a cluster election; synchronization between concurrent
instances is delegated to the storage plugin (I9).

**Implements**:
- `cpt-cf-quota-enforcement-algo-evaluation-pipeline`

**Constraints**: `cpt-cf-quota-enforcement-constraint-no-business-logic`,
`cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: no new endpoint (in-process pipeline behind the operation handlers)
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: `EvaluationContext`, `Decision`, `QuotaDebitPlan`, `MutationResult`

### Idempotency Guarantee

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-idempotency`

The system **MUST** enforce typed `IdempotencyScope { tenant_id, subject_key, operation_type, idem_key }` on every write
operation. `subject_key: IdempotencySubjectKey` is the fixed-width SHA-256 fingerprint of the canonical complete
PDP-authorized, catalogue-mapped subject set for debit, or of the owning Quota's persisted subject pair for credit and
rollback; it is never derived from a caller-selected projection or from `quota_id`.
Exact replays **MUST** return the stored `decision_blob` verbatim without re-invoking the Engine or re-binding `time`;
divergent payloads **MUST** return `IDEMPOTENCY_PAYLOAD_MISMATCH` (409) leaving the original record untouched. The
`payload_hash` is the canonical SHA-256 of the sorted-JSON payload stored as fixed-width binary; the evaluation
subject key is computed after PDP authorization and catalogue mapping, not from a caller-selected projection. The record captures
the full Decision blob plus `engine_id`, `policy_id`, and `policy_version`; `IdempotencyCache`
(`cpt-cf-quota-enforcement-component-idempotency-cache`) provides the in-process LRU (default TTL 5 s) while
persistence stays implicit in the mutating storage primitives (I1, I2). The guarantee holds for the configurable
retention window (default 24 h, per-`(tenant, metric)`); post-window replays are new operations. This mechanism is
established here and consumed unchanged by the lease-operations and batch-debit features for their operation types.

**Implements**:
- `cpt-cf-quota-enforcement-algo-idempotency-replay`
- `cpt-cf-quota-enforcement-state-idempotency-record`

**Constraints**: `cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: no new endpoint (rides every write path)
- DB: `cpt-cf-quota-enforcement-db-schema` (`idempotency_records`, `idempotency_retention_config`)
- Entities: `IdempotencyRecord`, `IdempotencyScope`, `IdempotencySubjectKey`

### Counter Shapes and Period Semantics

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-counter-period`

The system **MUST** implement the two P1 counter shapes: allocation Quotas keep a single in-flight counter with no
period (incremented by debit, decremented by credit with a floor of zero, and reversed exactly by rollback), and
consumption Quotas keep a per-period consumed counter (increased by debit, decreased by credit with a floor of zero,
and reversed exactly by rollback against the debit's attribution period) that resets to zero at every period
boundary. Periods **MUST** be the five GTS instances under `gts.cf.qe.period.type.v1~`, UTC and
calendar-aligned, with the current boundary timestamp persisted per counter; consumption rows are materialized lazily
on first evaluate; rollover **MUST** be atomic with respect to in-flight operations, attribute mutations by the
half-open `[start, end)` commit-time rule, set `highest_crossed_threshold_pct = NULL` on every new row (I13), emit the
`period-rollover` event strictly after the last commit attributed to the closing period, and suppress
`quota-counter-adjusted` and `threshold-crossed` emission for settlement-window mutations (ADR-0004). All events are
enqueued in the same transaction as the counter mutation (I11); emission rules for `threshold-crossed` and all
dispatch belong to the notifications feature.

**Implements**:
- `cpt-cf-quota-enforcement-algo-period-rollover`
- `cpt-cf-quota-enforcement-state-consumption-period`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`

**Touches**:
- API: no new endpoint (rides the evaluation paths)
- DB: `cpt-cf-quota-enforcement-db-schema` (`quota_allocation_counters`, `quota_consumption_counters`,
  `operation_log`, `notification_outbox`)
- Entities: `Counter` (allocation), `Counter` (consumption), `OperationLog`

### Retention Sweeper

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-retention-sweeper`

The system **MUST** deliver `RetentionSweeper` (`cpt-cf-quota-enforcement-component-retention-sweeper`) as a
single-leader background task under the `retention-sweeper` cluster election (`SingletonScope::RetentionSweeper`)
through the foundation coordination adapter, invoking `reclaim_expired_idempotency` per the
`idempotency_retention_config` windows (default 24 h) and `reclaim_operation_log` under the 30-day default, with
operator-configurable batch size and frequency. Sweeper liveness **MUST NOT** affect counter correctness. The
sweeper **MUST** run as a lifecycle-managed background task per the ToolKit lifecycle model: it receives a child
`CancellationToken`, its reclamation loop is cancellation-aware, and on graceful shutdown it stops starting new
batches while the adapter resigns the election (ADR-0006).
Counter-partition retention stays with the storage plugin's partition reclamation, and policy-version reclamation is
not delivered here (see section 7).

**Implements**:
- `cpt-cf-quota-enforcement-algo-retention-sweep`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`

**Touches**:
- API: `SingletonCoordinator` port (`SingletonScope::RetentionSweeper`)
- DB: `cpt-cf-quota-enforcement-db-schema` (`idempotency_records`, `operation_log`, `idempotency_retention_config`)
- Entities: `IdempotencyRecord`, `OperationLog`

### Hot-Path NFR Verification

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-nfr-verification`

The system **MUST** verify the seven NFRs this feature owns through the following hooks, adding no promise beyond the
PRD targets. Evaluation latency (`cpt-cf-quota-enforcement-nfr-evaluation-latency`) and throughput
(`cpt-cf-quota-enforcement-nfr-throughput`): the Criterion benchmark suite in `quota-enforcement/benches/` covering
single-Quota debit at 10 000 ops/s, with CI gating p95 <= 100 ms before merge. Subject scale
(`cpt-cf-quota-enforcement-nfr-subject-scale`): the 100 M-subject load benchmark holding the latency threshold. Quota
density (`cpt-cf-quota-enforcement-nfr-quota-density`): the 10-Quota cascade benchmark at 5 000 ops/s holding the
latency threshold. Availability (`cpt-cf-quota-enforcement-nfr-availability`): the end-to-end 99.95% monthly
evaluation-endpoint criterion, verified by the chaos failover drill whose gateway-failover input the foundation
feature provides. Fault tolerance (`cpt-cf-quota-enforcement-nfr-fault-tolerance`): the end-to-end RPO = 0 drill
(storage-backend restart after `Allowed`/`Denied` was returned loses no committed mutation), consuming the foundation
durable-commit input. Idempotency guarantee (`cpt-cf-quota-enforcement-nfr-idempotency-guarantee`): the simulated
retry storm at 10x normal RPS with a 5% retry rate showing zero double-count events.

**Implements**:
- `cpt-cf-quota-enforcement-algo-evaluation-pipeline`
- `cpt-cf-quota-enforcement-algo-idempotency-replay`

**Constraints**: `cpt-cf-quota-enforcement-constraint-bounded-cardinality`

**Touches**:
- API: `quota-enforcement/benches/` Criterion suite; chaos and durability drills
- Entities: benchmark and drill fixtures (no new domain entities)

## 6. Acceptance Criteria

- [ ] An `Allowed` debit applies exactly the Engine's Debit Plan: every named Quota moves by its `entry.amount`,
  unnamed Quotas never move, and the caller never observes a partially applied plan; `Denied` and every canonical
  error leave all counters unchanged (concurrency integration test)
- [ ] A debit or credit with `amount <= 0` fails with `INVALID_AMOUNT` before idempotency lookup (and, for debit,
  subject resolution): no idempotency record, no operation-log entry, no counter change
- [ ] An exact replay of a committed debit returns the original Decision verbatim (result, `debit_plan`,
  `diagnostics`, original identifiers and amounts) without re-invoking the Engine and without a second counter
  effect; a replay under a time-gated `cel` Policy at a different wall-clock time still returns the original verdict
- [ ] A replay with a divergent payload under the same scope fails with `IDEMPOTENCY_PAYLOAD_MISMATCH` (409) and
  leaves the original record untouched; the same key string used by a different tenant, subject, or operation type
  creates an independent record
- [ ] A replay after the retention window (default 24 h, honoring a per-`(tenant, metric)` override) is re-evaluated
  as a new operation
- [ ] Credit rejection arms fire before any mutation, in-tx with the row lock: unknown `quota_id` gives 404
  `UNKNOWN_QUOTA`, a cross-tenant Quota gives 403, a deactivated Quota gives 400 `QUOTA_DEACTIVATED`, and a
  consumption Quota at `time >= period_end` gives 400 `PERIOD_CLOSED` even while the settlement window is still
  draining; a successful credit floors the counter at zero and enqueues `quota-counter-adjusted` with the credited
  amount, `quota_id`, and authenticated service principal in the same transaction
- [ ] Rollback restores the counter against the original debit's `acquisition_period_id`; an unknown original key
  gives `UNKNOWN_OPERATION`; a rollback during the settlement window succeeds while one after `period-rollover`
  emission gives `PERIOD_CLOSED` with no event; a lease-commit-derived debit rolls back via the commit call's
  idempotency key; a rollback naming a credit is rejected; a successful rollback enqueues `quota-rollback-applied`
  same-tx and its replay is a no-op returning the stored Decision
- [ ] Preview runs the full pipeline against current counter state and persists nothing: no counter mutation, no held
  capacity, no idempotency record, no operation-log entry, no outbox event; the response carries the full Decision
  shape plus `preview: true` and Policy attribution in diagnostics; preview under a denied or unreachable PDP fails
  exactly like the corresponding write
- [ ] Decision-shaped fields injected into any request body are silently ignored on both fresh and replay paths
  (adversarial integration test of the PRD §3.4 trust boundary)
- [ ] The first evaluate after a period boundary materializes the new consumption row with `consumed = 0` and
  `highest_crossed_threshold_pct = NULL`; a debit committing at exactly `boundary_at` lands in the new period; the
  `period-rollover` event carries the closing consumed amount, closing cap, and new boundary, and is observed
  strictly after the last commit attributed to the closing period
- [ ] A write or preview against a Quota on a `Direct`-classified metric fails with `METRIC_NOT_QUOTA_GATED` and
  persists nothing
- [ ] One `RetentionSweeper` leader runs at a time in steady state; killing the leader makes a survivor the leader
  of `SingletonScope::RetentionSweeper` within one election TTL plus observation lag; expired idempotency and
  operation-log rows are physically reclaimed per their configured windows, and sweeper downtime changes no counter
  outcome
- [ ] Two retention sweep bodies forced to run at once (advisory overlap) complete without error, and each expired
  row is deleted exactly once
- [ ] The Criterion CI gate holds p95 <= 100 ms for single-Quota debit at 10 000 ops/s sustained; the 100 M-subject
  and 10-Quota-density benchmarks hold the same latency threshold
- [ ] The retry storm at 10x normal RPS with 5% retry rate produces zero double-count events; the RPO = 0 drill loses
  no mutation acknowledged with `Allowed` or `Denied` across a storage-backend restart; the chaos failover drill
  sustains the 99.95% evaluation-endpoint availability criterion with requests failing over to surviving replicas
- [ ] Metrics scrape shows `denial_total` labeled only by the closed `reason` enum, with no `tenant_id`,
  `subject_id`, `quota_id`, `idempotency_key`, metric, projection-type, or caller label on any instrument this
  feature touches

## 7. Additional Context (optional)

- **ADR dependencies**: ADR-0002 (acquisition ordering), ADR-0003 (metadata snapshot timing), ADR-0004
  (settlement window emit), and ADR-0007 (declarative GTS projection contracts) are accepted and load-bearing for
  this pipeline. The projection-contracts feature owns the ingress and subject-resolution contracts this pipeline
  consumes.
- **Boundary with resolution-policy-engine**: Policy selection, the Engine contract, the per-Policy timeout, and the
  Debit-Plan invariant boundary are defined there
  (`cpt-cf-quota-enforcement-algo-engine-boundary`); this feature owns the pipeline that invokes them and the atomic
  application of the resulting plan. The evaluation-latency and throughput budgets that motivated the cached
  `ValidatedConfig` and the sync no-I/O Engine contract are verified by this feature's benchmarks.
- **Upstream alignment items (tracked upstream prerequisites)**: DESIGN's rollback sequence names a
  `StorageError::OperationNotFound` variant that the closed `StorageError` enum does not declare; this document
  states only the domain outcome (canonical `NotFound` with `UNKNOWN_OPERATION`) and the enum alignment is a tracked
  upstream DESIGN item. DESIGN's `RetentionSweeper` declares no policy-version reclamation primitive although PRD
  §5.9 assigns that sweep to storage retention; the gap is already tracked by the resolution-policy-engine feature
  and no such primitive is promised here. The hot-path source of the `Direct`/`QuotaGated` classification is not
  spelled out in DESIGN beyond the closed `StorageError::MetricNotQuotaGated` variant; this document anchors the
  rejection on that variant and leaves the realisation plugin-internal. PRD §5.8 replay durability requires a `Denied`
  verdict to replay verbatim, while the PRD acceptance criteria state that a `NO_APPLICABLE_QUOTA` denial creates no
  idempotency record; the two pull in opposite directions for that denial. This document follows the explicit
  acceptance criterion, and the reconciliation is a tracked upstream PRD item.
- **Known P1 limitation**: silent Quotas emit their closing `period-rollover` only on the next operation; the P2
  active rollover scheduler (DESIGN §4.3) covers them without changing this feature's contract.
- **Rust contract notes**: the pipeline is async over the Tokio-based storage plugin while the Engine call is sync
  and I/O-free; the orchestrator holds no in-process lock across an await point and delegates cross-instance
  synchronization to storage serialization (I9), so instances are freely replicated. `IdempotencyRecord` and the
  cached Decision blobs cross task boundaries and are `Send + Sync` compatible plain data.
- **Rollout / rollback**: all components here are stateless above the storage plugin except the sweeper leadership,
  which resigns its election on shutdown and hands over within one round trip; rollout is a rolling update under the
  same schema major version. Counter rows, idempotency records, and the operation log are forward-compatible via the
  schema-versioned `decision_blob` and additive table evolution.
- **Test layering**: pipeline ordering, `INVALID_AMOUNT` fail-fast, counter floors, period attribution, and the
  state machines get unit tests; replay, payload mismatch, credit/rollback rejection arms, and the trust boundary
  get integration and adversarial tests against the storage plugin; atomicity, the retry storm, the RPO drill, and
  the availability chaos test are the drills named in section 6, not unit tests.
- **Cap guard consumption arm (tracked here)**: the reference plugin's `update_quota` evaluates the I6 cap guard on the
  merged row under the Quota's row lock; the allocation arm reads `qe_quota_allocation_counters.in_flight` today, and
  the consumption arm — the active period's consumed amount from the `quota_consumption_counters` row this feature
  materialises — lands here through the single seam the plugin leaves for it. Until then a consumption Quota's cap
  reduction is guarded by the gear's pre-check only.
- **Non-applicable review domains**: UX/accessibility is not applicable; there is no user-facing surface. Data
  protection inherits the Platform Operational Data rules from PRD §6.2; idempotency records and the operation log
  follow their configured retention windows with no additional feature-specific requirement.
