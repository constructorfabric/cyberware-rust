<!-- Created: 2026-08-26 by Constructor Tech -->

# Feature: Resolution Policy & Engine

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-featstatus-resolution-policy-engine-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-quota-enforcement-feature-resolution-policy-engine`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Policy Create and Update with Engine Validation](#policy-create-and-update-with-engine-validation)
  - [Policy Rollback](#policy-rollback)
  - [Policy Delete](#policy-delete)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Engine Bootstrap Registration and Global Policy Seeding](#engine-bootstrap-registration-and-global-policy-seeding)
  - [Active Policy Selection and Engine Invocation Boundary](#active-policy-selection-and-engine-invocation-boundary)
  - [Most-Restrictive-Wins Evaluation](#most-restrictive-wins-evaluation)
  - [CEL Validation and Sandboxed Evaluation](#cel-validation-and-sandboxed-evaluation)
- [4. States (CDSL)](#4-states-cdsl)
  - [QuotaResolutionPolicyVersion State Machine](#quotaresolutionpolicyversion-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Policy Service and Versioned Operator Surface](#policy-service-and-versioned-operator-surface)
  - [Engine Registry and Global Policy Seeding](#engine-registry-and-global-policy-seeding)
  - [Engine Plugin Contract](#engine-plugin-contract)
  - [Debit-Plan Invariant Boundary](#debit-plan-invariant-boundary)
  - [Most-Restrictive-Wins Built-in Engine](#most-restrictive-wins-built-in-engine)
  - [CEL Built-in Engine](#cel-built-in-engine)
  - [Policy and Engine Telemetry](#policy-and-engine-telemetry)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Implements the Quota Resolution Policy entity with immutable versioning and rollback, the `QuotaResolutionEngineV1`
plugin contract with the `most-restrictive-wins` and `cel` built-ins, multi-Quota arbitration with strict Debit-Plan
invariants enforced at the Engine boundary, cascade and attribute-based selection expressiveness, and the hard-cap
denial contract.

### 1.2 Purpose

Arbitration logic is not a property of QE-core: it is a `QuotaResolutionEngineV1` plugin selected per scope through an
operator-managed, versioned Policy. This feature delivers that seam. QE-core enforces only the closed Debit-Plan
invariant set at the Engine boundary, so operator-authored `cel` Policies and future Engines cannot corrupt counters,
while cascade, split, and metadata-gated selection stay expressible without core changes. It also seeds the `global`
Policy at bootstrap, extending the foundation bootstrap flow, so evaluation never enters a "no Policy applies" state
and no active Policy ever references an unregistered Engine.

**Scope**: `PolicyService` with scope precedence (per-metric over `global`) and the four-state version lifecycle with
optimistic `if_match_version` concurrency; `EngineRegistry` with fail-fast bootstrap registration and `global`-Policy
seeding; the Engine contract (`id`/`validate_config`/`evaluate`) with the `ValidatedConfig` cache keyed by
`(policy_id, policy_version)`; Debit-Plan invariant enforcement at the Engine boundary with violation telemetry; the
`most-restrictive-wins` binding-Quota selection with validity-window prefilter; the sandboxed cost-bounded `cel`
Engine with static checking against snapshotted request and constraint schemas; the operator-only Policy surface
(`QuotaOperatorClientV1`).

**Out of scope**: applying Debit Plans to counters and the `EvaluationOrchestrator` pipeline that invokes the boundary
defined here (consumption-operations feature), additional Engine languages (P2-or-later per PRD §13), ingress
validation and the catalogue-membership check invoked by Policy writes (projection-contracts feature), and the storage
retention sweep that hard-deletes retired Policy versions (PRD §5.9 assigns it to "the storage retention sweeper";
DESIGN's `RetentionSweeper` component does not yet declare a policy-version reclamation primitive, a tracked upstream
DESIGN/consumption-operations alignment item).

**Requirements**: `cpt-cf-quota-enforcement-fr-quota-resolution-policy`,
`cpt-cf-quota-enforcement-fr-quota-resolution-policy-versioning`,
`cpt-cf-quota-enforcement-fr-quota-resolution-engine`, `cpt-cf-quota-enforcement-fr-multi-quota-evaluation`,
`cpt-cf-quota-enforcement-fr-quota-cascade`, `cpt-cf-quota-enforcement-fr-attribute-based-quota-selection`,
`cpt-cf-quota-enforcement-fr-hard-quota-reject`

**Principles**: `cpt-cf-quota-enforcement-principle-engine-pluggable`,
`cpt-cf-quota-enforcement-principle-strict-engine-boundary`

**Constraints**: `cpt-cf-quota-enforcement-constraint-in-process-engine-registration`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-quota-enforcement-actor-platform-operator` | Creates, updates, rolls back, and deletes Quota Resolution Policies; Policy management is operator-only per PRD §2.3 |
| `cpt-cf-quota-enforcement-actor-quota-consumer` | Sends the operations whose evaluation runs under the active Policy and Engine |
| `cpt-cf-quota-enforcement-actor-types-registry` | Serves the request, resource, and attached constraint schemas snapshotted at Policy create/update |
| `cpt-cf-quota-enforcement-actor-monitoring-system` | Scrapes the Engine and Policy telemetry instruments |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) (§5.9 multi-Quota evaluation, Policy, versioning, Engine contract, cascade,
  attribute-based selection; §5.11 cap-violation rejection; §3.4 Decision contract)
- **Design**: [DESIGN.md](../DESIGN.md) (PolicyService, EngineRegistry, Engine Plugin Trait, seq-policy-version-update,
  §4.1 telemetry surface)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) (§2.4)
