<!-- Created: 2026-08-26 by Constructor Tech -->

# Feature: Lease Operations

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-featstatus-lease-operations-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-quota-enforcement-feature-lease-operations`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Acquire Lease](#acquire-lease)
  - [Commit Lease](#commit-lease)
  - [Release Lease](#release-lease)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Lazy Semantic Expiry](#lazy-semantic-expiry)
  - [Lease Sweeper Reclamation](#lease-sweeper-reclamation)
- [4. States (CDSL)](#4-states-cdsl)
  - [Lease State Machine](#lease-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Lease Operation Endpoints](#lease-operation-endpoints)
  - [Atomic Multi-Quota Acquisition and Guard Rails](#atomic-multi-quota-acquisition-and-guard-rails)
  - [Acquisition-Period and Validity Attribution](#acquisition-period-and-validity-attribution)
  - [Lazy Expiry Enforcement](#lazy-expiry-enforcement)
  - [Lease Sweeper](#lease-sweeper)
  - [Recovery NFR Verification](#recovery-nfr-verification)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Implements the two-phase lease protocol: atomic multi-Quota hold acquisition with a caller-supplied TTL, commit with
unused-capacity return, release, and TTL auto-release. Correctness rests on the lazy-expiry semantic that every read
and write path enforces on its own; the `LeaseSweeper` background task is only the physical reclamation tier.

### 1.2 Purpose

Long-running operations do not know their consumption up front: a 30-minute compute job may abort, and a caller that
debits the worst case up front must invent its own compensation logic. The lease primitive holds capacity for a bounded
TTL, converts to a debit on commit for the realized amount, and returns capacity on release or on TTL expiry. The
guarantee earned at acquisition time is durable: commit lands on the acquisition period and survives validity and
period boundaries, and an expired lease can never block new work, even when the sweeper is down. Guard rails (TTL
bounds, the per-`(tenant, metric)` active-lease cap, and the acquisition contention timeout) keep hold traffic bounded
and its failure modes deterministic and observable.

**Scope**: the `LeaseManager` state machine (`Active` to `Committed` / `Released` / `AutoReleased` /
`ResolvedByDeactivation`); atomic multi-Quota hold acquisition in lexicographic `quota_id` order (ADR-0002); TTL
bounds without clamping; cross-period and cross-validity commit attribution to the acquisition period (I5); lazy
semantic release on every read and write path (I4); `LeaseSweeper` physical reclamation under the
`lease-sweeper` cluster election (`SingletonScope::LeaseSweeper`); the acquisition contention timeout (I8) and the
active-lease cap (I7) with their telemetry; the recovery NFR verification.

**Out of scope**: the lease-resolution behavior of Quota deactivation (the quota-lifecycle feature,
`cpt-cf-quota-enforcement-flow-quota-deactivate`; this document only records the resulting state transition), the
evaluation pipeline and the idempotency machinery (established by the consumption-operations feature as
`cpt-cf-quota-enforcement-algo-evaluation-pipeline` and `cpt-cf-quota-enforcement-algo-idempotency-replay` and
consumed here unchanged), the cluster coordination adapter (the foundation feature,
`cpt-cf-quota-enforcement-dod-coordination-adapter`; this feature owns only the sweep body), notification
dispatch and the shared threshold-emission routine (the notifications feature,
`cpt-cf-quota-enforcement-algo-threshold-emission`; this feature only enqueues events in the same transaction as its
state mutation, invariant I11), and the settlement machinery of period rollover (the consumption-operations feature,
`cpt-cf-quota-enforcement-algo-period-rollover`; leases feed it their acquisition-period attribution).

**Requirements**: `cpt-cf-quota-enforcement-fr-lease-acquire`, `cpt-cf-quota-enforcement-fr-lease-commit`,
`cpt-cf-quota-enforcement-fr-lease-release`, `cpt-cf-quota-enforcement-fr-lease-timeout`,
`cpt-cf-quota-enforcement-nfr-recovery`

**Principles**: `cpt-cf-quota-enforcement-principle-lazy-expiry`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-quota-enforcement-actor-quota-consumer` | Acquires, commits, and releases leases with client-supplied idempotency keys and a caller-chosen TTL |
| `cpt-cf-quota-enforcement-actor-storage-backend` | Serializes concurrent hold acquisition under the deterministic ordering and persists lease, hold, and capacity-counter rows atomically |
| `cpt-cf-quota-enforcement-actor-monitoring-system` | Scrapes the lease guard-rail counters, the acquisition-wait histogram, and the unreclaimed-expired gauge |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) (§5.6 lease and two-phase operations, §5.8 idempotency, §3.4 Decision contract,
  §5.16 telemetry, §6.1 gear NFRs)
- **Design**: [DESIGN.md](../DESIGN.md) (`LeaseManager`, `LeaseSweeper`, storage-plugin lease group and invariants
  I1/I4/I5/I7/I8/I9/I11, §3.3 Cluster Coordination, §3.6 sequences, §1.2 NFR allocation and §3.8 deployment
  topology for sweeper coordination and recovery)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) (§2.6)
- **ADR**: [ADR-0002 Acquisition ordering](../ADR/0002-cpt-cf-quota-enforcement-adr-acquisition-ordering.md),
  [ADR-0004 Settlement window emit](../ADR/0004-cpt-cf-quota-enforcement-adr-settlement-window-emit.md),
  [ADR-0006 Coordination via the cluster gear](../ADR/0006-cpt-cf-quota-enforcement-adr-coordination-plugin.md)
