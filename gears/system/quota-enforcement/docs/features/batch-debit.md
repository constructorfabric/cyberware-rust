<!-- Created: 2026-08-26 by Constructor Tech -->

# Feature: Batch Debit

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-featstatus-batch-debit-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-quota-enforcement-feature-batch-debit`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Batch Debit](#batch-debit)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Batch Envelope Evaluation](#batch-envelope-evaluation)
- [4. States (CDSL)](#4-states-cdsl)
  - [Batch Envelope State Machine](#batch-envelope-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Batch Debit Endpoint and Envelope Validation](#batch-debit-endpoint-and-envelope-validation)
  - [Atomic Envelope Evaluation](#atomic-envelope-evaluation)
  - [Envelope Idempotency and Batch Timeout](#envelope-idempotency-and-batch-timeout)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Implements the multi-item debit envelope `POST /v1/quota-enforcement/operations/batch-debit` for single logical
operations that consume several metrics at once: atomic all-or-nothing evaluation in which each item observes the
running batch state, envelope idempotency, the batch-level evaluation timeout, maximum-batch-size enforcement, and the
P1 rejection of the deferred `independent` mode.

### 1.2 Purpose

A single logical operation often consumes several metrics simultaneously (an LLM call consumes input tokens, output
tokens, and compute seconds). Single-item idempotency keys do not compose naturally for such multi-metric debits, and a
partial application leaves counters inconsistent with the operation the caller actually performed, forcing brittle
compensating logic at every consumer (PRD §5.7 rationale). Atomic mode preserves the all-or-nothing semantics of that
single logical operation: admit or deny the whole set. This feature extends the `EvaluationOrchestrator` pipeline
established by the consumption-operations feature with an envelope evaluation: one locked read over the union of
applicable Quotas, sequential per-item Engine evaluation over running batch state, and one all-or-nothing mutation via
`apply_batch_debit`. The bulk-independent transport optimization stays P2; the P1 answer for unrelated items is N
parallel single-item debits.

**Scope**: the `apply_batch_debit` storage primitive with envelope and per-item idempotency keys; `mode = atomic`
semantics with per-item Decisions reported in submission order; the batch-level evaluation timeout superseding
per-Policy timeouts; maximum batch size enforcement; rejection of `mode = independent` with `NOT_YET_IMPLEMENTED`.

**Out of scope**: partial-success `independent` mode (P2 per PRD §5.7); the canonical pipeline, the idempotency replay
mechanism, subject resolution, and the Engine boundary, all owned by earlier features and consumed here unchanged
(DECOMPOSITION §2.12).

**Requirements**: `cpt-cf-quota-enforcement-fr-batch-debit`

**Principles**: none of its own; the feature reuses the pipeline principles owned by earlier features (per
DECOMPOSITION §2.7).

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-quota-enforcement-actor-quota-consumer` | Submits batch debits with an envelope idempotency key, a required `mode`, and per-item idempotency keys |
| `cpt-cf-quota-enforcement-actor-storage-backend` | Serializes the envelope transaction and provides the all-or-nothing commit or rollback of the union mutation |
| `cpt-cf-quota-enforcement-actor-monitoring-system` | Scrapes the existing pipeline instruments, which this feature reuses unchanged with no new instrument or label dimension |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) (§5.7 batch debit, §5.8 idempotency and the envelope subject slot, §3.2 metric
  classification and default deny, §3.4 Decision contract and trust boundary)
- **Design**: [DESIGN.md](../DESIGN.md) (`QuotaEnforcementService`, `EvaluationOrchestrator`, the storage
  counter-mutation group with `apply_batch_debit`, the `BatchItem` in-memory entity, §3.3 error model,
  `cpt-cf-quota-enforcement-seq-batch-debit`)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) (§2.7)