- **ADR**: [ADR-0005 Pluggable evaluation engine](../ADR/0005-cpt-cf-quota-enforcement-adr-evaluation-engine.md)
  (`cpt-cf-quota-enforcement-adr-evaluation-engine`, status: accepted);
  [ADR-0007 Declarative GTS projection contracts](../ADR/0007-cpt-cf-quota-enforcement-adr-projection-contracts.md)
  (`cpt-cf-quota-enforcement-adr-projection-contracts`, status: accepted; the Policy-write membership check and
  stable `{request, resource, arbitration}` input follow that ADR)
- **Dependencies**: `cpt-cf-quota-enforcement-feature-foundation` (bootstrap hook, Gateway admission, storage plugin,
  telemetry conventions), `cpt-cf-quota-enforcement-feature-projection-contracts` (snapshotted contract schemas, the
  catalogue-membership check on Policy writes, request/resource/constraint contract snapshots)

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-quota-enforcement-usecase-configure-policy`,
`cpt-cf-quota-enforcement-usecase-cascade-via-cel` (Policy authoring side; the debit that exercises it is owned by the
consumption-operations feature), `cpt-cf-quota-enforcement-usecase-region-gated-via-metadata` (same split)

### Policy Create and Update with Engine Validation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-policy-write`

**Actor**: `cpt-cf-quota-enforcement-actor-platform-operator`

**Success Scenarios**:
- A new Policy, or a new immutable version of an existing Policy, becomes active with an Engine-validated config

**Error Scenarios**:
- Unknown `engine_id`: rejected with `UNKNOWN_ENGINE` naming the registered Engines
- Engine config validation fails: actionable error before persistence (line/column for `cel`)
- Referenced projection outside the configured catalogue: `PROJECTION_NOT_RESOLVABLE`
- Stale `if_match_version` on update: `VERSION_CONFLICT` (409), no version row written
- Create at a scope that already has an active Policy: rejected

**Steps**:
1. [ ] - `p1` - Operator sends `POST /v1/quota-enforcement/policies` (create) or
   `PATCH /v1/quota-enforcement/policies/{id}` (update, with `if_match_version`) carrying scope (`global` or
   `metric=<metric_name>`), `engine_id`, opaque `engine_config`, optional per-Policy timeout (default 5ms, clamped to
   the operator-configured upper bound), and optional `comment`; foundation admission
   (`cpt-cf-quota-enforcement-flow-authorized-admission`) has already attached `SecurityContext` and `AccessScope` - `inst-pw-request`
2. [ ] - `p1` - **IF** create targets a scope that already has an active Policy - `inst-pw-dup-if`
   1. [ ] - `p1` - **RETURN** rejection; one active Policy per exact scope - `inst-pw-dup`
3. [ ] - `p1` - Resolve `engine_id` against `EngineRegistry` - `inst-pw-engine-lookup`
4. [ ] - `p1` - **IF** the `engine_id` is not registered in the current deployment - `inst-pw-unknown-if`
   1. [ ] - `p1` - **RETURN** `UNKNOWN_ENGINE` naming the registered Engines available in this deployment - `inst-pw-unknown`
5. [ ] - `p1` - API: resolve and snapshot the referenced request, resource, and attached constraint schemas through
   `TypesRegistryClient`; contract resolution and snapshotting occur at Policy create/update, never on the evaluation
   hot path; projection references are checked by the projection-contracts membership check
   (`cpt-cf-quota-enforcement-algo-catalog-membership`), which rejects a registered but non-configured projection with
   `PROJECTION_NOT_RESOLVABLE` - `inst-pw-snapshot`
6. [ ] - `p1` - Call the named Engine's `validate_config(raw)` with the snapshotted schemas: the `cel` validator
   parses, type-checks, and statically verifies property/projection references and pair compatibility per
   `cpt-cf-quota-enforcement-algo-cel-engine`; the `most-restrictive-wins` validator rejects any non-empty config - `inst-pw-validate`
7. [ ] - `p1` - **IF** validation fails - `inst-pw-invalid-if`
   1. [ ] - `p1` - **RETURN** the Engine's structured error before persistence; persisted Policies always carry an
      Engine-validated config - `inst-pw-invalid`
8. [ ] - `p1` - DB: in one storage transaction (`cpt-cf-quota-enforcement-seq-policy-version-update`): insert the new
   `quota_resolution_policy_version` row with `version_state = active` (create: `policy_version = 1`; update:
   `N + 1`), transition the prior active version to `superseded` (update only), move the latest-pointer atomically,
   enqueue the `policy-changed` event (`change_kind = created` or `updated`) in the same transaction (invariant
   I11; dispatch is owned by the notifications feature). The compiled artifact from step 6 is retained and published
   into the `ValidatedConfig` cache keyed by `(policy_id, policy_version)` only after the transaction commits, per the
   Engine Plugin Trait compiled-artifact contract; a rolled-back transaction publishes nothing, and a cache miss
   rebuilds from the persisted config - `inst-pw-persist`
9. [ ] - `p1` - **IF** `if_match_version` does not equal the current latest - `inst-pw-conflict-if`
   1. [ ] - `p1` - **RETURN** `VERSION_CONFLICT` (409) with the current latest; increment
      `policy_version_conflict_rejections_total`; no version row is written - `inst-pw-conflict`
10. [ ] - `p1` - **RETURN** `201 Created` (create) or `200 OK` (update) with the new `PolicyVersion`; increment
    `policy_version_transitions_total`; every replica's next evaluation observes the new version through the authoritative pointer read - `inst-pw-return`

