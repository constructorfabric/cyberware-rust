<!-- Created: 2026-08-26 by Constructor Tech -->

# Feature: Projection Contracts & Subject Attribution

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-featstatus-projection-contracts-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-quota-enforcement-feature-projection-contracts`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Owner Projection Publication and Catalogue Activation](#owner-projection-publication-and-catalogue-activation)
  - [Evaluation Request Ingress Validation](#evaluation-request-ingress-validation)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Catalogue Bootstrap and Consistency Set](#catalogue-bootstrap-and-consistency-set)
  - [PDP-Authorized Subject Mapping](#pdp-authorized-subject-mapping)
  - [Catalogue-Membership Check for Quota and Policy Writes](#catalogue-membership-check-for-quota-and-policy-writes)
- [4. States (CDSL)](#4-states-cdsl)
  - [ProjectionContractCatalog State Machine](#projectioncontractcatalog-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Abstract Base and Scope Registration](#abstract-base-and-scope-registration)
  - [Projection Contract Catalogue](#projection-contract-catalogue)
  - [Gateway Ingress Contract Validation](#gateway-ingress-contract-validation)
  - [Subject Attribution Mapping](#subject-attribution-mapping)
  - [Catalogue-Membership Check for Writes](#catalogue-membership-check-for-writes)
  - [Contract-Validation Telemetry](#contract-validation-telemetry)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Registers the abstract QE subject/resource/request/constraint bases and the scope-discriminator type, resolves the
deployment's owner projections into the immutable `ProjectionContractCatalog` at bootstrap, validates every evaluation
request's public shape at Gateway ingress, authorizes caller-supplied attribution through PDP, and then validates
contracts and maps scope kinds to owner
projections. This is the declarative contract surface the whole evaluation pipeline trusts.

### 1.2 Purpose

Without enforced contracts, a Policy expression can silently miss because a request key is absent, misspelled, or of the
wrong type, and per-caller projection taxonomies would fragment one shared counter into many. This feature turns those
failures into save-time, bootstrap-time, or ingress-time errors: the metric owner declares the projection, the caller
supplies logical attribution, QE checks its public shape, PDP authorizes it, and QE performs deterministic catalogue
mapping and contract validation. It also defines the
mapped-subject set that the consumption-operations feature consumes inside its `EvaluationOrchestrator` pipeline.

**Scope**: bootstrap registration of the abstract bases and the P1 `user`/`tenant` scope well-known instances;
bootstrap consistency checks; `ProjectionContractCatalog` build and
publication with the authoritative metric-to-projection reverse index; catalogue-membership checks on Quota and Policy
writes; PDP authorization and server-side `(metric, kind)` mapping; ingress validation of caller-supplied attribution,
one operation-level `metadata` object, admitted metric, and optional resource projection;
contract-validation telemetry counters.

**Out of scope**: Quota records themselves (quota-lifecycle feature), Engine consumption of request/resource/arbitration data
(resolution-policy-engine feature), the evaluation pipeline that consumes the resolved subject set
(consumption-operations feature), and breaking projection-version activation (out of P1 per PRD §4.2: writes that
reference a non-configured projection get `PROJECTION_NOT_RESOLVABLE`, and bootstrap fails on a catalogue incompatible
with active Quotas or Policies; no activation procedure is specified here).

**Requirements**: `cpt-cf-quota-enforcement-fr-projection-contracts`,
`cpt-cf-quota-enforcement-fr-contract-validation`, `cpt-cf-quota-enforcement-fr-subject-type-registry`,
`cpt-cf-quota-enforcement-fr-subject-resolution`

**Principles**: `cpt-cf-quota-enforcement-principle-declarative-projection-contracts`,
`cpt-cf-quota-enforcement-principle-pdp-authorized-attribution`

**Constraints**: `cpt-cf-quota-enforcement-constraint-types-registry-delegation`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-quota-enforcement-actor-metric-owner` | Publishes concrete subject/resource projections, one request contract per metric, and its attached constraint contract in `types-registry` |
| `cpt-cf-quota-enforcement-actor-quota-consumer` | Sends subject-based evaluation requests carrying `tenant_id`, additional `{kind,id}` subjects, and one conforming operation-level `metadata` object |
| `cpt-cf-quota-enforcement-actor-types-registry` | Authoritative catalogue of bases, projections, scope instances, and metrics; answers bootstrap resolution requests |
| `cpt-cf-quota-enforcement-actor-platform-operator` | Configures which owner projections the deployment resolves for evaluation |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) (§5.1 projection contracts, contract validation, subject registry, subject attribution)
- **Design**: [DESIGN.md](../DESIGN.md) (Gateway, EvaluationOrchestrator subject resolution, bootstrap seeded state,
  contract entities)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) (§2.2)