- **ADR**: [ADR-0002 Acquisition ordering](../ADR/0002-cpt-cf-quota-enforcement-adr-acquisition-ordering.md),
  [ADR-0003 Metadata snapshot timing](../ADR/0003-cpt-cf-quota-enforcement-adr-metadata-snapshot-timing.md),
  [ADR-0007 Declarative GTS projection contracts](../ADR/0007-cpt-cf-quota-enforcement-adr-projection-contracts.md)
  (`cpt-cf-quota-enforcement-adr-projection-contracts`, status: accepted; the per-item ingress and resolution
  contracts follow that ADR)
- **Dependencies**: `cpt-cf-quota-enforcement-feature-consumption-operations` (the `EvaluationOrchestrator` pipeline,
  the idempotency replay mechanism, and the counter shapes this envelope mutates), plus transitively
  `cpt-cf-quota-enforcement-feature-quota-lifecycle`, `cpt-cf-quota-enforcement-feature-resolution-policy-engine`,
  `cpt-cf-quota-enforcement-feature-projection-contracts`, and `cpt-cf-quota-enforcement-feature-foundation`

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-quota-enforcement-usecase-batch-debit`

### Batch Debit

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-batch-debit`

Realises `cpt-cf-quota-enforcement-seq-batch-debit`.

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Success Scenarios**:
- Every item evaluates to `Allowed`: the union of all per-item `debit_plan`s is applied atomically and the caller
  receives a batch-level `Allowed` `BatchDecision` with per-item Decisions preserving submission order
- Any item evaluates to `Denied`: the caller receives a batch-level `Denied` verdict as an HTTP 200 body with per-item
  statuses for diagnostic purposes only and every counter unchanged
- A replay of the same envelope idempotency key returns the stored `BatchDecision` with no second counter effect

**Error Scenarios**:
- Any item with `amount <= 0`: envelope-level `INVALID_AMOUNT` naming the offending item index, regardless of `mode`;
  nothing is persisted
- Batch larger than the operator-configured maximum: `BULK_TOO_LARGE` before any item is evaluated
- `mode = independent`: `NOT_YET_IMPLEMENTED` (canonical `Unimplemented`, 501) until the P2 mode ships
- Batch-level timeout fires: canonical `DeadlineExceeded` with `reason = "BATCH_TIMEOUT"` for the whole batch, no
  counter mutations
- Any item fails with a canonical error (including `METRIC_NOT_QUOTA_GATED`): the envelope fails with that canonical
  error, fail-closed, no counter mutation

**Steps**:
1. [ ] - `p1` - Caller sends `POST /v1/quota-enforcement/operations/batch-debit` with a `BatchDebitRequest` carrying
   the envelope idempotency key, the required `mode` field, and the items; each item carries caller-supplied
   attribution, one operation-level metadata object, an optional resource, a positive integer `amount`, and its own idempotency key for individual identification
   (`cpt-cf-quota-enforcement-fr-batch-debit`); foundation admission
   (`cpt-cf-quota-enforcement-flow-authorized-admission`) and per-item ingress validation
   (`cpt-cf-quota-enforcement-flow-ingress-validation`, which covers each batch item) have already run - `inst-bde-request`
2. [ ] - `p1` - **IF** any item carries `amount <= 0` - `inst-bde-amount-if`
   1. [ ] - `p1` - **RETURN** an envelope-level `INVALID_AMOUNT` (`DomainError::InvalidAmount`, canonical
      `InvalidArgument`) naming the offending item index, regardless of `mode`, before idempotency lookup or any other
      pipeline step; no envelope or per-item idempotency record is persisted, no operation-log entry is written, no
      counter is mutated - `inst-bde-amount`
3. [ ] - `p1` - Consume each item's PDP-authorized, catalogue-mapped subject set via
   `cpt-cf-quota-enforcement-algo-subject-resolution` (projection-contracts feature; consumed, never re-specified),
   mirroring the canonical pipeline (`cpt-cf-quota-enforcement-algo-evaluation-pipeline`), which maps subjects
   before its idempotency lookup; canonicalize the sorted, deduplicated union of all item sets into the envelope's
   `IdempotencySubjectKey` per `cpt-cf-quota-enforcement-fr-idempotency` - `inst-bde-resolve`