### Policy Rollback

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-policy-rollback`

**Actor**: `cpt-cf-quota-enforcement-actor-platform-operator`

**Success Scenarios**:
- Rollback makes a prior version active again atomically

**Error Scenarios**:
- Rollback to a nonexistent version: `UNKNOWN_POLICY_VERSION`
- Rollback to a `rolled_back` version: `VERSION_ROLLED_BACK`

**Steps**:
1. [ ] - `p1` - Operator sends `POST /v1/quota-enforcement/policies/{id}/rollback` with `target_version` and optional
   `comment` - `inst-prd-rollback-request`
2. [ ] - `p1` - **IF** `target_version` does not exist - `inst-prd-unknown-if`
   1. [ ] - `p1` - **RETURN** `UNKNOWN_POLICY_VERSION` - `inst-prd-unknown`
3. [ ] - `p1` - **IF** `target_version` is in `rolled_back` state - `inst-prd-rb-if`
   1. [ ] - `p1` - **RETURN** `VERSION_ROLLED_BACK`; terminal versions are never re-activated - `inst-prd-rb`
4. [ ] - `p1` - DB: atomically make `target_version` active again, transition the previously-active version to
   `rolled_back` (terminal), move the latest-pointer, and enqueue `policy-changed` with `change_kind = updated`
   (rollback is a latest-pointer move; `rolled_back` is a `version_state` value, not a notification discriminator);
   the operation is naturally idempotent on retry against the same target - `inst-prd-rollback-apply`
5. [ ] - `p1` - **RETURN** `200 OK` with the new active `PolicyVersion`; increment
   `policy_version_transitions_total` - `inst-prd-rollback-return`

### Policy Delete

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-policy-delete`

**Actor**: `cpt-cf-quota-enforcement-actor-platform-operator`

**Success Scenarios**:
- Delete soft-deletes a narrow-scope Policy so evaluation falls through to the next-most-specific scope

**Error Scenarios**:
- Delete of the seeded `global` Policy: `CANNOT_DELETE_SEEDED_GLOBAL_POLICY` (canonical `FailedPrecondition`, HTTP 400)

**Steps**:
1. [ ] - `p1` - Operator sends `DELETE /v1/quota-enforcement/policies/{id}` with optional `comment` - `inst-prd-delete-request`
2. [ ] - `p1` - **IF** the target is the seeded `global` Policy - `inst-prd-global-if`
   1. [ ] - `p1` - **RETURN** canonical `FailedPrecondition` (HTTP 400,
      `reason = "CANNOT_DELETE_SEEDED_GLOBAL_POLICY"`); the seeded global Policy is never deletable - `inst-prd-global`
3. [ ] - `p1` - DB: atomically transition the currently-active version to `deleted` (terminal), clear the
   latest-pointer, and enqueue `policy-changed` with `change_kind = deleted`; historical versions keep their existing
   `superseded`/`rolled_back` state; subsequent evaluations in this scope fall through to the next-most-specific scope - `inst-prd-delete-apply`
4. [ ] - `p1` - **IF** the `policy_id` is already deleted - `inst-prd-replay-if`
   1. [ ] - `p1` - **RETURN** `204 No Content` as a no-op: no state change and no second `policy-changed (deleted)`
      event; `404` is returned only when the `policy_id` was never created - `inst-prd-replay`
5. [ ] - `p1` - **RETURN** `204 No Content`; increment `policy_version_transitions_total`; retained versions follow
   the 90-day retention window per `cpt-cf-quota-enforcement-state-policy-version` - `inst-prd-delete-return`

## 3. Processes / Business Logic (CDSL)

### Engine Bootstrap Registration and Global Policy Seeding

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-engine-bootstrap-seed`

**Input**: the gear binary with its statically linked built-in Engines, the foundation bootstrap hook, the storage
plugin `bootstrap()` seeding step

**Output**: a populated `EngineRegistry` and the seeded `global` Policy, or failed gear readiness

**Steps**:
1. [ ] - `p1` - Register `most-restrictive-wins` and `cel` in the static in-process `EngineRegistry` at gear
   bootstrap; Engines link into the binary at build time and there is no runtime registration
   (`cpt-cf-quota-enforcement-constraint-in-process-engine-registration`) - `inst-ebs-register`
2. [ ] - `p1` - **IF** any built-in Engine declared in the deployment manifest fails to register - `inst-ebs-fail-if`
   1. [ ] - `p1` - Fail readiness and serve nothing; emit a structured log entry and increment
      `engine_bootstrap_failures_total` by `engine_id`; never silently fall back to a different Engine for Policies
      that referenced the failed one; recovery is fixing the registration failure and restarting the gear; this step
      extends `cpt-cf-quota-enforcement-flow-gear-bootstrap` from the foundation feature - `inst-ebs-fail`
3. [ ] - `p1` - DB: after Engine registration succeeds, seed the `global` Policy idempotently when missing, inside the
   storage plugin `bootstrap()` seeding step:
   `policy_id = global, policy_version = 1, version_state = active, engine_id = most-restrictive-wins, engine_config = {}` - `inst-ebs-seed`
4. [ ] - `p1` - The registration-before-seeding order guarantees that no active Policy ever references an unregistered
   Engine; the seeded global Policy is not deletable and remains the ultimate fallback, so evaluation never enters a
   "no Policy applies" state - `inst-ebs-order`
5. [ ] - `p1` - **RETURN** ready; a Policy referencing an `engine_id` not registered in the current deployment is
   rejected at create/update time per `cpt-cf-quota-enforcement-flow-policy-write` - `inst-ebs-return`

### Active Policy Selection and Engine Invocation Boundary

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-engine-boundary`

**Input**: the operation's metric, the materialized `EvaluationContext` (built by the consumption-operations pipeline;
CEL sees only `{request, resource, arbitration}`), `EngineRegistry`, `PolicyService`

**Output**: a validated `Decision`, or a platform-canonical error with no counter mutation