- **ADR**: [ADR-0007 Declarative GTS projection contracts](../ADR/0007-cpt-cf-quota-enforcement-adr-projection-contracts.md)
  (`cpt-cf-quota-enforcement-adr-projection-contracts`, status: accepted; this feature's contract shapes follow that
  ADR)
- **Dependencies**: `cpt-cf-quota-enforcement-feature-foundation` (bootstrap hook, Gateway admission, telemetry
  conventions)

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-quota-enforcement-usecase-debit` (ingress-validation prefix only; the debit body is owned by the
consumption-operations feature), `cpt-cf-quota-enforcement-usecase-create-quota` (projection-membership validation step
only; the Quota write itself is owned by the quota-lifecycle feature)

### Owner Projection Publication and Catalogue Activation

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-owner-projection-publication`

**Actor**: `cpt-cf-quota-enforcement-actor-metric-owner`

**Success Scenarios**:
- The owner's projections are published, configured, validated at bootstrap, and served from the immutable catalogue;
  every authorized caller debits the same shared counter through them

**Error Scenarios**:
- A configured projection is unregistered, abstract, or not derived from the QE base: bootstrap fails
- Two configured projections admit the same `(metric, scope)` pair: bootstrap fails rather than picking a winner
- The configured catalogue is incompatible with an active Quota or Policy: bootstrap fails

**Steps**:
1. [x] - `p1` - Owner authors one concrete derived subject projection per supported scope under `gts.cf.core.qe.subj.v1~`, declaring a required `scope` trait whose value is a `GtsInstanceId` narrowed to `gts.cf.core.qe.scope.v1~*`, and declaring admitted metrics through a typed `x-gts-traits` value narrowed by `x-gts-ref` (per `cpt-cf-quota-enforcement-fr-projection-contracts`) - `inst-pub-author`
2. [x] - `p1` - Owner publishes one concrete request contract per metric under `gts.cf.core.qe.request.v1~`; its traits name the metric and attach one constraint contract derived from `gts.cf.core.qe.constraint.v1~`; the request schema is never reused for arbitration constraints - `inst-pub-attrs`
3. [x] - `p1` - Owner optionally publishes a resource projection derived from `gts.cf.core.qe.res.v1~`; it carries identity plus schematized properties only and does not enter the P1 counter key - `inst-pub-res`
4. [x] - `p1` - API: publication happens in `types-registry`; QE exposes no registration endpoint (`cpt-cf-quota-enforcement-constraint-types-registry-delegation`) - `inst-pub-registry`
5. [x] - `p1` - Operator configures the deployment's evaluation catalogue to include the owner's projections - `inst-pub-config`
6. [x] - `p1` - QE bootstrap resolves and validates the configured set (`cpt-cf-quota-enforcement-algo-catalog-bootstrap`) and publishes the immutable `ProjectionContractCatalog` - `inst-pub-boot`
7. [x] - `p1` - **RETURN** any authorized caller supplies logical attribution and QE maps it to the owner's projection; per-caller projections for the same metric are forbidden, so one shared counter stays intact - `inst-pub-return`

### Evaluation Request Ingress Validation

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-ingress-validation`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer`

**Success Scenarios**:
- A conforming S2S request passes public request-shape checks, is authorized by PDP, is mapped through the
  catalogue, and proceeds with one validated metadata object

**Error Scenarios**:
- Missing required fields, wrong public container types, missing or empty ids, duplicate kinds, or repeated tenant scope:
  canonical `InvalidArgument` before PDP
- PDP denies a structurally valid supplied attribution tuple: canonical `PermissionDenied` before catalogue or
  contract lookup
- Authorized request with an unknown/unadmitted subject kind, contract-schema mismatch, or inadmissible metric: canonical
  `InvalidArgument` with a stable field-level reason, never `Decision::Denied`