4. [ ] - `p1` - DB: `lookup_idempotency(scope)` with the envelope's typed `IdempotencyScope`; on an exact replay
   **RETURN** the stored `BatchDecision` verbatim per
   `cpt-cf-quota-enforcement-algo-idempotency-replay` (consumption-operations feature), which this feature consumes
   unchanged for its operation type; this replay short-circuit takes precedence over the `mode` and size checks, so an
   exact replay returns the stored outcome even after an operator lowers the maximum batch size (PRD §5.7 replay
   guarantee); a divergent payload returns `IDEMPOTENCY_PAYLOAD_MISMATCH` (409) - `inst-bde-idem`
5. [ ] - `p1` - **IF** `mode = independent` - `inst-bde-mode-if`
   1. [ ] - `p1` - **RETURN** `NOT_YET_IMPLEMENTED` (`DomainError::NotYetImplemented`, canonical `Unimplemented`, 501);
      partial-success bulk semantics are deferred to P2 per PRD §5.7 - `inst-bde-mode`
6. [ ] - `p1` - **IF** the item count exceeds the operator-configurable maximum batch size (default 100 items per
   batch) - `inst-bde-size-if`
   1. [ ] - `p1` - **RETURN** `BULK_TOO_LARGE` (`DomainError::BulkTooLarge`, canonical `InvalidArgument`) before any
      item is evaluated - `inst-bde-size`
7. [ ] - `p1` - Run `cpt-cf-quota-enforcement-algo-batch-envelope-evaluation` under the batch-level evaluation
   timeout - `inst-bde-envelope`
8. [ ] - `p1` - **IF** the envelope outcome is `Denied` - `inst-bde-denied-if`
   1. [ ] - `p1` - **RETURN** the batch-level `Denied` verdict as an HTTP 200 `BatchDecision` body with per-item
      statuses included for diagnostic purposes only and every counter unchanged; `Denied` is a deterministic over-cap
      signal, so a retry is futile until a credit or period rollover (PRD §5.7) - `inst-bde-denied`
9. [ ] - `p1` - **RETURN** the batch-level outcome with the per-item array preserving submission order; a
   `BatchDecision` (HTTP 200) and a `Problem` (canonical error) are mutually exclusive outcomes, and a retry of a
   canonical error under the same envelope key is replay-safe; the SDK path is
   `QuotaEnforcementClientV1::batch_debit(req)` returning `BatchDecision` - `inst-bde-return`

## 3. Processes / Business Logic (CDSL)

### Batch Envelope Evaluation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-batch-envelope-evaluation`

**Input**: a validated `BatchDebitRequest` with `mode = atomic` whose envelope idempotency lookup missed, the per-item
applicable-subject sets already resolved in the flow (`inst-bde-resolve`), `SecurityContext`, the `AccessScope`
returned by `PolicyEnforcer`, the published `ProjectionContractCatalog`

**Output**: a committed `BatchDecision` with all side effects applied atomically, or a canonical error with no counter
mutation

**Steps**:
1. [ ] - `p1` - Consume each item's complete applicable-subject set as resolved in the flow before the envelope
   idempotency lookup (`inst-bde-resolve`, via `cpt-cf-quota-enforcement-algo-subject-resolution`,
   projection-contracts feature; consumed, never re-specified); no further resolution runs here - `inst-bev-resolve`
2. [ ] - `p1` - DB: begin the single backend transaction; compute the union of applicable Quotas across all items and
   run one locked read on that union in lexicographic `quota_id` order
   (`cpt-cf-quota-enforcement-adr-acquisition-ordering`); this union locked read replaces the single-operation
   locked-read stage of `cpt-cf-quota-enforcement-algo-evaluation-pipeline`, `EvaluationContext` metadata is captured
   at this locked-read step (ADR-0003), and lazy period-row materialization runs here when a boundary was crossed - `inst-bev-union`