**Steps**:
1. [ ] - `p1` - Select the active Policy by most-specific-scope precedence: the `metric` Policy if one is defined for
   the operation's metric, else the `global` Policy; `PolicyService` reads the latest-version pointer
   authoritatively from storage inside the evaluation transaction (`read_policy` in the DESIGN evaluation sequence),
   so every replica always evaluates the active version per PRD §5.9; only the immutable version-keyed
   `ValidatedConfig` is cached - `inst-eb-select`
2. [ ] - `p1` - Resolve the Policy's `engine_id` in `EngineRegistry` and fetch the `ValidatedConfig` from the cache
   keyed by `(policy_id, policy_version)`; a cache miss rebuilds from the persisted Engine-validated config - `inst-eb-config`
3. [ ] - `p1` - Call `evaluate(ctx, config)`: the call is synchronous, deterministic given the `EvaluationContext` (which carries
   the effective `EvaluationBudget`, the per-Policy timeout), and performs no I/O; observe `engine_evaluation_seconds` by `engine_id`; `EvaluationContext.active_policy` carries
   `policy_id` and `policy_version` - `inst-eb-evaluate`
4. [ ] - `p1` - **IF** the Engine reports the per-Policy evaluation timeout (default 5ms) as `EngineError::Timeout` - `inst-eb-timeout-if`
   1. [ ] - `p1` - **RETURN** canonical `DeadlineExceeded` (fail-closed); the bound is enforced inside the Engine per
      ADR-0005 cost-bounding, because a synchronous `evaluate()` call cannot be preempted from outside; no partial
      Decision is accepted and no counter is mutated - `inst-eb-timeout`
5. [ ] - `p1` - **CATCH** `EngineError` - `inst-eb-error-catch`
   1. [ ] - `p1` - Lift per the DESIGN §3.3 mapping: `Timeout` to `DeadlineExceeded`; `CostExceeded` to
      `ResourceExhausted`; `TypeError`/`Internal` to `Internal`; `InvalidConfig` is caught at Policy create/update and
      never reaches the evaluation hot path; **RETURN** the canonical error with no counter mutation - `inst-eb-error`
6. [ ] - `p1` - Validate the returned Decision against the closed Debit-Plan invariant set before any mutation
   (`cpt-cf-quota-enforcement-principle-strict-engine-boundary`): every `debit_plan` quota_id is a member of
   `applicable_quotas`; per entry `amount >= 0`; per entry `amount <= request.amount`; result-plan consistency
   (`Allowed` implies non-empty `debit_plan`, `Denied` implies empty); the system does not constrain the sum of
   entries; additionally enforce the `most-restrictive-wins` engine-specific invariant (exactly one entry with
   `amount = request.amount` on `Allowed`) - `inst-eb-invariants`
7. [ ] - `p1` - **IF** any invariant is violated - `inst-eb-violation-if`
   1. [ ] - `p1` - **RETURN** canonical `Internal` carrying the `INVARIANT_VIOLATION` sub-token in `detail`
      (DESIGN §3.3); the Decision shape is not
      returned to the caller and no counter is mutated; increment `debit_plan_invariant_violations_total` by
      `(engine_id, invariant)` from the closed set
      `{quota_id_outside_applicable_set, negative_amount, amount_exceeds_request_amount, result_plan_inconsistency}` - `inst-eb-violation`
8. [ ] - `p1` - Require Decision `diagnostics` to include `engine_id`, `policy_id`, and `policy_version` of the Policy
   that produced the Decision, plus the per-Quota detail of
   `cpt-cf-quota-enforcement-fr-multi-quota-evaluation` (quota ID, type, `enforcement_mode`, current
   consumed/in-flight amount, cap, contribution to the final Decision) - `inst-eb-diagnostics`
9. [ ] - `p1` - **RETURN** the validated Decision to the evaluation pipeline; the consumption-operations
   `EvaluationOrchestrator` owns the pipeline and applies the Debit Plan atomically via the storage plugin - `inst-eb-return`

### Most-Restrictive-Wins Evaluation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-most-restrictive-wins`

**Input**: `EvaluationContext` (applicable Quotas with type, `enforcement_mode`, cap, validity window, metadata,
current usage; request metric and amount; time); an empty `ValidatedConfig` (any non-empty config is rejected at
Policy write)

**Output**: a single-entry `Allowed` Debit Plan against the binding Quota, or `Denied`

**Steps**:
1. [ ] - `p1` - **IF** the applicable-Quotas set is empty - `inst-mrw-empty-if`
   1. [ ] - `p1` - **RETURN** `Denied(violated_quota_ids = [], reason = "NO_APPLICABLE_QUOTA")` with an empty
      `debit_plan`; for a `QuotaGated` metric, absence of any applicable Quota is absence of authorization - `inst-mrw-empty`
2. [ ] - `p1` - Apply the validity-window prefilter: exclude Quotas whose `validity_window` is set and whose `time`
   falls outside `[validity_start, validity_end]`; Quotas without a validity window are always considered; if the
   prefilter empties the set, **RETURN** the same `NO_APPLICABLE_QUOTA` denial - `inst-mrw-window`
3. [ ] - `p1` - Ignore `arbitration`, `request`, and `resource` entirely; metadata-driven selection requires a `cel`
   Policy per `cpt-cf-quota-enforcement-fr-attribute-based-quota-selection` - `inst-mrw-metadata`
4. [ ] - `p1` - Compute the satisfiable set: a Quota is satisfiable if `remaining >= request.amount`; unbounded Quotas
   (`cap = null`) are trivially satisfiable - `inst-mrw-satisfiable`
5. [ ] - `p1` - **IF** no Quota is satisfiable (every applicable bounded Quota has remaining below `request.amount`
   and no applicable unbounded Quota exists) - `inst-mrw-deny-if`
   1. [ ] - `p1` - **RETURN** `Denied(violated_quota_ids, reason)` naming every such bounded Quota with the requested
      amount, the current remaining capacity, and the violation amount (full enumeration, no short-circuit; unbounded
      Quotas are never violators); counters are not modified
      (`cpt-cf-quota-enforcement-fr-hard-quota-reject`; `hard` is the only P1 `enforcement_mode`) - `inst-mrw-deny`