**Steps**:
1. [x] - `p1` - Caller sends a subject-based evaluation operation carrying `tenant_id`, additional `subjects: [{kind,id}]`, one operation-level `metadata`, and optional `resource`; platform admission has authenticated the service principal and deserialized the DTO - `inst-ing-request`
2. [x] - `p1` - Validate public request shape: require all required top-level fields and their declared container types, require non-empty `tenant_id` and subject ids, reject duplicate kinds, and reject tenant scope repeated in `subjects`; return canonical `InvalidArgument` before PDP on failure - `inst-ing-shape`
3. [x] - `p1` - Send the complete untrusted tenant/subject/metric/resource tuple to PDP and attach the returned `AccessScope`; fail closed on denial or PDP unavailability - `inst-ing-authz`
4. [x] - `p1` - Map each authorized `(metric, kind)` through the process-local `ProjectionContractCatalog`; no registry call occurs on this path - `inst-ing-lookup`
5. [x] - `p1` - Validate operation-level `metadata` against the metric request contract by wrapping the wire object into the contract envelope `{type, metadata}` and validating the whole document (never the inner subschema alone); when an optional resource is present, validate its already-complete `{type, id?, metadata}` projection directly; absent operation `metadata` was rejected before PDP and is never defaulted to `{}` - `inst-ing-metadata`
6. [x] - `p1` - **IF** any kind is unknown or does not admit the request metric - `inst-ing-metric-if`
   1. [x] - `p1` - **RETURN** canonical `InvalidArgument`; increment `admitted_metric_violations_total` by closed validation surface - `inst-ing-metric`
7. [x] - `p1` - Materialize tenant scope from `tenant_id`, combine it with the mapped additional subjects, and exclude attribution and authenticated principal data from Policy input `{request, resource, arbitration}` - `inst-ing-map`
8. [x] - `p1` - **RETURN** the validated, authorized, catalogue-mapped request to the evaluation pipeline; this validation runs at ingress of every debit, reserve, preview, and each batch item, and its failures are canonical errors, never `Decision::Denied` (`cpt-cf-quota-enforcement-fr-contract-validation`) - `inst-ing-forward`

## 3. Processes / Business Logic (CDSL)

### Catalogue Bootstrap and Consistency Set

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-catalog-bootstrap`

**Input**: deployment configuration naming the projections configured for evaluation, `TypesRegistryClient`

**Output**: the published immutable `ProjectionContractCatalog`, or failed gear bootstrap

**Steps**:
1. [x] - `p1` - API: register missing QE-owned definitions idempotently through `TypesRegistryClient`: the abstract bases `gts.cf.core.qe.subj.v1~`, `gts.cf.core.qe.res.v1~`, `gts.cf.core.qe.request.v1~`, `gts.cf.core.qe.constraint.v1~`, the scope-discriminator type `gts.cf.core.qe.scope.v1~`, and its P1 well-known instances `gts.cf.core.qe.scope.v1~cf.core.qe.user.v1` and `gts.cf.core.qe.scope.v1~cf.core.qe.tenant.v1`; registration touches only QE-owned definitions, and QE seeds no platform-wide subject instances - `inst-cat-bases`
2. [x] - `p1` - API: resolve every configured subject/resource projection and metric request contract from `types-registry` into a candidate catalogue; `types-registry` remains authoritative and the catalogue is only a validated local snapshot - `inst-cat-resolve`
3. [x] - `p1` - Verify each configured projection/request contract is concrete and genuinely derived from its QE base - `inst-cat-concrete`
4. [x] - `p1` - Verify every admitted metric reference resolves to a registered instance of the metric base; a narrowed `x-gts-ref` is a pattern-level prefix match only, so QE owns both checks - `inst-cat-metric`
5. [x] - `p1` - Read each projection's registry-validated effective `scope` trait, compare the `GtsInstanceId` values directly, and reject any `(metric, scope)` pair admitted by two configured projections; scope is never inferred from the type-id name segment - `inst-cat-unique`
6. [x] - `p1` - Resolve exactly one request contract per admitted metric and verify its attached constraint contract is registered, concrete, and derived from the constraint base - `inst-cat-contract-pair`
7. [x] - `p1` - **IF** the configured catalogue is incompatible with any active Quota, read as the distinct `(metric, projection_type)` pairs the storage contract reports (`read_active_projection_bindings`) - `inst-cat-compat-if`
   1. [x] - `p1` - Fail gear bootstrap; increment `contract_validation_failures_total` with the `bootstrap` surface where emission completes before process exit; the current projection version stays active and no activation procedure exists in P1 - `inst-cat-compat`
8. [x] - `p1` - **IF** any consistency check fails - `inst-cat-fail-if`
   1. [x] - `p1` - Fail gear bootstrap and serve nothing; increment `contract_validation_failures_total` with the `bootstrap` surface where emission completes before process exit; this step extends `cpt-cf-quota-enforcement-flow-gear-bootstrap` from the foundation feature - `inst-cat-fail`
9. [x] - `p1` - Build the authoritative reverse index from each `(metric, scope)` to its configured subject projection set out of the admitted-metric declarations - `inst-cat-index`
10. [x] - `p1` - **RETURN** publish the immutable catalogue to the Gateway only after all checks pass; it is immutable for the process lifetime, with no runtime refresh or breaking-version activation in P1; registered projections outside the configured catalogue remain discoverable but are rejected by Quota and Policy writes - `inst-cat-publish`

### PDP-Authorized Subject Mapping

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-subject-resolution`