3. [ ] - `p1` - Arm the batch-level evaluation timeout: a single operator-configurable flat duration
   (deployment-default 250 ms) that supersedes per-Policy Engine timeouts for the batch as a whole
   (`cpt-cf-quota-enforcement-fr-batch-debit`); DESIGN §3.3 models it as the envelope tokio timeout
   (`DomainError::BatchTimeout`); a tokio timeout is observed only at an await point and the per-item Engine call is
   synchronous and I/O-free, so the tokio timeout guards the await-bearing stages while the item loop checks the
   armed deadline cooperatively between items (step 4) - `inst-bev-timeout`
4. [ ] - `p1` - **FOR EACH** item in submission order - `inst-bev-loop`
   1. [ ] - `p1` - Compare the current instant against the armed deadline; **IF** the deadline is exceeded, stop the
      loop and take the timeout branch (step 5) with `DomainError::BatchTimeout` - `inst-bev-deadline`
   2. [ ] - `p1` - Invoke the Engine boundary (`cpt-cf-quota-enforcement-algo-engine-boundary`,
      resolution-policy-engine feature) unchanged against the item's own applicable-Quotas set, with the item's
      evaluation observing counter state that reflects the application of every previously-evaluated item in the same
      batch (PRD §5.7 normative; otherwise the Engine cannot produce a correct Debit Plan) - `inst-bev-evaluate`
   3. [ ] - `p1` - **IF** the item targets a Quota whose metric is classified `Direct`, the item fails with
      `METRIC_NOT_QUOTA_GATED` exactly as on the single-item pipeline, and the envelope fails with that canonical
      error (PRD §3.2) - `inst-bev-gated`
   4. [ ] - `p1` - Record the item's `Decision` or canonical error at the item's submission index in the per-item
      array - `inst-bev-record`
5. [ ] - `p1` - **IF** the armed deadline is exceeded before evaluation completes (the cooperative check trips between
   items, or the tokio timeout fires at an await-bearing stage) - `inst-bev-fire-if`
   1. [ ] - `p1` - Roll back the entire transaction and **RETURN** a canonical `DeadlineExceeded` error with
      `reason = "BATCH_TIMEOUT"` carried in the envelope, with no counter mutations and no idempotency record; the
      caller retries with the same envelope key (replay-safe) or, if persistent, with a smaller batch under a new
      envelope key - `inst-bev-fire`
6. [ ] - `p1` - **IF** every item evaluates to `Allowed` - `inst-bev-apply-if`
   1. [ ] - `p1` - DB: `apply_batch_debit(envelope_idem_key, items, events)`: apply the union of all per-item
      `debit_plan`s atomically, persist the envelope idempotency record (the full `BatchDecision` blob; Engine and
      Policy attribution is recorded per item, because a multi-metric batch can resolve a different Policy per item,
      and the record-level multi-Policy attribution shape is a tracked upstream DESIGN item, section 7), append the
      operation-log `batch_debit` entry, and enqueue the outbox events, all inside the same transaction (I11); commit;
      concurrent multi-Quota mutation follows the ADR-0002 acquisition ordering - `inst-bev-apply`
7. [ ] - `p1` - **IF** any item evaluated to `Denied` - `inst-bev-denied-if`
   1. [ ] - `p1` - DB: commit the `Denied` outcome through `apply_batch_debit(envelope_idem_key, items, events)` with
      an empty union: no counter moves, and the envelope idempotency record (the `Denied` `BatchDecision` with
      per-item statuses) persists inside the committed transaction, matching the single-debit precedent where a
      `Denied` Decision persists its idempotency record with an empty plan, so an exact replay returns the original
      verdict (PRD §5.8; the DESIGN-sequence alignment is a tracked upstream item, section 7) - `inst-bev-denied`