6. [ ] - `p1` - Select the binding Quota from the satisfiable set in priority order: (1) subject-scope tier,
   more-specific owner projection wins (P1: user-scope over tenant-scope), which is the built-in subject-scope cascade
   of `cpt-cf-quota-enforcement-fr-quota-cascade`; (2) bounded over unbounded within the chosen tier; (3) smallest
   remaining capacity among bounded satisfiable Quotas of the tier, ties broken by ascending `quota_id` (UUIDv7);
   among unbounded Quotas (reached only when rule 2 falls through), ascending `quota_id` is the sole tiebreaker - `inst-mrw-binding`
7. [ ] - `p1` - **RETURN** `Allowed` with `debit_plan` of exactly one entry against the binding Quota at
   `amount = request.amount`; non-binding applicable Quotas are absent from the plan and their counters are not
   mutated; this exact shape is enforced as the engine-specific invariant in
   `cpt-cf-quota-enforcement-algo-engine-boundary` - `inst-mrw-return`

### CEL Validation and Sandboxed Evaluation

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-cel-engine`

**Input**: at Policy write: raw `engine_config = { expr: <CEL string> }` plus the snapshotted request, resource, and
constraint schemas; at evaluation: `EvaluationContext` and the cached compiled `ValidatedConfig`

**Output**: a compiled `ValidatedConfig` or a structured validation error; at evaluation, a Decision or a canonical
error

**Steps**:
1. [ ] - `p1` - `validate_config` parses and type-checks the expression against the stable
   `{request, resource, arbitration}` environment, the snapshotted request/resource/constraint schemas, and the Decision return schema; errors carry line/column
   diagnostics - `inst-cel-parse`
2. [ ] - `p1` - Statically check property references and request/arbitration compatibility: type disagreement,
   non-intersecting declared domains, and scalar/collection operator mismatch are rejected at save time. Attribution
   and authenticated principal fields are absent and therefore rejected as unknown - `inst-cel-static`
3. [ ] - `p1` - Cache the compiled representation by `(policy_id, policy_version)`; the artifact is compiled at every
   Policy create/update and published to the cache after the write transaction commits, per
   `cpt-cf-quota-enforcement-flow-policy-write` - `inst-cel-cache`
4. [ ] - `p1` - `evaluate` binds the `EvaluationContext` into the CEL environment and evaluates under sandbox: no I/O,
   deterministic, fixed step/cost cap tuned to the `EvaluationBudget` carried on the context - `inst-cel-evaluate`
5. [ ] - `p1` - **IF** a runtime error occurs (cost-cap exceeded, type error at evaluation, malformed return record) - `inst-cel-error-if`
   1. [ ] - `p1` - **RETURN** the corresponding `EngineError`; the boundary lifts it to a platform-canonical error
      with no counter mutation per `cpt-cf-quota-enforcement-algo-engine-boundary` - `inst-cel-error`
6. [ ] - `p1` - Interpret the returned record as a Decision; the standard Debit-Plan invariants apply uniformly:
   `cel` Policies may emit multi-entry plans for cross-tier splits, intra-tier cascade between same-scope Quotas
   identified by metadata, proportional distributions, and multi-tier cascades
   (`cpt-cf-quota-enforcement-fr-quota-cascade`), and may match predicates over
   `request`, `resource`, and each applicable Quota's `arbitration` value to emit a sparse plan over the selected subset
   (`cpt-cf-quota-enforcement-fr-attribute-based-quota-selection`); subject resolution stays metadata-agnostic, so
   `applicable_quotas` always carries every resolved Quota regardless of metadata - `inst-cel-decision`
7. [ ] - `p1` - **RETURN** the Decision; a Policy that filters out every applicable Quota returns `Denied` with an
   actionable `reason`, because `Allowed` with an empty `debit_plan` for a non-zero `request.amount` violates
   `result_plan_inconsistency` and surfaces a canonical `Internal` error - `inst-cel-return`

## 4. States (CDSL)

### QuotaResolutionPolicyVersion State Machine

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-state-policy-version`

**States**: Active, Superseded, RolledBack, Deleted

**Initial State**: Active (create and bootstrap seeding materialize `policy_version = 1` as `active`)

**Transitions**:
1. [ ] - `p1` - **FROM** Active **TO** Superseded **WHEN** an update creates version `N + 1` as the new active
   version; latest-pointer moves atomically in the same transaction - `inst-pvst-supersede`
2. [ ] - `p1` - **FROM** Superseded **TO** Active **WHEN** a rollback targets this version; the latest-pointer moves
   to it atomically - `inst-pvst-reactivate`
3. [ ] - `p1` - **FROM** Active **TO** RolledBack **WHEN** a rollback replaces this version with an earlier one;
   RolledBack is terminal and never re-activated (`VERSION_ROLLED_BACK` on a later rollback attempt targeting it) - `inst-pvst-rollback`
4. [ ] - `p1` - **FROM** Active **TO** Deleted **WHEN** `delete_policy` soft-deletes the `policy_id`; Deleted is
   terminal, the latest-pointer is cleared, and evaluation falls through to the next-most-specific scope - `inst-pvst-delete`

At most one version per `policy_id` is Active at any time, and no reader observes intermediate states: every
transition commits atomically with its latest-pointer move. Superseded, RolledBack, and Deleted versions are retained
for an operator-configurable window (default 90 days) and then hard-deleted by the storage retention sweeper (PRD
§5.9; DESIGN's `RetentionSweeper` component does not yet declare a policy-version reclamation primitive, a tracked
upstream DESIGN/consumption-operations alignment item); Active versions are never auto-deleted, and the seeded
`global` Policy's versions are kept indefinitely.