- **Dependencies**: `cpt-cf-quota-enforcement-feature-consumption-operations` (the `EvaluationOrchestrator` pipeline
  the acquire path runs through, the idempotency scope and replay machinery, and the period-rollover settlement that
  waits on lease resolution), plus transitively `cpt-cf-quota-enforcement-feature-resolution-policy-engine` (the
  Decision and Debit Plan a lease holds against), `cpt-cf-quota-enforcement-feature-projection-contracts` (ingress
  validation and subject resolution), and `cpt-cf-quota-enforcement-feature-foundation` (storage plugin, admission,
  coordination adapter, telemetry conventions)

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-quota-enforcement-usecase-reserve-and-commit`

### Acquire Lease

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-lease-acquire`

Realises `cpt-cf-quota-enforcement-seq-lease-acquire`.

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Success Scenarios**:
- An `Allowed` Decision places a hold on every Quota named in the Engine's Debit Plan atomically, and the caller
  receives an opaque lease token with its expiry timestamp
- A `Denied` Decision is returned as an HTTP 200 verdict with no capacity held in any Quota
- A replay of the same idempotency key returns the original `AcquireLeaseOutcome` without a second hold

**Error Scenarios**:
- `amount <= 0`: `INVALID_AMOUNT` before idempotency lookup, multi-quota evaluation, or any hold acquisition;
  no idempotency record, no lease row, no capacity hold, and the active-lease counter is unchanged
- `ttl` missing or outside `[min_lease_ttl, max_lease_ttl]`: `TTL_OUT_OF_BOUNDS` with the same nothing-persisted
  guarantee; the TTL is never clamped
- The per-`(tenant, metric)` active-lease cap would be exceeded: `LEASE_INFLIGHT_LIMIT_EXCEEDED` (429) regardless of
  underlying Quota capacity
- The acquisition contention timeout fires: `LEASE_CONTENTION_TIMEOUT` (canonical `Aborted`, 409) with no hold on any
  Quota

**Steps**:
1. [ ] - `p1` - Caller sends `POST /v1/quota-enforcement/leases` with an `AcquireLeaseRequest` carrying caller-supplied
   attribution, one operation-level metadata object, optional resource, metric, positive integer `amount`, required
   `ttl`, and idempotency key; foundation admission (`cpt-cf-quota-enforcement-flow-authorized-admission`) and
   projection-contracts ingress validation have already run - `inst-lac-request`
2. [ ] - `p1` - **IF** `amount <= 0` - `inst-lac-amount-if`
   1. [ ] - `p1` - **RETURN** `INVALID_AMOUNT` (`DomainError::InvalidAmount`, canonical `InvalidArgument`) before
      idempotency lookup, multi-quota evaluation, or any hold acquisition; no idempotency record, lease row, or
      capacity hold is created and the per-`(tenant, metric)` active-lease counter is unchanged
      (`cpt-cf-quota-enforcement-fr-lease-acquire`) - `inst-lac-amount`
3. [ ] - `p1` - **IF** `ttl` is missing or falls outside the operator-configurable `[min_lease_ttl, max_lease_ttl]`
   window (platform defaults 1 s and 1 h) - `inst-lac-ttl-if`
   1. [ ] - `p1` - **RETURN** `TTL_OUT_OF_BOUNDS` (`DomainError::TtlOutOfBounds`, canonical `InvalidArgument`) at the
      gateway with the same nothing-persisted guarantee as the `INVALID_AMOUNT` fail-fast; clamping is not performed,
      because the lease contract entitles the holder to the exact TTL it reserved - `inst-lac-ttl`
4. [ ] - `p1` - Run the pipeline `cpt-cf-quota-enforcement-algo-evaluation-pipeline` with `acquire_lease` as the
   mutating primitive and `reserve` as the operation type in the idempotency scope; the complete authorized,
   catalogue-mapped applicable-subject set becomes the acquisition's `IdempotencySubjectKey`, and an exact replay short-circuits
   through `cpt-cf-quota-enforcement-algo-idempotency-replay` and **RETURN**s the stored `AcquireLeaseOutcome` - `inst-lac-pipeline`
5. [ ] - `p1` - **IF** the Decision is `Denied` (the lease would exceed at least one applicable Quota under the active
   Policy; lease evaluation is identical to debit per `cpt-cf-quota-enforcement-fr-multi-quota-evaluation`) - `inst-lac-denied-if`
   1. [ ] - `p1` - Persist `AcquireLeaseOutcome::Denied { decision }` under the acquisition idempotency scope and
      **RETURN** it as an HTTP 200 verdict with no capacity held in any Quota; increment `denial_total` by the closed
      `reason` kind - `inst-lac-denied`
6. [ ] - `p1` - DB: `LeaseManager` locks the `lease_capacity_counters` row for `(tenant, metric)`; **IF** the count of
   active leases would exceed the operator-configured cap (default 1000, sourced from `lease_capacity_config` with the
   `tenant_id IS NULL`/`metric IS NULL` row as the platform default, cached in-process per I7) - `inst-lac-cap-if`
   1. [ ] - `p1` - **RETURN** `LEASE_INFLIGHT_LIMIT_EXCEEDED` (`StorageError::LeaseInflightLimitExceeded` lifted to
      canonical `ResourceExhausted`, 429) without holding any Quota; increment
      `lease_inflight_limit_exceeded_total` with the canonical registered `metric` label; expired leases never count
      toward the cap (I4) - `inst-lac-cap`