8. [ ] - `p1` - **IF** any item failed with a canonical error - `inst-bev-error-if`
   1. [ ] - `p1` - Roll back the entire envelope, persist nothing, and **RETURN** the canonical error (fail-closed,
      no counter mutation); the same-envelope-key retry is re-evaluated - `inst-bev-error`
9. [ ] - `p1` - **RETURN** the `BatchDecision`; the orchestrator emits the established pipeline stage spans and
   counters unchanged, and no new instrument, label dimension, or per-item attribution is introduced (PRD §5.16
   closed catalogue) - `inst-bev-return`

## 4. States (CDSL)

### Batch Envelope State Machine

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-state-batch-envelope`

The lifecycle of one envelope within its single backend transaction; `BatchItem` is an in-memory entity per DESIGN and
no new persisted state is introduced.

**States**: Validating, Evaluating, Applied, DeniedRecorded, RolledBack

**Initial State**: Validating

**Transitions**:
1. [ ] - `p1` - **FROM** Validating **TO** Evaluating **WHEN** the amount validation passes, the envelope idempotency
   lookup misses, and the `mode` and size checks pass; an exact replay short-circuits to the stored `BatchDecision`
   without entering Evaluating - `inst-best-enter`
2. [ ] - `p1` - **FROM** Evaluating **TO** Applied **WHEN** every item evaluates to `Allowed` within the batch-level
   timeout; the union of per-item `debit_plan`s, the envelope idempotency record, the operation-log entry, and the
   outbox events commit in one transaction (I11) - `inst-best-applied`
3. [ ] - `p1` - **FROM** Evaluating **TO** DeniedRecorded **WHEN** any item evaluates to `Denied` within the
   batch-level timeout; the transaction commits through `apply_batch_debit` with an empty union, so no counter moves
   and only the envelope idempotency record (the `Denied` `BatchDecision`) persists - `inst-best-denied`
4. [ ] - `p1` - **FROM** Evaluating **TO** RolledBack **WHEN** any item fails with a canonical error or the
   batch-level timeout fires; the transaction rolls back, no counter mutation survives, and no envelope idempotency
   record persists - `inst-best-rollback`

Applied, DeniedRecorded, and RolledBack are terminal for the envelope; the idempotency record persisted by the Applied
and DeniedRecorded outcomes then follows the Idempotency Record state machine owned by the consumption-operations
feature.

## 5. Definitions of Done

### Batch Debit Endpoint and Envelope Validation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-batch-debit-endpoint`

The system **MUST** deliver `POST /v1/quota-enforcement/operations/batch-debit` on `QuotaEnforcementService` (per the
DESIGN service surface; see the section 7 note on the DECOMPOSITION sharer list) and the SDK method
`QuotaEnforcementClientV1::batch_debit(req: BatchDebitRequest)` returning `BatchDecision`. The `mode` field is
required; `mode = independent` **MUST** be rejected with `NOT_YET_IMPLEMENTED` (canonical `Unimplemented`, 501) until
the P2 mode ships. Batches over the operator-configurable maximum size (default 100 items) **MUST** be rejected with
`BULK_TOO_LARGE` before any item is evaluated. If any item carries `amount <= 0`, the envelope **MUST** be rejected
with an envelope-level `INVALID_AMOUNT` naming the offending item index, regardless of `mode`, before idempotency
lookup or any other pipeline step, persisting nothing. The response **MUST** carry the batch-level outcome plus the
per-item array preserving submission order; a `BatchDecision` and a `Problem` are mutually exclusive, and
Decision-shaped request fields are silently ignored per the PRD §3.4 trust boundary.