## 5. Definitions of Done

### Policy Service and Versioned Operator Surface

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-policy-service`

The system **MUST** deliver `PolicyService` owning the Quota Resolution Policy lifecycle over the
`quota_resolution_policy` and `quota_resolution_policy_version` tables via the storage plugin: create, update (new
immutable version), rollback, soft-delete (narrow-scope only), read latest or a specific version, and list versions,
with version-based optimistic concurrency (`VERSION_CONFLICT` on `if_match_version` mismatch), atomic
version-plus-latest-pointer transitions, most-specific-scope selection (per-metric over `global`) with the latest-version pointer read authoritatively from
storage per evaluation, Engine-delegated config validation with registry-snapshotted schemas, the
projection-contracts membership check on every Policy write, and `policy-changed` events enqueued in the same
transaction as the mutation. The surface is operator-only: the REST endpoints and the `QuotaOperatorClientV1` methods
(`create_policy`, `update_policy`, `rollback_policy`, `delete_policy`, `list_policy_versions`) are deliberately absent
from `QuotaManagerClientV1` per PRD §2.3.

**Implements**:
- `cpt-cf-quota-enforcement-flow-policy-write`
- `cpt-cf-quota-enforcement-flow-policy-rollback`
- `cpt-cf-quota-enforcement-flow-policy-delete`
- `cpt-cf-quota-enforcement-state-policy-version`

**Constraints**: `cpt-cf-quota-enforcement-constraint-in-process-engine-registration`

**Touches**:
- API: `POST /v1/quota-enforcement/policies`, `GET /v1/quota-enforcement/policies/{id}`,
  `GET /v1/quota-enforcement/policies/{id}/versions`, `PATCH /v1/quota-enforcement/policies/{id}`,
  `POST /v1/quota-enforcement/policies/{id}/rollback`, `DELETE /v1/quota-enforcement/policies/{id}`;
  `QuotaOperatorClientV1` Policy methods
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: `QuotaResolutionPolicy`, `QuotaResolutionPolicyVersion`

### Engine Registry and Global Policy Seeding

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-engine-registry`

The system **MUST** deliver the static in-process `EngineRegistry` with compile-time linkage of the built-in Engines,
fail-fast bootstrap registration (readiness failure, structured log, and `engine_bootstrap_failures_total` on any
registration failure, with no silent Engine fallback), ID-to-Engine resolution at evaluation time, and the idempotent
seeding of the `global` Policy (`most-restrictive-wins`, version 1, empty config) after Engine registration, so no
active Policy ever references an unregistered Engine.

**Implements**:
- `cpt-cf-quota-enforcement-algo-engine-bootstrap-seed`

**Constraints**: `cpt-cf-quota-enforcement-constraint-in-process-engine-registration`

**Touches**:
- API: no new endpoint (bootstrap extension of the foundation gear-bootstrap flow)
- DB: `cpt-cf-quota-enforcement-db-schema`
- Entities: `QuotaResolutionPolicy`, `QuotaResolutionPolicyVersion`

### Engine Plugin Contract

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-engine-contract`

The system **MUST** ship `QuotaResolutionEngineV1` in the `quota-enforcement-sdk` crate as a sync trait with three
methods: `id() -> &str`, `validate_config(raw: &serde_json::Value) -> Result<Box<dyn ValidatedConfig>,
EngineConfigError>`, and `evaluate(ctx: &EvaluationContext, config: &dyn ValidatedConfig) -> Result<Decision,
EngineError>`, where `ValidatedConfig` is an opaque marker trait (`Any + Send + Sync`) and `EngineError` is the closed
enum `{Timeout, CostExceeded, TypeError, InvalidConfig, Internal}`. `evaluate` **MUST** be deterministic given the
`EvaluationContext` and **MUST NOT** perform I/O; cost-bounding is the Engine implementation's responsibility. Engines
whose evaluation requires a compiled artifact rely on the `ValidatedConfig` cache keyed by
`(policy_id, policy_version)`, compiled at every Policy create/update and published after the write transaction
commits; a cache miss rebuilds from the persisted Engine-validated config.

**Implements**:
- `cpt-cf-quota-enforcement-algo-engine-boundary`

**Constraints**: `cpt-cf-quota-enforcement-constraint-in-process-engine-registration`

**Touches**:
- API: `QuotaResolutionEngineV1` (SDK trait)
- Entities: `EvaluationContext`, `Decision`, `QuotaDebitPlan`

### Debit-Plan Invariant Boundary

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-invariant-boundary`

The system **MUST** validate every Engine-returned Decision against the closed Debit-Plan invariant set before any
counter mutation: `debit_plan` membership in `applicable_quotas`, per-entry `amount >= 0`, per-entry
`amount <= request.amount`, result-plan consistency, and the `most-restrictive-wins` engine-specific single-entry
invariant. Violations **MUST** surface the canonical `Internal` error carrying the `INVARIANT_VIOLATION` sub-token in
`detail` (DESIGN §3.3), never the Decision shape, with no counter mutation, and increment
`debit_plan_invariant_violations_total` by
`(engine_id, invariant)`. The per-Policy timeout (default 5ms) **MUST** be enforced with `DeadlineExceeded` on expiry
and the partial Decision discarded. Decision diagnostics **MUST** carry `engine_id`, `policy_id`, `policy_version`,
and the per-Quota detail of `cpt-cf-quota-enforcement-fr-multi-quota-evaluation`. This DoD defines the boundary
contract; the consumption-operations `EvaluationOrchestrator` invokes it inside the pipeline it owns.