7. [ ] - `p1` - **IF** the wait on contended counter rows exceeds the operator-configured per-metric acquisition
   contention timeout (default 0 ms, fail-fast, from `contention_timeout_config`; mechanism plugin-internal per
   I8) - `inst-lac-contention-if`
   1. [ ] - `p1` - **RETURN** `LEASE_CONTENTION_TIMEOUT` (`StorageError::LeaseContentionTimeout` lifted to canonical
      `Aborted`, 409) with no hold on any Quota; increment `lease_contention_rejected_total` with the canonical
      registered `metric` label - `inst-lac-contention`
8. [ ] - `p1` - DB: `acquire_lease(applicable, plan, ttl, idem_scope)` in the single backend transaction: insert the
   `leases` row and one `lease_holds` row per Quota named in the Debit Plan, acquiring row locks in ascending
   lexicographic `quota_id` order (ADR-0002) so that either every named Quota's capacity is held or none is; capture
   `acquisition_period_id` for every consumption Quota in the plan (I5) and the acquisition's
   `IdempotencySubjectKey`; increment the active-lease counter same-tx (I7); persist
   `AcquireLeaseOutcome::Acquired { token }` as the idempotency outcome (I1, I2); commit - `inst-lac-insert`
9. [ ] - `p1` - Observe `lease_acquisition_wait_seconds` with the canonical registered `metric` label for the wait
   before successful acquisition or rejection on every path through steps 6 to 8 - `inst-lac-wait`
10. [ ] - `p1` - **RETURN** `{ lease_token, expiry_at }`; the token is opaque and server-issued, each applicable
    Quota's remaining capacity is decreased by the reserved amount for the TTL duration, and the SDK path is
    `QuotaEnforcementClientV1::acquire_lease(req)` returning `AcquireLeaseOutcome::Acquired { token }` (a `Denied`
    verdict from step 5 returns `AcquireLeaseOutcome::Denied { decision }`) - `inst-lac-return`

### Commit Lease

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-lease-commit`

Realises `cpt-cf-quota-enforcement-seq-lease-commit`.

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Success Scenarios**:
- An active lease converts into a debit of `actual_amount` against the acquisition period, and the difference
  (reserved minus actual) returns to each affected Quota's remaining capacity
- A commit after the acquisition period's calendar boundary, or after the Quota's `validity_end`, succeeds and lands
  on the acquisition period's counter
- A replay of the same idempotency key returns the original Decision without a second counter effect

**Error Scenarios**:
- The lease is expired or already resolved: `LEASE_NOT_ACTIVE` (canonical `FailedPrecondition`, 400)
- `actual_amount > reserved_amount`: `OVER_COMMIT_NOT_AUTHORIZED` (canonical `FailedPrecondition`, 400)

**Steps**:
1. [ ] - `p1` - Caller sends `POST /v1/quota-enforcement/leases/{token}/commit` with a `CommitLeaseRequest` carrying
   an optional `actual_amount` less than or equal to the reserved amount and the commit's own idempotency key
   (operation type `commit`, so the same key string never cross-matches the acquire) - `inst-lcm-request`
2. [ ] - `p1` - DB: read the lease's persisted acquisition `IdempotencySubjectKey`, construct the commit
   `IdempotencyScope`, and run `lookup_idempotency`; on an exact replay **RETURN** the stored Decision per
   `cpt-cf-quota-enforcement-algo-idempotency-replay` - `inst-lcm-idem`
3. [ ] - `p1` - DB: begin the transaction and lock the lease row with the lazy-expiry guard in the predicate,
   `state = 'active' AND expiry_at > now()` (I4), so an expired lease is rejected without depending on sweeper
   liveness - `inst-lcm-lock`
4. [ ] - `p1` - **IF** the lease is expired or in a terminal state - `inst-lcm-notactive-if`
   1. [ ] - `p1` - **RETURN** `LEASE_NOT_ACTIVE` (`StorageError::LeaseNotActive` lifted to canonical
      `FailedPrecondition`); this covers committed, released, auto-released, and resolved-by-deactivation leases
      alike - `inst-lcm-notactive`
5. [ ] - `p1` - **IF** `actual_amount > reserved_amount` - `inst-lcm-overcommit-if`
   1. [ ] - `p1` - **RETURN** `OVER_COMMIT_NOT_AUTHORIZED` (`StorageError::OverCommitNotAuthorized`); callers that
      need more than their lease must reserve a higher worst-case estimate up front
      (`cpt-cf-quota-enforcement-fr-lease-commit`) - `inst-lcm-overcommit`
6. [ ] - `p1` - DB: `commit_lease(token, actual_amount, idem_scope, events)` in the same transaction: lock the
   `lease_holds` and counter rows for the `acquisition_period_id`, apply `actual_amount` against the acquisition
   period's counter and return `reserved - actual` to it (I5), transition the lease to `Committed`, decrement the
   active-lease counter, invoke the shared threshold-emission routine
   (`cpt-cf-quota-enforcement-algo-threshold-emission`; it stays silent for settlement-window mutations per
   ADR-0004), and persist the commit's idempotency record (I1, I11); commit - `inst-lcm-apply`
7. [ ] - `p1` - Cross-boundary attribution holds by construction: a commit after a period rollover mutates the
   acquisition period's counter and never the new period's, and a commit after the Quota's `validity_end` succeeds
   because the lease guarantee was earned at acquisition time; operators that want strict cutoffs constrain the TTL
   bounds instead - `inst-lcm-boundary`
8. [ ] - `p1` - **RETURN** the `Decision` (Allowed); the commit produces a debit record addressable by the commit
   call's idempotency key, so it is reversible through the consumption-operations rollback flow
   (`cpt-cf-quota-enforcement-flow-rollback`); the SDK path is
   `QuotaEnforcementClientV1::commit_lease(req)` returning `Decision` - `inst-lcm-return`

### Release Lease

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-lease-release`