**Implements**:
- `cpt-cf-quota-enforcement-flow-batch-debit`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`,
`cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: `POST /v1/quota-enforcement/operations/batch-debit`
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: `BatchDebitRequest`, `BatchItem`, `BatchDecision`

### Atomic Envelope Evaluation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-batch-atomic-envelope`

The system **MUST** extend `EvaluationOrchestrator`
(`cpt-cf-quota-enforcement-component-evaluation-orchestrator`, established by the consumption-operations feature) with
the envelope evaluation: one locked read over the union of applicable Quotas across all items under the ADR-0002
lexicographic acquisition ordering, sequential per-item evaluation through the Engine boundary with each item
observing counter state that reflects every previously-evaluated item in the same batch, and all-or-nothing
application via `apply_batch_debit` with the envelope idempotency record, the operation-log entry, and the outbox
events persisted in the same transaction (I11). Any `Denied` item, any canonical error, or a timeout **MUST** leave
every counter unchanged (fail-closed): the `Denied` outcome commits only its envelope idempotency record through
`apply_batch_debit` with an empty union, while a canonical error or a timeout rolls the transaction back and persists
nothing. The extension **MUST** consume subject resolution
(projection-contracts feature) and the Engine boundary (resolution-policy-engine feature) unchanged and never
re-specify them; the `AccessScope` is forwarded unmodified into every storage call, the orchestrator never calls the
PDP, and synchronization between concurrent instances stays delegated to the storage plugin (I9).

**Implements**:
- `cpt-cf-quota-enforcement-algo-batch-envelope-evaluation`
- `cpt-cf-quota-enforcement-state-batch-envelope`

**Constraints**: `cpt-cf-quota-enforcement-constraint-no-business-logic`,
`cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: no new endpoint (extends the in-process pipeline behind the batch-debit handler)
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: `BatchItem`, `EvaluationContext`, `Decision`, `BatchDecision`, `MutationResult`

### Envelope Idempotency and Batch Timeout

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-batch-idempotency-timeout`

The system **MUST** enforce envelope idempotency under the established scope
`IdempotencyScope { tenant_id, subject_key, operation_type, idem_key }`, where `subject_key` fingerprints the canonical
sorted, deduplicated union of every item's complete authorized, catalogue-mapped applicable-subject set. An exact envelope replay
**MUST** return the stored `BatchDecision` verbatim without re-invoking the Engine and without a second counter effect;
a divergent payload **MUST** return `IDEMPOTENCY_PAYLOAD_MISMATCH` (409) leaving the original record untouched. Each
item **MUST** carry its own idempotency key for individual identification per
`cpt-cf-quota-enforcement-fr-batch-debit` (no per-item replay semantics are defined or promised; see section 7 for the
DESIGN alignment note). The system **MUST** enforce the batch-level evaluation timeout as a single
operator-configurable flat duration (deployment-default 250 ms) that supersedes per-Policy timeouts for the batch as a
whole; a timeout fire **MUST** surface a canonical `DeadlineExceeded` error with `reason = "BATCH_TIMEOUT"` carried in
the envelope, with no counter mutations, and a retry under the same envelope key is replay-safe.

**Implements**:
- `cpt-cf-quota-enforcement-flow-batch-debit`
- `cpt-cf-quota-enforcement-algo-batch-envelope-evaluation`