**Input**: authenticated service principal, caller-supplied `tenant_id`, additional `subjects[{kind,id}]`, metric,
optional resource, and the published `ProjectionContractCatalog`

**Output**: the complete set of `(projection_type, subject_id)` pairs for the request, or a canonical error before
evaluation

**Steps**:
1. [x] - `p1` - Reject an unauthenticated service principal, empty `tenant_id`, empty subject id, duplicate kind, or a tenant kind repeated in `subjects` - `inst-res-shape`
2. [x] - `p1` - Send the complete supplied tenant/subject/metric/resource tuple to PDP and fail closed unless it is authorized for the authenticated service principal - `inst-res-authz`
3. [x] - `p1` - Materialize the tenant-scope subject from `tenant_id` - `inst-res-tenant`
4. [x] - `p1` - **FOR EACH** materialized or additional subject - `inst-res-each`
   1. [x] - `p1` - Resolve `(metric, kind)` through the catalogue's unique reverse index - `inst-res-map`
   2. [x] - `p1` - **IF** the kind is unknown or does not admit the metric - `inst-res-invalid-if`
      1. [x] - `p1` - **RETURN** canonical `InvalidArgument` before evaluation - `inst-res-invalid`
   3. [x] - `p1` - Append `(projection_type, id)`; the caller cannot select the concrete projection - `inst-res-append`
5. [x] - `p1` - **RETURN** the complete mapped set for applicable-Quota lookup - `inst-res-return`

### Catalogue-Membership Check for Quota and Policy Writes

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-catalog-membership`

**Input**: a projection reference carried by a Quota or Policy write, the published `ProjectionContractCatalog`, and
for Quota writes the Quota's metric

**Output**: pass, or a canonical rejection before persistence

**Steps**:
1. [x] - `p1` - Resolve the referenced projection against the published catalogue and the registry snapshot taken by the write path; the evaluation hot path never runs this check - `inst-mem-lookup`
2. [x] - `p1` - **IF** the reference is unregistered, abstract, non-subject, of unknown scope, or not derived from the QE base - `inst-mem-invalid-if`
   1. [x] - `p1` - **RETURN** canonical rejection per `cpt-cf-quota-enforcement-fr-subject-type-registry`; increment `contract_validation_failures_total` with the closed write surface (`arbitration` or `policy_pair`) - `inst-mem-invalid`
3. [x] - `p1` - **IF** the reference is registered but outside the configured catalogue, including a registered replacement version - `inst-mem-nonconf-if`
   1. [x] - `p1` - **RETURN** rejection with `PROJECTION_NOT_RESOLVABLE`; increment `contract_validation_failures_total` with the closed write surface; P1 provides no projection alias or Quota/counter migration operation - `inst-mem-nonconf`
4. [x] - `p1` - **IF** a Quota write names a metric the referenced projection does not admit - `inst-mem-metric-if`
   1. [x] - `p1` - **RETURN** canonical rejection; increment `admitted_metric_violations_total` with the closed write surface - `inst-mem-metric`
5. [x] - `p1` - **RETURN** pass; the quota-lifecycle and resolution-policy-engine features invoke this check inside their write paths, which own the writes themselves - `inst-mem-pass`

## 4. States (CDSL)

### ProjectionContractCatalog State Machine

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-state-projection-catalog`

**States**: Candidate, Published, Rejected