Realises `cpt-cf-quota-enforcement-seq-lease-release` (the symmetric inverse of
`cpt-cf-quota-enforcement-seq-lease-commit`; only the counter direction differs).

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Success Scenarios**:
- The full held amount returns to every affected Quota's remaining capacity against the acquisition period, with no
  debit committed
- A replay of the same idempotency key is a no-op after the first invocation

**Error Scenarios**:
- The lease is expired or already resolved: `LEASE_NOT_ACTIVE`

**Steps**:
1. [ ] - `p1` - Caller sends `POST /v1/quota-enforcement/leases/{token}/release` with a `ReleaseLeaseRequest` carrying
   the release's own idempotency key (operation type `release`) - `inst-lrl-request`
2. [ ] - `p1` - DB: read the lease's persisted acquisition `IdempotencySubjectKey`, construct the release
   `IdempotencyScope`, and run `lookup_idempotency`; on an exact replay **RETURN** the stored outcome, a no-op per
   `cpt-cf-quota-enforcement-fr-lease-release` - `inst-lrl-idem`
3. [ ] - `p1` - DB: lock the lease row under the same `state = 'active' AND expiry_at > now()` predicate (I4);
   **IF** the lease is expired or terminal - `inst-lrl-notactive-if`
   1. [ ] - `p1` - **RETURN** `LEASE_NOT_ACTIVE`; an expired lease has already been semantically released, so a
      caller-issued release has nothing left to return - `inst-lrl-notactive`
4. [ ] - `p1` - DB: `release_lease(token, idem_scope, events)` in one transaction: return the full `held_amount` of
   every hold to the acquisition period's counter (I5), transition the lease to `Released`, decrement the
   active-lease counter, and persist the release's idempotency record (I1, I11); commit; the eight-kind event catalog
   defines no dedicated event for a caller-initiated release - `inst-lrl-apply`
5. [ ] - `p1` - **RETURN** the `Decision`; the SDK path is `QuotaEnforcementClientV1::release_lease(req)` returning
   `Decision` - `inst-lrl-return`

## 3. Processes / Business Logic (CDSL)

### Lazy Semantic Expiry

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-lazy-expiry`

**Input**: any read or write path that observes lease rows; the lease `expiry_at` timestamp; `now()`

**Output**: capacity accounting in which no expired lease holds capacity, independent of sweeper liveness

**Steps**:
1. [ ] - `p1` - Every reader and every writer treats a lease with `expiry_at <= now()` as released, regardless of
   whether its storage row still exists, still reads `Active`, or has been reclaimed (I4,
   `cpt-cf-quota-enforcement-principle-lazy-expiry`) - `inst-lzy-rule`
2. [ ] - `p1` - The held amounts of expired leases are excluded from every Quota's in-flight capacity, so new leases
   and debits are never blocked by zombie rows of already-expired leases - `inst-lzy-capacity`
3. [ ] - `p1` - Expired leases do not count toward the per-`(tenant, metric)` active-lease cap; the cap bounds live
   in-flight leases and the row growth between sweeper runs - `inst-lzy-cap`
4. [ ] - `p1` - Write-path enforcement is the `expiry_at > now()` predicate on the lease row lock in commit and
   release; a commit against an expired lease is rejected with `LEASE_NOT_ACTIVE` deterministically with respect to
   the expiry timestamp - `inst-lzy-write`
5. [ ] - `p1` - **RETURN** the semantic tier stays correct under sweeper outage, partition, restart, or any other
   lifecycle event of the reclamation tier; it is the only tier on which correctness depends
   (`cpt-cf-quota-enforcement-fr-lease-timeout`) - `inst-lzy-return`

### Lease Sweeper Reclamation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-lease-sweep`

Realises `cpt-cf-quota-enforcement-seq-lease-auto-release`.

**Input**: the periodic tick (operator-configurable interval, default 60 s), the coordination adapter's
`lease-sweeper` election, the operator-configurable batch size (P1 reference default 1000)

**Output**: expired lease rows transitioned to `AutoReleased`, their capacity returned, and exactly one
`lease-auto-released` event enqueued per lease

**Steps**:
1. [ ] - `p1` - API: the `LeaseSweeper` hands its sweep body to the coordination adapter for
   `SingletonScope::LeaseSweeper`; while this replica is a follower no sweep body runs (the foundation
   `cpt-cf-quota-enforcement-dod-coordination-adapter` owns the election semantics) - `inst-swp-elect`