**Implements**:
- `cpt-cf-quota-enforcement-algo-engine-boundary`

**Constraints**: `cpt-cf-quota-enforcement-constraint-in-process-engine-registration`

**Touches**:
- API: no new endpoint (in-process boundary between orchestrator and Engine)
- Entities: `Decision`, `QuotaDebitPlan`, `EvaluationContext`

### Most-Restrictive-Wins Built-in Engine

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-mrw-engine`

The system **MUST** ship the `most-restrictive-wins` built-in Engine: hardcoded, config-free (any non-empty
`engine_config` rejected at validation), metadata-ignoring, with the validity-window prefilter, the
`NO_APPLICABLE_QUOTA` denial for an empty (or fully prefiltered) applicable set, binding-Quota selection by
subject-scope tier (user-scope over tenant-scope, the built-in cascade), bounded-over-unbounded, smallest remaining,
and ascending-`quota_id` tiebreaks, full enumeration of `violated_quota_ids` with requested amount, current remaining,
and violation amount on denial, and a single-entry `Allowed` plan at `amount = request.amount`.

**Implements**:
- `cpt-cf-quota-enforcement-algo-most-restrictive-wins`

**Constraints**: `cpt-cf-quota-enforcement-constraint-in-process-engine-registration`

**Touches**:
- API: `QuotaResolutionEngineV1` implementation (built-in crate, in-process linkage)
- Entities: `Decision`, `QuotaDebitPlan`, `EvaluationContext`

### CEL Built-in Engine

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-cel-engine`

The system **MUST** ship the `cel` built-in Engine: sandboxed, deterministic, cost-bounded evaluation with a
pre-compiled artifact cache keyed by `(policy_id, policy_version)`; save-time static validation against the
snapshotted request, resource, and constraint schemas covering property references and pair
compatibility (type disagreement, non-intersecting domains, scalar/collection operator mismatch), and the Decision
return schema, with line/column diagnostics; exclusion of attribution and principal fields; and runtime
errors surfaced as canonical errors with no counter mutation. The Engine **MUST** preserve the customizable cascade
and attribute-based selection expressiveness: multi-entry split, intra-tier cascade, proportional distribution, and
sparse metadata-gated plans.

**Implements**:
- `cpt-cf-quota-enforcement-algo-cel-engine`

**Constraints**: `cpt-cf-quota-enforcement-constraint-in-process-engine-registration`

**Touches**:
- API: `QuotaResolutionEngineV1` implementation (built-in crate, in-process linkage)
- Entities: `Decision`, `QuotaDebitPlan`, `EvaluationContext`