**Initial State**: Candidate

**Transitions**:
1. [ ] - `p1` - **FROM** Candidate **TO** Published **WHEN** the full bootstrap consistency set of `cpt-cf-quota-enforcement-algo-catalog-bootstrap` passes and the catalogue is compatible with every active Quota and Policy - `inst-catst-publish`
2. [ ] - `p1` - **FROM** Candidate **TO** Rejected **WHEN** any consistency check of `cpt-cf-quota-enforcement-algo-catalog-bootstrap` fails or the catalogue is incompatible with an active Quota or Policy; gear bootstrap fails and serves nothing (`cpt-cf-quota-enforcement-fr-contract-validation`) - `inst-catst-reject`

Published is terminal for the process lifetime: the catalogue is immutable, P1 has no runtime refresh, and changing the
configured projection set means a new bootstrap of a new process. Individual contracts have no QE-side lifecycle;
`types-registry` owns their versions.

## 5. Definitions of Done

### Abstract Base and Scope Registration

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-base-registration`

The system **MUST** register the abstract bases `gts.cf.core.qe.subj.v1~`, `gts.cf.core.qe.res.v1~`,
`gts.cf.core.qe.request.v1~`, and `gts.cf.core.qe.constraint.v1~`, the scope-discriminator type
`gts.cf.core.qe.scope.v1~`, and its P1
well-known instances `user` and `tenant` idempotently through `TypesRegistryClient` at bootstrap, touching only
QE-owned definitions, seeding no platform-wide subject instances, and exposing no QE registration endpoint.

**Implements**:
- `cpt-cf-quota-enforcement-flow-owner-projection-publication`
- `cpt-cf-quota-enforcement-algo-catalog-bootstrap`

**Constraints**: `cpt-cf-quota-enforcement-constraint-types-registry-delegation`

**Touches**:
- API: `TypesRegistryClient` (platform `types-registry-sdk`, obtained from ClientHub)
- Entities: `SubjectScope`

### Projection Contract Catalogue

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-projection-catalog`

The system **MUST** build the immutable process-local `ProjectionContractCatalog` at bootstrap, enforce the complete
consistency set (concreteness and base derivation, admitted-metric registration and derivation, `(metric, scope)`
uniqueness via the effective `scope` trait, exactly one request contract per metric and its attached constraint
contract, compatibility with active Quotas and Policies), build the authoritative `(metric, scope)` and metric-request
indexes, fail gear bootstrap on
any mismatch, and publish the catalogue to the Gateway only after all checks pass. Contracts are registry-resident;
no QE-side table exists.

**Implements**:
- `cpt-cf-quota-enforcement-algo-catalog-bootstrap`
- `cpt-cf-quota-enforcement-state-projection-catalog`

**Constraints**: `cpt-cf-quota-enforcement-constraint-types-registry-delegation`

**Touches**:
- API: `TypesRegistryClient` (bootstrap only; never on the request path)
- Entities: `ProjectionContractCatalog`, `SubjectProjectionContract`, `ResourceProjectionContract`,
  `MetricRequestContract`, `ConstraintContract`

### Gateway Ingress Contract Validation

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-ingress-validation`

The system **MUST** extend the foundation Gateway with fail-closed validation of debit, reserve, preview, and each batch
item: explicit `tenant_id`, additional `{kind,id}` subjects, one operation-level `metadata` object
never defaulted to `{}`, admitted metric, and optional resource projection. It **MUST** reject malformed public request
shape before PDP, authorize the complete supplied tuple through PDP, and only then map each `(metric, kind)`
and validate request/resource contracts through the process-local catalogue. Failures **MUST** map
to the appropriate canonical error and **MUST NOT** be encoded as `Decision::Denied`. Consumer DTOs contain no
`caller_type` or concrete subject projection type.

**Implements**:
- `cpt-cf-quota-enforcement-flow-ingress-validation`

**Constraints**: `cpt-cf-quota-enforcement-constraint-types-registry-delegation`

**Touches**:
- API: evaluation request contract fields on debit, reserve, preview, and batch-item DTOs (no new endpoint)
- Entities: `ProjectionContractCatalog`

### Subject Attribution Mapping

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-subject-resolution`