2. [ ] - `p1` - The elected replica runs the tick loop on the child `CancellationToken` it received; the resolved
   cluster backend renews the claim; on leadership loss the token is cancelled, the loop stops before its next batch,
   and the replica is a follower again until re-election, which is automatic - `inst-swp-lost`
3. [ ] - `p1` - DB: `reclaim_expired_leases(batch_size, before = now())`; **FOR EACH** batch, one transaction:
   transition every lease with `expiry_at <= now() AND state = 'active'` to `AutoReleased`, decrement the
   active-lease counter per row, return each hold's `held_amount` to its acquisition period's counter (I5), and
   enqueue exactly one `lease-auto-released` event per lease, carrying the lease ID, owning subject context, held
   amount, affected Quotas, and expiry timestamp, in the same transaction (I11); commit - `inst-swp-reclaim`
4. [ ] - `p1` - The sweeper is the canonical emission point for `lease-auto-released`: emission is deterministic with
   respect to the expiry timestamp, and under sweeper outage the events are deferred until reclamation while the
   semantic tier keeps accounting correct - `inst-swp-emit`
5. [ ] - `p1` - Physical reclamation completes within an operator-configurable interval after expiry (default 1 hour);
   the sweeper **MAY** delete lease rows after a grace period per operator configuration - `inst-swp-interval`
6. [ ] - `p1` - Emit the `lease_unreclaimed_expired` gauge for the count of expired-but-unreclaimed leases by canonical
   registered `metric`, so operators can detect and localize sweeper outages - `inst-swp-gauge`
7. [ ] - `p1` - **RETURN** after the cycle; sweeper liveness never gates correctness: a paused or crashed sweeper
   only defers reclamation and event emission, and a dead leader's lock becomes acquirable by a survivor within one
   TTL - `inst-swp-return`

## 4. States (CDSL)

### Lease State Machine

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-state-lease`

**States**: Active, Committed, Released, AutoReleased, ResolvedByDeactivation

**Initial State**: Active (created by `acquire_lease` together with its holds and the capacity-counter increment)

**Transitions**:
1. [ ] - `p1` - **FROM** Active **TO** Committed **WHEN** a commit succeeds before `expiry_at`: `actual_amount` is
   debited against the acquisition period and `reserved - actual` returns to it - `inst-lst-commit`
2. [ ] - `p1` - **FROM** Active **TO** Released **WHEN** a release succeeds before `expiry_at`: the full held amount
   returns to the acquisition period - `inst-lst-release`
3. [ ] - `p1` - **FROM** Active **TO** AutoReleased **WHEN** the sweeper reclaims a lease whose `expiry_at` has
   passed; semantically the lease is already released from `expiry_at` onward (I4), so this physical transition only
   reconciles the row and emits the `lease-auto-released` event - `inst-lst-autorelease`
4. [ ] - `p1` - **FROM** Active **TO** ResolvedByDeactivation **WHEN** the owning Quota's deactivation cascade
   resolves the lease atomically with the deactivation transaction (owned by the quota-lifecycle feature,
   `cpt-cf-quota-enforcement-flow-quota-deactivate`) - `inst-lst-deactivate`

All four non-initial states are terminal (closed enum); commit and release against any terminal state, or against an
expired `Active` row, return `LEASE_NOT_ACTIVE`. Between `expiry_at` and physical reclamation a row may still read
`Active` in storage; every reader and writer overrides that reading per
`cpt-cf-quota-enforcement-algo-lazy-expiry`. Each lease transition mutates `lease_capacity_counters` atomically with
the state change.

## 5. Definitions of Done

### Lease Operation Endpoints

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-lease-endpoints`

The system **MUST** deliver `LeaseManager` (`cpt-cf-quota-enforcement-component-lease-manager`) behind the three REST
endpoints and the SDK methods `QuotaEnforcementClientV1::acquire_lease`,
`QuotaEnforcementClientV1::commit_lease`, and `QuotaEnforcementClientV1::release_lease`. Acquire requests with
`amount <= 0` **MUST** be rejected with `INVALID_AMOUNT`, and requests with a missing `ttl` or a `ttl` outside
`[min_lease_ttl, max_lease_ttl]` **MUST** be rejected with `TTL_OUT_OF_BOUNDS` without clamping; both fail-fasts fire
before idempotency lookup, multi-quota evaluation, or any hold acquisition and persist nothing. All three operations
**MUST** reuse the consumption-operations idempotency machinery unchanged under the distinct operation types
`reserve`, `commit`, and `release`, with acquire replay returning the original `AcquireLeaseOutcome`, commit/release
replay returning the original Decision, and payload divergence returning `IDEMPOTENCY_PAYLOAD_MISMATCH`. Commit **MUST**
reject `actual_amount > reserved_amount` with
`OVER_COMMIT_NOT_AUTHORIZED`, and commit or release against an expired or resolved lease **MUST** return
`LEASE_NOT_ACTIVE`.

**Implements**:
- `cpt-cf-quota-enforcement-flow-lease-acquire`
- `cpt-cf-quota-enforcement-flow-lease-commit`
- `cpt-cf-quota-enforcement-flow-lease-release`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`,
`cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: `POST /v1/quota-enforcement/leases`, `POST /v1/quota-enforcement/leases/{token}/commit`,
  `POST /v1/quota-enforcement/leases/{token}/release`
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: `Lease`, `LeaseToken`, `AcquireLeaseRequest`, `CommitLeaseRequest`, `ReleaseLeaseRequest`, `Decision`

