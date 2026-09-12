<!-- Created: 2026-08-26 by Constructor Tech -->

# Feature: Rate Quotas

- [ ] `p3` - **ID**: `cpt-cf-quota-enforcement-featstatus-rate-quotas-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p3` - `cpt-cf-quota-enforcement-feature-rate-quotas`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Rate Quota Create](#rate-quota-create)
  - [Rate Debit Admission](#rate-debit-admission)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Rate Draft Validation](#rate-draft-validation)
  - [Rate Admission Evaluation](#rate-admission-evaluation)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Rate Data-Model Activation and Rejection Lift](#rate-data-model-activation-and-rejection-lift)
  - [Rate Admission through the Orchestrator](#rate-admission-through-the-orchestrator)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Activates the reserved `rate` quota type per the P3 field contract of PRD §5.3: the optional `rate_spec` field on the
Quota record, acceptance of `type = rate` on the existing CRUD and evaluation endpoints, and rate admission through
the unchanged `EvaluationOrchestrator` pipeline, with `RATE_WINDOW_EXHAUSTED` denials carrying a `Retry-After` floor.
The burst mechanism (bucket vs window) stays open until the PRD §13 decision closes.

### 1.2 Purpose

P1 reserves the `rate` GTS instance and rejects it on create and update; this feature is the P3 activation that turns the
reservation into a working quota type without migrating any persisted Quota. It exists so the activation lands as the
PRD already shaped it: the field contract `(rate, burst_capacity, smoothing_window)`, the closed admission outcomes
(`Allowed` / `Denied` only, no cap-clamp), and the deterministic denial reason with a retry hint. It also states
explicitly which P1 rejection it lifts and which it leaves standing, so the earlier features' P1 wording is never
contradicted.

**Scope**: rate-quota data-model activation (the optional `rate_spec` JSON field on the `quotas` logical table, added
by schema migration at activation time per DESIGN §4.3) without migrating existing Quotas; rate admission semantics
per the P3 field contract in PRD §5.3, reusing the debit sequence of the consumption-operations feature; the explicit
transition that replaces the P1 `Unimplemented` rejection arms of the quota-lifecycle feature.

**Out of scope**: burst mechanism selection (sliding window vs token bucket vs fixed window, the smoothing axis, and
the burst-vs-lease-TTL interaction) until the PRD §13 open question is resolved; the canonical pipeline, idempotency
replay, and period machinery (consumption-operations feature, consumed unchanged); Quota CRUD flows and their
validation chain (quota-lifecycle feature, extended at exactly one arm); the batch-debit feature's rejection of
`mode = independent`, which this feature does not lift (it is a P2 transport mode unrelated to the quota type);
notification dispatch (notifications feature) and lease operations (lease-operations feature).

**Requirements**: `cpt-cf-quota-enforcement-fr-quota-type-rate-declared`

**Principles**: none of its own; the feature applies the existing evaluation principles to a new quota type
(per DECOMPOSITION §2.11).

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-quota-enforcement-actor-platform-operator` | Creates and updates rate Quotas once the type is activated |
| `cpt-cf-quota-enforcement-actor-quota-manager` | Drives rate-Quota CRUD on behalf of tenant administrators through the same lifecycle endpoints |
| `cpt-cf-quota-enforcement-actor-quota-consumer` | Sends debits and previews that evaluate against rate Quotas and receives the `RATE_WINDOW_EXHAUSTED` denials |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) (§5.3 Rate Quota Type P3 implementation contract and P1 rejection, §13 the
  burst-semantics open question)
- **Design**: [DESIGN.md](../DESIGN.md) (§4.3 future considerations: the `rate_spec` activation hook; §3.7 table
  inventory; `EvaluationOrchestrator`)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) (§2.11; §2.12 shared-component rule: the orchestrator is
  established by consumption-operations and extended here through the same contract)
- **Dependencies**: `cpt-cf-quota-enforcement-feature-consumption-operations` (the canonical pipeline, idempotency
  scope, counter machinery, and the debit sequence this feature reuses), plus transitively
  `cpt-cf-quota-enforcement-feature-quota-lifecycle` (the CRUD flows and draft-validation chain extended here)

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-quota-enforcement-usecase-create-quota` (rate-typed drafts ride the same lifecycle use case)
and `cpt-cf-quota-enforcement-usecase-debit` (rate admission rides the same debit body; a dedicated rate sequence is
a P3 DESIGN concern per DECOMPOSITION §2.11)

### Rate Quota Create

- [ ] `p3` - **ID**: `cpt-cf-quota-enforcement-flow-rate-quota-create`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-manager` (and
`cpt-cf-quota-enforcement-actor-platform-operator` through the same endpoint)

**Success Scenarios**:
- A rate draft with a complete `rate_spec` passes the full lifecycle validation chain and is persisted through the
  same transactional create as any other Quota
- Persisted `allocation` / `consumption` Quotas are untouched by the activation: no row migration, no shape change

**Error Scenarios**:
- Before activation: `type = rate` keeps returning canonical `Unimplemented` (HTTP 501, `NOT_YET_IMPLEMENTED`)
  exactly per the quota-lifecycle feature; nothing in that P1 wording changes until this feature ships
- `rate_spec` absent or missing any of the three contract fields: rejected before persistence (canonical
  `InvalidArgument`)
- A period field on a rate draft: rejected; the period specification is consumption-only per the PRD Quota definition
- Every other lifecycle error (unknown metric, catalogue miss, subject-scope violation, metadata violation) fires
  unchanged per the quota-lifecycle feature

**Steps**:
1. [ ] - `p3` - Caller sends `POST /v1/quota-enforcement/quotas` (or `PATCH /v1/quota-enforcement/quotas/{id}`) with
   `quota_type = gts.cf.qe.quota.type.v1~cf.qe.quota.rate.v1` and a `rate_spec` object; DECOMPOSITION §2.11 activates
   the existing endpoints for `type = rate`, adding no new route - `inst-rqc-request`
2. [ ] - `p3` - Run `cpt-cf-quota-enforcement-algo-rate-draft-validation` in place of the P1 rejection arms
   (`inst-qdv-rate` in `cpt-cf-quota-enforcement-algo-quota-draft-validation` and `inst-qup-rate` in
   `cpt-cf-quota-enforcement-flow-quota-update`); every other step of the lifecycle validation chain runs unchanged - `inst-rqc-validate`
3. [ ] - `p3` - DB: the same single-transaction `create_quota` / `update_quota` path of
   `cpt-cf-quota-enforcement-flow-quota-create` (create) and `cpt-cf-quota-enforcement-flow-quota-update` (update)
   persists the row with `rate_spec` stored in the optional JSON field; the `quota-changed` event and the
   operation-log entry ride the same transaction exactly as for the P1 types - `inst-rqc-persist`
4. [ ] - `p3` - **RETURN** `201` (create) or success (update) through the unchanged endpoint contract - `inst-rqc-return`

### Rate Debit Admission

- [ ] `p3` - **ID**: `cpt-cf-quota-enforcement-flow-rate-debit`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Success Scenarios**:
- A debit within the sustained rate or the burst capacity returns `Allowed` with its Debit Plan applied atomically
  through the unchanged pipeline
- A debit against an exhausted rate Quota returns the deterministic `Denied(reason = "RATE_WINDOW_EXHAUSTED")`
  verdict with a `Retry-After` floor and no counter mutation
- A replay of the same idempotency key returns the original Decision verbatim per the established replay contract

**Error Scenarios**:
- Operational failures (engine timeout, storage failure, PDP denial) surface as platform-canonical errors,
  fail-closed, exactly as on the P1 debit path
- Pre-pipeline rejections (`INVALID_AMOUNT`, ingress validation, `METRIC_NOT_QUOTA_GATED`) fire unchanged per the
  consumption-operations feature

**Steps**:
1. [ ] - `p3` - Caller sends `POST /v1/quota-enforcement/operations/debit` (the existing endpoint; preview rides
   `POST /v1/quota-enforcement/operations/preview` read-only, both activated for rate Quotas per DECOMPOSITION
   §2.11) - `inst-rda-request`
2. [ ] - `p3` - Run the canonical pipeline `cpt-cf-quota-enforcement-algo-evaluation-pipeline` unchanged: subject
   resolution, idempotency lookup, applicable-Quotas locked read, Policy lookup, Engine boundary, invariant check,
   atomic mutation, idempotency persist, outbox enqueue, commit; rate Quotas join the applicable set through the same
   locked read - `inst-rda-pipeline`
3. [ ] - `p3` - Apply `cpt-cf-quota-enforcement-algo-rate-admission` as the counter semantics for every rate Quota in
   the plan - `inst-rda-admission`
4. [ ] - `p3` - **IF** the rate Quota is exhausted at request time - `inst-rda-exhausted-if`
   1. [ ] - `p3` - **RETURN** `Denied(reason = "RATE_WINDOW_EXHAUSTED")` as an HTTP 200 verdict carrying the
      `Retry-After` floor, with no counter mutation; increment `denial_total` by the closed `reason` kind through the
      existing instrument - `inst-rda-denied`
5. [ ] - `p3` - **RETURN** the `Decision`; the only valid admission outcomes for `quota_type = rate` are `Allowed`
   and `Denied`, with operational failures surfaced as canonical errors
   (`cpt-cf-quota-enforcement-fr-quota-type-rate-declared`) - `inst-rda-return`

## 3. Processes / Business Logic (CDSL)

### Rate Draft Validation

- [ ] `p3` - **ID**: `cpt-cf-quota-enforcement-algo-rate-draft-validation`

**Input**: a `QuotaDraft` or patched Quota shape whose `quota_type` is the `rate` GTS instance

**Output**: a validated rate draft, or a canonical error before any storage call

**Steps**:
1. [ ] - `p3` - Transition rule: until this feature activates, the P1 rejection stands verbatim; `type = rate`
   create and update requests return canonical `Unimplemented` (HTTP 501, `NOT_YET_IMPLEMENTED`) per
   `cpt-cf-quota-enforcement-fr-quota-type-rate-rejection` and the quota-lifecycle arms `inst-qdv-rate` and
   `inst-qup-rate`. At activation this algorithm replaces exactly those two arms; PRD §5.3 states the P1 reject
   contract is superseded once `cpt-cf-quota-enforcement-fr-quota-type-rate-declared` is implemented - `inst-rdv-transition`
2. [ ] - `p3` - Require `rate_spec` to carry the three P3 contract fields (PRD §5.3): `rate` (the sustained
   admission rate, for example 100 ops / minute), `burst_capacity` (up to this many operations are admitted without
   backoff if capacity is full at request time), and `smoothing_window` (the period over which capacity refills at
   `rate`, typically equal to or smaller than the rate's denominator) - `inst-rdv-fields`
3. [ ] - `p3` - **IF** `rate_spec` is absent or missing any contract field - `inst-rdv-missing-if`
   1. [ ] - `p3` - **RETURN** rejection before persistence (canonical `InvalidArgument`); the field contract is the
      entire declared shape of a rate Quota, so an incomplete `rate_spec` cannot express one; concrete value-range
      constraints beyond the field shape are settled with the PRD §13 mechanism decision (section 7) - `inst-rdv-missing`
4. [ ] - `p3` - **IF** any period field is present on the rate draft - `inst-rdv-period-if`
   1. [ ] - `p3` - **RETURN** rejection; the period specification belongs to consumption types only per the PRD
      Quota definition, and rate smoothing is expressed by `rate_spec`, not by a period - `inst-rdv-period`
5. [ ] - `p3` - Run the remainder of `cpt-cf-quota-enforcement-algo-quota-draft-validation` and the rest of the
   lifecycle chain (metric validation, catalogue membership, subject scope, metadata validation) unchanged; this
   algorithm adds the rate-spec and period validation branches and replaces the two P1 rejection arms
   (`inst-qdv-rate`, `inst-qup-rate`), nothing else - `inst-rdv-chain`
6. [ ] - `p3` - Existing persisted Quotas with `type` in `allocation` / `consumption` require no migration when
   `rate` activates; the `rate_spec` field is optional and added by an additive schema migration
   (`cpt-cf-quota-enforcement-fr-quota-type-rate-rejection`, DESIGN §4.3) - `inst-rdv-coexist`
7. [ ] - `p3` - **RETURN** the validated rate draft - `inst-rdv-return`

### Rate Admission Evaluation

- [ ] `p3` - **ID**: `cpt-cf-quota-enforcement-algo-rate-admission`

**Input**: an applicable rate Quota's `rate_spec` and its persisted rate admission state, inside the pipeline's
single backend transaction

**Output**: the rate Quota's contribution to the Decision (`Allowed` or `Denied`), with the admission-state mutation
applied atomically on `Allowed` and nothing mutated on `Denied`

**Steps**:
1. [ ] - `p3` - Admit at the sustained `rate`; admit up to `burst_capacity` operations without backoff when capacity
   is full at request time; refill capacity at `rate` over the `smoothing_window` (the PRD §5.3 field contract; the
   realising mechanism, bucket vs window, is deliberately not chosen here) - `inst-radm-contract`
2. [ ] - `p3` - **IF** the rate Quota is exhausted - `inst-radm-exhausted-if`
   1. [ ] - `p3` - **RETURN** `Denied(reason = "RATE_WINDOW_EXHAUSTED")` with a `Retry-After` floor; the floor is
      advisory and clients SHOULD add randomized jitter (PRD §5.3 keeps this at SHOULD); the floor's computation rule
      and its concrete field placement in the Decision are settled by the P3 DESIGN sequence gated on PRD §13
      (section 7) - `inst-radm-denied`
3. [ ] - `p3` - Cap-clamp does not apply to rate Quotas: no `AllowedWithClamp` arm is ever produced for
   `quota_type = rate`, in P3 or later (`cpt-cf-quota-enforcement-fr-quota-type-rate-declared`) - `inst-radm-noclamp`
4. [ ] - `p3` - The admission state extends the `Counter` entity of the consumption-operations feature for rate
   windows (DECOMPOSITION §2.11); its mutation rides `apply_debit_plan` inside the same transaction as every other
   plan entry, under the same invariants (I1, I2, I11) and the ADR-0002 acquisition ordering, all owned by the
   pipeline and not re-specified here - `inst-radm-counter`
5. [ ] - `p3` - Denials mutate nothing and replay verbatim; the idempotency scope and replay behavior of
   `cpt-cf-quota-enforcement-algo-idempotency-replay` apply unchanged to rate admissions, so a replay at a different
   wall-clock time returns the original verdict without re-admission - `inst-radm-idem`
6. [ ] - `p3` - **RETURN** the rate Quota's admission outcome into the Decision - `inst-radm-return`

## 4. States (CDSL)

This feature introduces no lifecycle state machine of its own. A rate Quota uses the Active / Deactivated machine of
`cpt-cf-quota-enforcement-state-quota-lifecycle` (quota-lifecycle feature) unchanged. The rate admission state
extends the counter model of the consumption-operations feature (DECOMPOSITION §2.11); its state shape, including
any window or bucket lifecycle, is gated on the PRD §13 burst-mechanism decision and is specified with the P3 DESIGN
sequence, not here. The consumption period machine (`cpt-cf-quota-enforcement-state-consumption-period`) is not
reused: rate smoothing is expressed by `rate_spec`, not by a calendar period.

## 5. Definitions of Done

### Rate Data-Model Activation and Rejection Lift

- [ ] `p3` - **ID**: `cpt-cf-quota-enforcement-dod-rate-activation`

The system **MUST** activate the `rate` quota type on the existing Quota CRUD endpoints: one additive schema
migration adds the optional `rate_spec` JSON field to the `quotas` logical table (DESIGN §4.3), persisted
`allocation` / `consumption` Quotas **MUST NOT** require migration or shape changes, and the two P1 `Unimplemented`
rejection arms of the quota-lifecycle feature (`inst-qdv-rate`, `inst-qup-rate`) are replaced by rate-draft
validation. Until activation, the P1 rejection stands verbatim per
`cpt-cf-quota-enforcement-fr-quota-type-rate-rejection`; this feature lifts only that rejection and leaves the
batch-debit feature's `mode = independent` rejection untouched.

**Implements**:
- `cpt-cf-quota-enforcement-flow-rate-quota-create`
- `cpt-cf-quota-enforcement-algo-rate-draft-validation`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`

**Touches**:
- API: `POST /v1/quota-enforcement/quotas`, `PATCH /v1/quota-enforcement/quotas/{id}` (acceptance of `type = rate`;
  no new route)
- DB: `cpt-cf-quota-enforcement-db-schema` (`quotas` optional `rate_spec` field)
- Entities: `Quota`, `QuotaDraft`, `QuotaPatch`

### Rate Admission through the Orchestrator

- [ ] `p3` - **ID**: `cpt-cf-quota-enforcement-dod-rate-admission`

The system **MUST** extend `EvaluationOrchestrator`
(`cpt-cf-quota-enforcement-component-evaluation-orchestrator`, established by the consumption-operations feature and
extended through the same contract per DECOMPOSITION §2.12) so rate Quotas evaluate through the unchanged canonical
pipeline. For `quota_type = rate` the only valid admission outcomes **MUST** be `Allowed` and `Denied`; an exhausted
rate Quota **MUST** produce `Denied(reason = "RATE_WINDOW_EXHAUSTED")` carrying a `Retry-After` floor (advisory;
clients SHOULD add randomized jitter); cap-clamp **MUST NOT** apply. Rate admission-state mutations ride
`apply_debit_plan` in the pipeline's single transaction, denials mutate nothing, and rate denials increment the
existing `denial_total` instrument by the closed `reason` kind. No instrument is added.

**Implements**:
- `cpt-cf-quota-enforcement-flow-rate-debit`
- `cpt-cf-quota-enforcement-algo-rate-admission`

**Constraints**: `cpt-cf-quota-enforcement-constraint-no-business-logic`,
`cpt-cf-quota-enforcement-constraint-bounded-cardinality`

**Touches**:
- API: `POST /v1/quota-enforcement/operations/debit`, `POST /v1/quota-enforcement/operations/preview` (acceptance of
  rate Quotas; no new route)
- DB: `cpt-cf-quota-enforcement-db-schema` (the `Counter` extension for rate windows; shape gated on PRD §13)
- Entities: `Quota` (with `rate_spec`), `Counter` (rate extension), `Decision`

## 6. Acceptance Criteria

- [ ] After activation, `POST /v1/quota-enforcement/quotas` with `type = rate` and a complete `rate_spec` returns
  `201`, persists the row with `rate_spec` stored, and enqueues `quota-changed (created)` plus the operation-log
  entry in the same transaction
- [ ] The activation migration applies with zero updates to existing `allocation` / `consumption` rows; those Quotas
  remain readable and evaluable unchanged afterward (migration-free coexistence)
- [ ] A rate draft missing `rate_spec`, or missing any of `rate`, `burst_capacity`, `smoothing_window`, is rejected
  before persistence; a rate draft carrying any period field is rejected before persistence
- [ ] Every non-rate arm of the lifecycle validation chain fires unchanged for rate drafts: an unknown metric still
  returns `METRIC_NOT_REGISTERED`, a catalogue miss still returns `PROJECTION_NOT_RESOLVABLE`
- [ ] A debit against an exhausted rate Quota returns HTTP 200 with
  `Denied(reason = "RATE_WINDOW_EXHAUSTED")` carrying a `Retry-After` floor, mutates no counter, and increments
  `denial_total` by the `reason` kind; a debit within capacity returns `Allowed` with its plan applied atomically
- [ ] No evaluation against a rate Quota ever produces an outcome other than `Allowed`, `Denied`, or a canonical
  error; no clamp arm appears for `quota_type = rate`
- [ ] An exact replay of a committed rate debit returns the stored Decision verbatim with no second admission and no
  counter effect, including a replay at a different wall-clock time (the established idempotency contract, exercised
  with rate inputs)

## 7. Additional Context (optional)

- **Gated on PRD §13 (tracked upstream prerequisites)**: the burst mechanism (token bucket vs sliding window vs
  fixed window), the smoothing axis (per-tenant vs per-region), and the burst-vs-lease-TTL interaction are open in
  PRD §13 and are resolved when P3 implementation begins. Downstream of that decision sit: the `Retry-After` floor
  computation rule and its concrete placement in the Decision shape, value-range constraints on the three
  `rate_spec` fields beyond their presence, the concrete row shape of the rate counter extension, and the dedicated
  rate admission sequence (a P3 DESIGN concern per DECOMPOSITION §2.11). This document pins only what PRD §5.3
  already pins: the field contract, the closed outcomes, and the denial reason.
- **Transition summary**: lifted at activation: the canonical `Unimplemented` rejection of `type = rate` on Quota
  create and update (quota-lifecycle `inst-qdv-rate`, `inst-qup-rate`,
  `cpt-cf-quota-enforcement-dod-rate-rejection`), per the PRD §5.3 supersession clause. Not lifted: the batch-debit
  feature's `mode = independent` rejection (a deferred P2 transport mode, unrelated to the quota type). Until this
  feature ships, both P1 rejections stand exactly as their owning documents word them.
- **Unspecified upstream (tracked upstream PRD items)**: whether `cap` and `notification_thresholds` apply to rate
  Quotas is stated nowhere in PRD or DESIGN; the P3 field contract names only the three `rate_spec` fields. This
  document does not invent an answer; the applicability call is tracked for the PRD update that closes §13.
- **Rust contract notes**: rate admission adds no new trait and no new shared mutable state; the admission state
  lives in storage rows mutated inside the pipeline's transaction, so cross-instance synchronization stays delegated
  to the storage plugin's serialization (I9) and the orchestrator still holds no lock across await points on the hot
  path. `rate_spec` rides the existing `Quota` plain-data entity and stays `Send + Sync` compatible.
- **Rollout / rollback**: activation is one additive schema migration plus the replacement of two validation arms;
  rolling update under the same schema major version. Rollback before any rate Quota exists is trivial; rollback
  after rate Quotas exist re-enables the create/update rejection while existing rate rows stay readable, since the
  P1 rejection contract governs only create and update requests.
- **Test layering**: the draft-validation branch and the coexistence migration get unit and migration tests; the
  admission outcomes, denial telemetry, and replay behavior get integration tests through the existing pipeline
  harness; the batch `mode = independent` criterion reuses the batch-debit suite as a regression guard.
- **Non-applicable review domains**: UX/accessibility is not applicable; there is no user-facing surface. Data
  protection: `rate_spec` carries numeric admission parameters only, Platform Operational Data per PRD §6.2, with no
  feature-specific handling added.