The system **MUST** shape-check caller-supplied `tenant_id` and additional `{kind,id}` subjects, PDP-authorize the
complete tenant/subject/metric/resource tuple, materialize tenant scope from `tenant_id`, and then map every
`(metric, kind)` through the catalogue's unique index. It rejects duplicate, unknown, or unadmitted kinds and returns the complete
`(projection_type, subject_id)` set consumed by `EvaluationOrchestrator`. The caller cannot select a concrete
projection; no resolver trait exists.

**Implements**:
- `cpt-cf-quota-enforcement-algo-subject-resolution`

**Constraints**: `cpt-cf-quota-enforcement-constraint-types-registry-delegation`

**Touches**:
- API: consumer attribution fields on every subject-based evaluation DTO
- Entities: `SecurityContext` authenticated service principal, `ProjectionContractCatalog`

### Catalogue-Membership Check for Writes

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-catalog-membership`

The system **MUST** validate every projection reference carried by a Quota or Policy write against the published
catalogue and the registry snapshot taken by the write path, before persistence: a reference that is unregistered,
abstract, non-subject, of unknown scope, or not derived from the QE base **MUST** receive a canonical rejection per
`cpt-cf-quota-enforcement-fr-subject-type-registry`; a reference that is registered but outside the configured
catalogue **MUST** receive `PROJECTION_NOT_RESOLVABLE`; a Quota write naming a metric the referenced projection does
not admit **MUST** be rejected. The evaluation hot path **MUST NOT** run this check.

**Implements**:
- `cpt-cf-quota-enforcement-algo-catalog-membership`

**Constraints**: `cpt-cf-quota-enforcement-constraint-types-registry-delegation`

**Touches**:
- API: no new endpoint (the quota-lifecycle and resolution-policy-engine write paths invoke this check)
- Entities: `ProjectionContractCatalog`

### Contract-Validation Telemetry

- [x] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-contract-validation-telemetry`

The system **MUST** emit `contract_validation_failures_total` (rejected contract instances by closed validation
surface/reason) and `admitted_metric_violations_total` (projection/metric incompatibilities by closed validation
surface) through the foundation telemetry conventions. The ingress surface increments in
`cpt-cf-quota-enforcement-flow-ingress-validation`; the Quota/Policy write and bootstrap surfaces increment in the
rejection and failure steps of `cpt-cf-quota-enforcement-algo-catalog-membership` and
`cpt-cf-quota-enforcement-algo-catalog-bootstrap`. Metric names, projection types, and caller attribution **MUST
NOT** appear as label values; permitted dimensions are the closed `surface` and `reason` enums from PRD §5.16.

**Implements**:
- `cpt-cf-quota-enforcement-flow-ingress-validation`
- `cpt-cf-quota-enforcement-algo-catalog-membership`
- `cpt-cf-quota-enforcement-algo-catalog-bootstrap`

**Constraints**: `cpt-cf-quota-enforcement-constraint-types-registry-delegation`

**Touches**:
- API: platform observability stack (`tracing` + `toolkit` `otel` feature, per the foundation telemetry conventions)
- Entities: gear-specific counters per PRD §5.16

## 6. Acceptance Criteria

- [ ] Repeated bootstraps register the abstract bases, the scope-discriminator type, and its `user`/`tenant` instances
  exactly once; registration is idempotent and touches only QE-owned definitions
- [ ] Bootstrap fails, and the gear serves nothing, when a configured projection is unregistered, abstract, or not
  derived from the QE base; when an admitted metric reference is unregistered or is not an instance of the metric base;
  when two configured projections admit the same `(metric, scope)` pair; or when an admitted metric lacks exactly one
  concrete request contract with a valid attached constraint contract
- [ ] Bootstrap fails when the configured catalogue is incompatible with any active Quota or Policy; no activation of a
  breaking projection version occurs in P1
- [ ] Two distinct scopes of one owner admitting the same metric pass bootstrap (scope cascade stays expressible)
- [ ] A write or preview request with a duplicate, unknown, or unadmitted subject kind, omitted `metadata`, contract
  violation, or inadmissible metric receives canonical `InvalidArgument` with a stable field-level reason,
  never `Decision::Denied`, and increments `contract_validation_failures_total` or
  `admitted_metric_violations_total` accordingly; absent `metadata` is never defaulted to `{}`
- [ ] With `types-registry` unreachable, ingress validation still succeeds against the process-local catalogue: no
  evaluation write or preview request path performs a registry call (verified by fault injection on the registry
  dependency); Quota and Policy write paths, which snapshot contracts from the registry, are outside this claim