### Atomic Multi-Quota Acquisition and Guard Rails

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-lease-acquisition`

The system **MUST** acquire holds on every Quota named in the Engine's Debit Plan atomically through `acquire_lease`:
either every named Quota's capacity is held or none is, including under failure and concurrent contention, with row
locks taken in ascending lexicographic `quota_id` order (ADR-0002) so concurrent multi-Quota lease traffic cannot
deadlock. The per-`(tenant, metric)` active-lease cap (default 1000, `lease_capacity_config` overrides, I7) **MUST**
be enforced atomically same-tx with the lease insert, rejecting over-cap requests with
`LEASE_INFLIGHT_LIMIT_EXCEEDED` regardless of underlying Quota capacity; expired leases do not count toward the cap.
The per-metric acquisition contention timeout (default 0 ms fail-fast, `contention_timeout_config`, I8) **MUST**
bound the wait on contended rows, rejecting with `LEASE_CONTENTION_TIMEOUT` and no holds. The system **MUST** populate
`lease_contention_rejected_total`, `lease_acquisition_wait_seconds`, and `lease_inflight_limit_exceeded_total`, each
labelled by canonical registered `metric`, so operators can distinguish and localize contention rejections from
cap-exceeded, not-active, and Engine-`Denied` outcomes.

**Implements**:
- `cpt-cf-quota-enforcement-flow-lease-acquire`

**Constraints**: `cpt-cf-quota-enforcement-constraint-bounded-cardinality`

**Touches**:
- API: no new endpoint (rides the acquire path)
- DB: `cpt-cf-quota-enforcement-db-schema` (`leases`, `lease_holds`, `lease_capacity_counters`,
  `lease_capacity_config`, `contention_timeout_config`)
- Entities: `Lease`, `LeaseHold`, `LeaseCapacityCounter`, `QuotaDebitPlan`

### Acquisition-Period and Validity Attribution

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-lease-attribution`

The system **MUST** capture `acquisition_period_id` for every consumption Quota in the plan at acquisition time and
attribute every commit, release, and auto-release counter mutation to that period (I5), never to the wall-clock
current period: the held amount counts against the acquisition period from acquisition until the lease reaches a
terminal state and never against any subsequent period. Commits arriving after the underlying Quota's
`validity_end` **MUST** succeed with their counter mutation applied, and commits or releases after a period boundary
**MUST** land on the acquisition period's counter, leaving the new period unaffected; commits whose lease TTL has
expired remain rejected with `LEASE_NOT_ACTIVE` independently of validity-window expiry. Settlement-window mutations
follow the emit policy of ADR-0004 through the shared threshold-emission routine; the settlement machinery itself is
owned by the consumption-operations feature.

**Implements**:
- `cpt-cf-quota-enforcement-flow-lease-commit`
- `cpt-cf-quota-enforcement-flow-lease-release`
- `cpt-cf-quota-enforcement-algo-lease-sweep`

**Constraints**: `cpt-cf-quota-enforcement-constraint-single-storage-plugin`

**Touches**:
- API: no new endpoint (semantics of the commit and release paths)
- DB: `cpt-cf-quota-enforcement-db-schema` (`lease_holds`, `quota_consumption_counters`,
  `quota_allocation_counters`)
- Entities: `Lease`, `LeaseHold`, `Counter` (allocation), `Counter` (consumption)

### Lazy Expiry Enforcement

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-lazy-expiry`

The system **MUST** enforce the lazy-expiry semantic (I4,
`cpt-cf-quota-enforcement-principle-lazy-expiry`) on every read and write path: a lease with `expiry_at <= now()` is
treated as released regardless of physical row presence, its held amount is excluded from in-flight capacity, it does
not count toward the active-lease cap, and new leases and debits are never blocked by unreclaimed expired rows. The
write-path realization is the `expiry_at > now()` predicate on the lease row lock in `commit_lease` and
`release_lease`. Semantic release **MUST** remain correct under sweeper outage, partition, restart, or any other
lifecycle event of the reclamation tier; the gateway never checks sweeper state defensively.

**Implements**:
- `cpt-cf-quota-enforcement-algo-lazy-expiry`
- `cpt-cf-quota-enforcement-state-lease`

**Constraints**: `cpt-cf-quota-enforcement-constraint-single-storage-plugin`

**Touches**:
- API: no new endpoint (a semantic on every lease-observing path)
- DB: `cpt-cf-quota-enforcement-db-schema` (`leases`, `lease_holds`, `lease_capacity_counters`)
- Entities: `Lease`, `LeaseCapacityCounter`

### Lease Sweeper

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-lease-sweeper`