### Policy and Engine Telemetry

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-policy-engine-telemetry`

The system **MUST** emit, through the foundation telemetry conventions, the instruments this feature owns: per PRD
§5.16 and DESIGN §4.1, `engine_bootstrap_failures_total` (`engine_id`), `engine_evaluation_seconds` (`engine_id`), and
`debit_plan_invariant_violations_total` (`engine_id`, `invariant` from the closed set of four); per the PRD §5.9
versioning telemetry requirement, with labels defined by DESIGN §4.1, `policy_version_transitions_total`
(`transition_kind` from `{create, update, rollback, delete}`) and
`policy_version_conflict_rejections_total` (no labels). `policy_id`, `metric`, projection types, and caller
attribution **MUST NOT** appear as label values; Policy attribution belongs on traces
(`qe.policy_id`, `qe.policy_version`, `qe.engine_id`), not metrics.

**Implements**:
- `cpt-cf-quota-enforcement-flow-policy-write`
- `cpt-cf-quota-enforcement-flow-policy-rollback`
- `cpt-cf-quota-enforcement-flow-policy-delete`
- `cpt-cf-quota-enforcement-algo-engine-bootstrap-seed`
- `cpt-cf-quota-enforcement-algo-engine-boundary`

**Constraints**: `cpt-cf-quota-enforcement-constraint-in-process-engine-registration`

**Touches**:
- API: platform observability stack (`tracing` + `toolkit` `otel` feature, per the foundation telemetry conventions)
- Entities: gear-specific counters and histograms per PRD §5.16, the PRD §5.9 versioning telemetry requirement, and
  DESIGN §4.1

## 6. Acceptance Criteria

- [ ] A deployment manifest declaring an Engine that fails registration (fault-injected `cel` initialization failure)
  fails readiness, serves nothing, increments `engine_bootstrap_failures_total`, and never falls back to another
  Engine
- [ ] Repeated bootstraps seed the `global` Policy exactly once as
  `policy_id = global, policy_version = 1, version_state = active, engine_id = most-restrictive-wins, engine_config = {}`
- [ ] `DELETE` against the seeded `global` Policy returns HTTP 400 with
  `reason = "CANNOT_DELETE_SEEDED_GLOBAL_POLICY"`; the Policy stays active
- [ ] A Policy create or update naming an unregistered `engine_id` is rejected with `UNKNOWN_ENGINE` naming the
  registered Engines; a `cel` config with a parse error, an attribution/principal reference, or an incompatible
  request/constraint pair is rejected before persistence with line/column diagnostics; a
  `most-restrictive-wins` Policy with a non-empty config is rejected
- [ ] A Policy write referencing a registered but non-configured projection is rejected with
  `PROJECTION_NOT_RESOLVABLE` (via the projection-contracts membership check)
- [ ] An update with a stale `if_match_version` returns `VERSION_CONFLICT` (409) with the current latest, writes no
  version row, and increments `policy_version_conflict_rejections_total`
- [ ] Rollback to a nonexistent version returns `UNKNOWN_POLICY_VERSION`; rollback to a `rolled_back` version returns
  `VERSION_ROLLED_BACK`; retried rollback against the same target is idempotent; rollback emits `policy-changed` with
  `change_kind = updated`
- [ ] After `delete_policy` on a per-metric Policy, evaluation for that metric falls through to the `global` Policy; a
  repeated `DELETE` returns 204 as a no-op with no second `policy-changed (deleted)` event; `404` is returned only for
  a never-created `policy_id`
- [ ] Concurrent readers never observe an inconsistent version/latest-pointer mix: every transition (update, rollback,
  delete) is atomic with its pointer move
- [ ] A test Engine returning a quota_id outside `applicable_quotas`, a negative amount, an amount above
  `request.amount`, or `Allowed` with an empty plan gets the canonical `Internal` error carrying the
  `INVARIANT_VIOLATION` sub-token in `detail`, mutates no counter, and increments
  `debit_plan_invariant_violations_total` with
  the matching `invariant` label; a `most-restrictive-wins` `Allowed` plan that is not exactly one entry at
  `amount = request.amount` is rejected the same way
- [ ] The PRD §5.9 reference scenario holds: with `user_q(remaining = 20)` and `tenant_q(remaining = 9700)` for a
  debit of 50, `most-restrictive-wins` produces `debit_plan = { tenant_q: 50 }`, and a `cel` split-cascade Policy
  produces `debit_plan = { user_q: 20, tenant_q: 30 }`
- [ ] When no applicable Quota is satisfiable, the `most-restrictive-wins` denial enumerates every violated bounded
  Quota with quota ID, requested amount, current remaining, and violation amount, and no counter changes; an empty or
  fully validity-window-prefiltered applicable set yields
  `Denied(violated_quota_ids = [], reason = "NO_APPLICABLE_QUOTA")`
- [ ] A `cel` Policy whose metadata predicate matches a subset debits only that subset
  (the PRD §5.9 region-gating example); a predicate that filters out every Quota yields `Denied` with an actionable
  reason, never `Allowed` with an empty plan
- [ ] An Engine evaluation exceeding the per-Policy timeout surfaces `DeadlineExceeded`, discards any partial
  Decision, and mutates no counter; a `cel` cost-cap exhaustion surfaces `ResourceExhausted`
- [ ] Both built-in Engines return byte-identical Decisions for repeated evaluation of the same `EvaluationContext`
  (determinism property test, the input to idempotent replay)
- [ ] Decision diagnostics carry `engine_id`, `policy_id`, and `policy_version`, plus the per-Quota detail (quota ID,
  type, `enforcement_mode`, current amount, cap, contribution)
- [ ] Metrics scrape shows no `policy_id`, `quota_id`, `tenant_id`, metric, projection-type, or caller label on any
  instrument this feature owns

## 7. Additional Context (optional)

- **Policy compatibility at bootstrap (tracked here)**: the projection-contracts feature's catalogue bootstrap fails
  when the configured catalogue is incompatible with an active Quota; the Policy half of that check waits for this
  feature, because a Policy version carries no contract reference until Policy validation snapshots the request,
  resource, and constraint contracts it type-checks against. This feature adds that reference and the bootstrap check
  over it, which (with the quota-lifecycle database query) closes `cpt-cf-quota-enforcement-dod-projection-catalog`.
- **ADR dependencies**: `cpt-cf-quota-enforcement-adr-evaluation-engine` (ADR-0005, accepted) fixes the
  capability-based Engine contract this document restates; `cpt-cf-quota-enforcement-adr-projection-contracts`
  (ADR-0007, accepted) fixes the Policy-write membership check and the CEL input contract; the
  projection-contracts feature owns both.
- **Budget and freshness contracts**: the Engine receives its per-Policy evaluation budget as the typed
  `EvaluationBudget` on `EvaluationContext` and enforces it internally per ADR-0005 (a synchronous `evaluate()`
  cannot be preempted from outside). A Policy activation takes effect on the next evaluation of every replica: the latest-version pointer is read
  authoritatively from storage inside the evaluation transaction, and only the immutable version-keyed
  `ValidatedConfig` artifacts are cached.
- **Boundary with consumption-operations**: this feature defines Policy selection, Engine invocation, and the
  invariant boundary as contracts; the `EvaluationOrchestrator` pipeline that calls them, applies Debit Plans, and
  persists idempotency state is owned by the consumption-operations feature. No NFR from the DECOMPOSITION allocation
  table is owned here; the evaluation-latency and throughput budgets that motivate the cached `ValidatedConfig` and
  the sync no-I/O Engine contract are verified by consumption-operations benchmarks.
- **Rollout / rollback**: Policy changes are data, not deploys: every change is a new immutable version and rollback
  is a first-class operation. Adding an Engine is a binary rebuild and redeploy; a built-in Engine declared in the
  deployment manifest that fails to register fails readiness fail-fast. Intentional removal of an Engine while
  persisted Policies still reference it has no P1 contract: the Engine deprecation lifecycle is a P2 consideration per
  DESIGN §4.3 and ADR-0005.
- **Test layering**: version state transitions, scope precedence, invariant checks, and both Engines' selection logic
  get unit tests (including the determinism property test); Policy write validation, `VERSION_CONFLICT`, and
  membership rejection get integration tests against the storage plugin; the malformed-Engine criteria use a test
  double `QuotaResolutionEngineV1`; bootstrap fail-fast uses fault injection on Engine registration.
- **Secrets discipline**: `engine_config` carries arbitration logic only; PRD §5.9 forbids credentials, keys, rates,
  or customer identifiers inside it, and QE does not inspect config content for secrets. Sensitive values reach
  Policies indirectly through the validated `request` or `arbitration` objects.
- **Non-applicable review domains**: UX/accessibility is not applicable; there is no user-facing surface. Data
  protection inherits the Platform Operational Data rules from PRD §6.2; Policy versions follow the 90-day retention
  window with no additional feature-specific requirement.