**Constraints**: `cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: no new endpoint (rides the batch-debit path)
- DB: `cpt-cf-quota-enforcement-db-schema` (`idempotency_records`)
- Entities: `IdempotencyRecord`, `BatchDecision`

## 6. Acceptance Criteria

- [ ] An all-`Allowed` atomic batch applies exactly the union of the per-item `debit_plan`s in one transaction: every
  named Quota moves by its planned amount, unnamed Quotas never move, and the response is a batch-level `Allowed`
  `BatchDecision` with per-item Decisions preserving submission order (concurrency integration test)
- [ ] Running batch state is observed per item, verified end-to-end with the PRD §9 fixture: on
  `Quota Q (cap=800, consumed=0)`, `batch_debit(mode=atomic, items=[{metric=M, amount=500}, {metric=M, amount=500}])`
  returns batch-level `Denied` (item 1 `Allowed` against remaining 800, item 2 `Denied` against remaining 300) and no
  mutation is persisted
- [ ] Any `Denied` item yields a batch-level `Denied` verdict as an HTTP 200 body with per-item statuses for
  diagnostic purposes only and every counter unchanged; an exact replay of that envelope key returns the original
  `Denied` verdict
- [ ] Any item with `amount <= 0` fails the envelope with `INVALID_AMOUNT` naming the offending item index, regardless
  of `mode`, before idempotency lookup: no envelope or per-item idempotency record, no operation-log entry, no counter
  change
- [ ] A batch over the maximum size (default 100, operator-configurable) fails with `BULK_TOO_LARGE` before any item
  is evaluated; `mode = independent` fails with `NOT_YET_IMPLEMENTED` (501)
- [ ] An exact replay of a committed envelope returns the stored `BatchDecision` verbatim without re-invoking the
  Engine and without a second counter effect; a divergent payload under the same envelope scope fails with
  `IDEMPOTENCY_PAYLOAD_MISMATCH` (409) leaving the original record untouched
- [ ] A batch whose evaluation exceeds the batch-level timeout (a single operator-configurable flat duration,
  deployment-default 250 ms, superseding per-Policy timeouts) fails with canonical `DeadlineExceeded` and
  `reason = "BATCH_TIMEOUT"` in the envelope, mutating nothing; a retry with the same envelope key is re-evaluated
  (replay-safe)
- [ ] An item targeting a Quota on a `Direct`-classified metric fails the envelope with `METRIC_NOT_QUOTA_GATED` and
  persists nothing
- [ ] Decision-shaped fields injected into a batch request or any of its items are silently ignored on both fresh and
  replay paths (adversarial integration test of the PRD §3.4 trust boundary)
- [ ] The feature reuses the existing pipeline instruments and stage spans unchanged: no new metric instrument, label
  dimension, or per-item attribution appears in the scrape (PRD §5.16 closed catalogue)

## 7. Additional Context (optional)

- **Pipeline extension boundary** (DECOMPOSITION §2.12): the `EvaluationOrchestrator` is established by the
  consumption-operations feature; this feature only extends it. The envelope evaluation reuses the canonical pipeline
  stages; the differences are the union locked read (one locked read over all items' applicable Quotas) and the
  sequential per-item Engine loop. Subject resolution, the Engine boundary, and the idempotency replay algorithm are
  consumed unchanged and never re-specified here.
- **Upstream alignment items (tracked upstream prerequisites)**:
  - PRD §5.7 requires each item to carry its own idempotency key, while DESIGN's `BatchItem` entity models the
    per-item key as `optional_per_item_idem_key`. This document follows the PRD requirement; the DESIGN entity
    alignment is a tracked upstream DESIGN item. PRD assigns the per-item key "individual identification" only, so no
    per-item replay semantics are promised.
  - PRD §5.7 ("replay of the same envelope key returns the original outcome") together with the §5.8 replay-durability
    rule points to persisting the `Denied` envelope outcome, matching the single-debit precedent where a `Denied`
    Decision persists its idempotency record with an empty plan. The DESIGN batch sequence draws the idempotency
    persist only in the all-items-succeed branch. This document follows the PRD reading and commits the `Denied`
    record through `apply_batch_debit` with an empty union; the sequence alignment, including the transactional
    placement of the `Denied` persist, is a tracked upstream DESIGN item.
  - PRD §5.8 defines idempotency-record attribution as the singular `engine_id`, `policy_id`, `policy_version` of one
    Decision, while Policy selection is per metric scope and one envelope can span several Policies. This document
    records attribution per item inside the `BatchDecision` blob; the record-level multi-Policy attribution shape is a
    tracked upstream DESIGN item.
  - PRD §5.16 and the DESIGN cardinality constraint permit an `operation` label dimension with `batch_debit` among its
    values, but no instrument in the DESIGN §4.1 catalogue carries an `operation` label, and attaching one to the
    existing pipeline instruments would add a label dimension. This document therefore claims no
    `operation = batch_debit` labeling; an `operation` dimension on the pipeline instruments is a tracked upstream
    DESIGN item.
  - DESIGN places `batch_debit` on `QuotaEnforcementService` and the DECOMPOSITION §2.7 API list carries the endpoint,
    but the §2.12 sharer list for that component names only consumption-operations and snapshot-reads. This document
    anchors delivery on the endpoint and the §2.7 allocation; the sharer-list alignment is a tracked upstream
    DECOMPOSITION item.
  - The DESIGN sequence's annotation places the per-item evaluation loop inside the `apply_batch_debit` call, while
    the `EvaluationOrchestrator` component contract keeps Engine invocation in the orchestrator and counter-mutation
    mechanics in the storage plugin. This document keeps Engine invocation with the orchestrator, running inside the
    single storage transaction; the diagram alignment is a tracked upstream DESIGN item.
  - DESIGN pins no dedicated storage location for the two operator-configurable batch settings (maximum batch size and
    batch-level timeout); bootstrap seeds "default config-table rows" generically. No new config table is defined
    here.
- **Deliberately unpinned behavior**: PRD §5.7 pins ordering only for `INVALID_AMOUNT`: it fires before any item
  evaluation and before the idempotency lookup. The timing of the size and `mode` checks is this document's
  implementation choice, constrained so that replay is not affected: they run after the replay short-circuit and
  before any item evaluation. The PRD also does not state whether items after the first failing item are still
  evaluated for diagnostics; that choice is left to the implementation, and no test may depend on it.
- **No NFR ownership**: the DECOMPOSITION NFR allocation assigns the hot-path NFRs to the consumption-operations
  feature; this feature adds no NFR promise. The worst-case batch latency bound follows from
  `cpt-cf-quota-enforcement-fr-batch-debit` itself (the flat timeout together with the maximum batch size); adaptation
  to load is the caller's concern (client-side retry and batch splitting).
- **Rust contract notes**: the batch-level deadline is enforced in two parts, because a tokio timeout is observed only
  at an await point and the Engine call is synchronous and I/O-free: a tokio timeout wraps the await-bearing stages of
  the envelope evaluation future, and the synchronous per-item loop checks the armed deadline between items
  (`inst-bev-deadline`), returning `DomainError::BatchTimeout` cooperatively. When the deadline trips, the transaction
  is dropped un-committed and the backend rolls the envelope back, so cancellation safety rests
  on the transaction lifecycle rather than on ad-hoc cleanup. Per-item evaluation is strictly sequential inside one
  task, and the running batch state is confined to the transaction and the orchestrator-held `EvaluationContext`, so
  no shared mutable state crosses a task boundary and no in-process lock is held across an await point. The Engine
  call itself stays synchronous and I/O-free per the resolution-policy-engine feature. `BatchDebitRequest`,
  `BatchItem`, and `BatchDecision` are Send + Sync compatible plain data.
- **Test layering**: envelope validation arms and their normative "before" clauses get unit tests; running batch
  state, envelope replay, payload mismatch, the `Direct`-metric rejection, and the trust boundary get integration and
  adversarial tests against the storage plugin; the atomicity concurrency test named in section 6 is an integration
  test against the storage plugin, and the timeout drill named in section 6 is an end-to-end test.
- **Non-applicable review domains**: UX/accessibility is not applicable; there is no user-facing surface. Data
  protection inherits the PRD §6.2 operational-data rules through the idempotency and operation-log retention owned by
  the consumption-operations feature; this feature adds no new data category.