The system **MUST** deliver `LeaseSweeper` (`cpt-cf-quota-enforcement-component-lease-sweeper`) as a single-leader
background task (default tick 60 s) under the `lease-sweeper` cluster election (`SingletonScope::LeaseSweeper`)
through the foundation coordination adapter, consuming its run-while-leader semantics without re-specifying them. Each cycle invokes `reclaim_expired_leases` in operator-configurable batches
(P1 reference default 1000): per batch one transaction transitions expired `Active` leases to `AutoReleased`, returns
held capacity to acquisition-period counters, decrements `lease_capacity_counters`, and enqueues exactly one
`lease-auto-released` event per lease same-tx (I11). Reclamation **MUST** complete within the operator-configured
interval after expiry (default 1 hour); rows **MAY** be deleted after a grace period. The sweeper **MUST** surface
the `lease_unreclaimed_expired` gauge by canonical registered `metric`. Sweeper liveness **MUST NOT** gate correctness. The
sweeper **MUST** run as a lifecycle-managed background task per the ToolKit lifecycle model: it receives a child
`CancellationToken`, its tick and batch loop are cancellation-aware, and on graceful shutdown it stops
starting new batches while the adapter resigns the election so a successor does not wait out the TTL (ADR-0006).

**Implements**:
- `cpt-cf-quota-enforcement-algo-lease-sweep`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`,
`cpt-cf-quota-enforcement-constraint-bounded-cardinality`

**Touches**:
- API: `SingletonCoordinator` port (`SingletonScope::LeaseSweeper`)
- DB: `cpt-cf-quota-enforcement-db-schema` (`leases`, `lease_holds`, `lease_capacity_counters`,
  `notification_outbox`)
- Entities: `Lease`, `LeaseCapacityCounter`, `NotificationOutboxEvent`

### Recovery NFR Verification

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-recovery-verification`

The system **MUST** verify `cpt-cf-quota-enforcement-nfr-recovery` (RTO of at most 15 minutes from storage backend
recovery) through the DESIGN disaster-recovery drill: a full restart of the gear and its storage backend, verifying
that evaluation operations resume within 15 minutes. The mechanisms under test are those the DESIGN allocates: gateway
auto-reconnect, automatic lease re-claim through lazy expiry (I4, no lease-recovery procedure exists or is needed),
and the sweeper rejoining its `lease-sweeper` cluster election after restart, with the leadership gap bounded by one
election TTL plus observation lag. No promise beyond the PRD threshold is added.