- [ ] Consumer DTOs contain no `caller_type`; the authenticated service principal comes from `SecurityContext`, while
  CEL input is limited to `{request, resource, arbitration}`
- [ ] With caller-supplied `tenant_id=T`, subject `{kind=user,id=U}`, and both owner scopes configured, mapping yields
  exactly `{(owner-tenant-projection, T), (owner-user-projection, U)}` without a caller-selected projection
- [ ] The complete supplied attribution tuple is sent to PDP; changing `tenant_id`, subjects, metric, or resource to an
  unauthorized target produces `PermissionDenied` before evaluation and no storage mutation
- [ ] Error precedence is stable: malformed public request shape returns `InvalidArgument` before PDP; a
  structurally valid unauthorized tuple returns `PermissionDenied` before catalogue or contract lookup; an authorized
  tuple with an unknown/unadmitted kind or invalid contract payload returns `InvalidArgument`
- [ ] The catalogue-membership check rejects a Quota or Policy reference to a registered but non-configured projection
  with `PROJECTION_NOT_RESOLVABLE`, and rejects a Quota reference whose projection does not admit the Quota's metric
- [ ] Metrics scrape shows no metric name, projection type, or caller attribution as a label value on
  `contract_validation_failures_total` or `admitted_metric_violations_total`

## 7. Additional Context (optional)

- **Compatibility is split**: the active-Quota check is complete against the storage contract
  (`QuotaEnforcementStoragePluginV1::read_active_projection_bindings()`, the distinct `(metric, projection_type)`
  pairs of active Quotas) and verified against the in-memory storage double; the storage plugin's database query and
  its integration tests are tracked by the quota-lifecycle feature. Policy compatibility is tracked by the
  resolution-policy-engine feature, since a Policy version carries no contract reference yet. Until both land, the
  `ProjectionContractCatalog` state machine, `cpt-cf-quota-enforcement-dod-projection-catalog`, and the feature
  status stay open.
- **Resolution rule**: the gear resolves each selected contract with the `gts` library the registry validates with,
  loading the type, its parent chain, and every type its `gts://` references reach into a local store; the resolved
  body keeps its Draft-07 `$schema` and its `x-gts-ref` constraints, which the compiled contract asserts at ingress
  together with the GTS id formats. Request contracts are discovered from the registry listing under
  `gts.cf.core.qe.request.v1~` and filtered to the metrics the configured projections admit, so a contract nobody
  configured cannot fail this deployment's bootstrap through a broken reference graph.
- **Operator prerequisites**: every metric an owner projection admits must be registered as an instance of the
  metric base, and in the deployed profile the registry's registration policy must admit the `cf.core.qe`
  definitions; otherwise bootstrap fails with `metric_unregistered` or a definition conflict. A consistency-set
  failure is reported under the `catalog` dependency (health code `qe_catalog_unavailable`), a registry that does
  not answer under `types_registry`.
- **ADR dependency**: the primary decision record, `cpt-cf-quota-enforcement-adr-projection-contracts` (ADR-0007), has
  status accepted. This document restates only what the feature needs; the ADR stays the authority on the
  contract shapes.
- **Rollout / rollback**: the catalogue is built per process at bootstrap and is immutable; changing the configured
  projection set is a redeploy. A failed consistency set keeps the previous healthy replicas serving (the new process
  never becomes ready). Runtime catalogue refresh is a P2 item in DESIGN §4.3 and is not specified here.
- **Test layering**: consistency-set and catalogue-mapping checks get unit tests against catalogue fixtures; ingress
  validation, PDP authorization, and registry-fault behavior get integration tests; attribution spoofing is an
  adversarial integration test.
- **Bootstrap compatibility query (2026-09-09)**: the storage half of `cpt-cf-quota-enforcement-dod-projection-catalog`
  landed with the quota-lifecycle feature: `read_active_projection_bindings()` is a distinct query over the reference
  plugin's `qe_quotas` table, tested on SQLite and PostgreSQL. The Policy half still lands with the
  resolution-policy-engine feature, so the Definition of Done stays open.
- **Non-applicable review domains**: UX/accessibility is not applicable; there is no user-facing surface. No QE-side
  persistence is added, so no schema or retention concerns arise here. Contract documents must not carry secrets, per
  the ADR shape-only rule.