**Implements**:
- `cpt-cf-quota-enforcement-algo-lazy-expiry`
- `cpt-cf-quota-enforcement-algo-lease-sweep`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`

**Touches**:
- API: no new endpoint (a drill against the deployed gear)
- Entities: drill fixtures (no new domain entities)

## 6. Acceptance Criteria

- [ ] An acquire with `amount <= 0` fails with `INVALID_AMOUNT`, and an acquire with a missing `ttl` or a `ttl` of
  `min_lease_ttl - 1` or `max_lease_ttl + 1` fails with `TTL_OUT_OF_BOUNDS`, each before idempotency lookup,
  multi-quota evaluation, or any hold acquisition: no idempotency record, no lease row, no capacity hold, and the
  active-lease counter is unchanged; the TTL is never silently clamped
- [ ] A successful acquire returns an opaque lease token with `expiry_at` and decreases each applicable Quota's
  remaining capacity by the reserved amount; a replay of the same acquire key returns the original token without a
  second hold; a `Denied` verdict returns HTTP 200 and holds no capacity in any Quota
- [ ] When the Debit Plan names multiple Quotas, holds land on every named Quota or on none, including under injected
  mid-transaction failure; a concurrency test with interleaved multi-Quota acquires against overlapping Quota sets
  observes no partial holds and no deadlock (locks taken in ascending `quota_id` order per ADR-0002)
- [ ] With the per-`(tenant, metric)` cap at its default, the 1001st concurrent active lease fails with
  `LEASE_INFLIGHT_LIMIT_EXCEEDED` (429) even though Quota capacity remains, and
  `lease_inflight_limit_exceeded_total` increments with the canonical registered `metric` label; letting held leases
  expire frees the cap without any sweeper run
- [ ] With the contention timeout at the 0 ms default, an acquire that hits a contended counter row fails with
  `LEASE_CONTENTION_TIMEOUT` (409) holding nothing; `lease_contention_rejected_total` and
  `lease_acquisition_wait_seconds` are populated with the canonical registered `metric` label for both rejected and
  successful acquisitions
- [ ] A commit with `actual_amount < reserved_amount` debits `actual_amount` and returns the difference to each
  affected Quota; a commit with `actual_amount > reserved_amount` fails with `OVER_COMMIT_NOT_AUTHORIZED`; commit and
  release against an expired, committed, released, auto-released, or resolved-by-deactivation lease fail with
  `LEASE_NOT_ACTIVE`; commit and release replays return the stored outcome with no second counter effect
- [ ] A lease acquired in period `P` and committed after the `P`/`P+1` boundary mutates period `P`'s counter only,
  with `P+1`'s consumed and remaining unchanged; release and auto-release after the boundary return capacity to `P`
  only; a commit after the Quota's `validity_end` succeeds when the lease TTL is still live
- [ ] A debit created by a lease commit is rolled back through the commit call's idempotency key via the
  consumption-operations rollback flow
- [ ] With the sweeper paused, an expired lease stops counting against capacity immediately: a new acquire or debit
  sized to need exactly the expired hold's capacity succeeds while the zombie row still exists, and a commit against
  the expired lease fails with `LEASE_NOT_ACTIVE`; the `lease_unreclaimed_expired` gauge reports the backlog by
  canonical registered `metric`; after the sweeper resumes, rows transition to `AutoReleased` and the deferred
  `lease-auto-released` events are enqueued
- [ ] The sweeper reclaims expired lease rows within the operator-configured interval after expiry (default at most
  1 hour) and enqueues exactly one `lease-auto-released` event per lease, carrying the lease ID, owning subject
  context, held amount, affected Quotas, and expiry timestamp, in the same transaction as the state transition
- [ ] One `LeaseSweeper` leader runs at a time across replicas in steady state; killing the leader makes a survivor
  the leader of `SingletonScope::LeaseSweeper` within one election TTL plus observation lag, and the survivor resumes
  reclamation
- [ ] Two sweep bodies forced to run at once over the same expired leases (advisory overlap) produce exactly one
  state transition, one capacity reversal, and one `lease-auto-released` event per lease
- [ ] The disaster-recovery drill (full restart of gear and storage backend) shows evaluation and lease operations
  resuming within 15 minutes, with no manual lease recovery step and the sweeper election rejoined automatically
- [ ] Metrics scrape shows the four lease instruments labelled by canonical registered `metric`, with no `tenant_id`,
  `subject_id`, `quota_id`, `idempotency_key`, `lease_token`, projection-type, caller, or raw/unregistered metric label

## 7. Additional Context (optional)

- **Feature boundaries**: the acquire path runs the consumption-operations pipeline
  (`cpt-cf-quota-enforcement-algo-evaluation-pipeline`) and its idempotency machinery unchanged; this feature adds
  the hold-acquisition tail (cap check, contention guard, `acquire_lease`). The deactivation transition of the state
  machine is executed by the quota-lifecycle cascade; only the resulting `ResolvedByDeactivation` state is recorded
  here. Event dispatch, per-sink behavior, and the threshold-emission routine belong to the notifications feature;
  this feature enqueues its events same-tx (I11) and invokes the shared routine at its mutation call sites. The
  cluster coordination adapter and its run-while-leader semantics are the foundation's; this feature owns the
  sweep body (tick and batch loop on the adapter's cancellation token).
- **Upstream alignment items (tracked upstream prerequisites)**: the DESIGN sequence names a
  `system:quota-enforcement-sweeper` identity for the sweeper, but `SecurityContext` carries
  no service-identity field; this document treats the sweeper as an internal background task and leaves the identity
  representation as a tracked upstream DESIGN item. The SDK acquire path returns the DESIGN-defined
  `AcquireLeaseOutcome` (`Acquired { token }` or `Denied { decision }`), so a `Denied` reserve surfaces as a Decision
  verdict at HTTP 200 per PRD §3.4 without abusing the error channel.
- **Idempotency subject key**: reserve fingerprints its complete authorized, catalogue-mapped applicable-subject set. The lease row
  persists that `IdempotencySubjectKey`, and commit/release reuse it rather than resolving against the current
  catalogue. This preserves both complete-projection coverage and stable follow-up replay scope.
- **Rust contract notes**: `LeaseManager` and the sweeper call the async Tokio-based storage plugin; the sweeper is a
  single Tokio task whose leadership state is process-local, so no shared mutable state crosses tasks without the
  storage plugin or the cluster election as the synchronization owner. Commit and release hold no in-process lock
  across an await point; row serialization is delegated to the storage plugin (I9). The sweeper's reclamation
  transaction is idempotent by predicate (`state = 'active' AND expiry_at <= now()`), so a repeated cycle after a
  crash re-processes only still-unreclaimed rows.
- **Rollout / rollback**: the endpoints are stateless above the storage plugin; the sweeper resigns its election on shutdown and a
  successor is elected within one round trip, so rollout is a rolling update under the same schema major version. Lease rows carry a closed
  state enum, and terminal rows are retained as ledger entries within operation-log retention, so a binary rollback
  re-reads the same rows without migration.
- **Test layering**: fail-fast ordering, TTL bounds, cap arithmetic, state transitions, and period attribution get
  unit tests; atomic multi-Quota acquisition, contention rejection, lazy expiry under a paused sweeper, and replay
  behavior get integration and concurrency tests against the storage plugin; leader failover and the recovery drill
  are the deployment-level checks named in section 6.
- **Deactivation cascade (tracked here)**: the quota-lifecycle feature implemented the deactivation orchestration and
  the reference plugin's `deactivate_quota`, which flips the status under the row lock, logs, and enqueues
  `quota-changed` but resolves no lease yet (`resolved_leases` is empty). This feature fills the lease half inside that
  transaction — lock the Quota's active leases, mark them resolved-by-deactivation, decrement
  `lease_capacity_counters`, return held capacity to the acquisition-period counters, and append one
  `lease-resolved-by-deactivation` event per lease — which closes `cpt-cf-quota-enforcement-flow-quota-deactivate`,
  `cpt-cf-quota-enforcement-state-quota-lifecycle`, and `cpt-cf-quota-enforcement-dod-deactivation-cascade` of the
  quota-lifecycle feature. Lock order stays Quota row, then its counter and lease rows (ADR-0002).
- **Non-applicable review domains**: UX/accessibility is not applicable; there is no user-facing surface. Data
  protection inherits the PRD §6.2 rules; lease rows carry tenant-scoped opaque identifiers and follow the
  operation-log retention boundary with no additional feature-specific requirement.
