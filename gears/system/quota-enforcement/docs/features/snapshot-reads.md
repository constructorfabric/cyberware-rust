<!-- Created: 2026-08-26 by Constructor Tech -->

# Feature: Quota Snapshot Reads

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-featstatus-snapshot-reads-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p1` - `cpt-cf-quota-enforcement-feature-snapshot-reads`

<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Unified Snapshot Read](#unified-snapshot-read)
  - [Consumer-Backed Self-Service Snapshot](#consumer-backed-self-service-snapshot)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Per-Quota Snapshot State Assembly](#per-quota-snapshot-state-assembly)
  - [Snapshot Pagination](#snapshot-pagination)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Unified Snapshot Endpoint](#unified-snapshot-endpoint)
  - [Per-Quota State Contract](#per-quota-state-contract)
  - [S2S Self-Service Boundary](#s2s-self-service-boundary)
  - [Read-Only Guarantee](#read-only-guarantee)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Additional Context (optional)](#7-additional-context-optional)

<!-- /toc -->

## 1. Feature Context

### 1.1 Overview

Implements the engine-agnostic per-Quota state read: the unified S2S `POST /v1/quota-enforcement/snapshot` endpoint
serving consumer and manager/operator cases from one PDP-scoped request shape. Returns, for every
applicable Quota, its current per-Quota state (cap, consumed or in-flight, remaining, period boundary, metadata,
validity window, `currently_within_window`), with PDP-scoped filtering, cursor pagination, and no Policy attribution
or aggregate headline numbers.

### 1.2 Purpose

Dashboards, billing systems, and self-service surfaces need to observe quota state without exercising the evaluation
pipeline. This feature is that read surface. It composes the Quota records of the quota-lifecycle feature with the
counter rows of the consumption-operations feature into a point-in-time Quota Snapshot, and it keeps the read honest:
no aggregate "headline" cap or balance is computed, because under cascade, split, or attribute-gated Engines no
single number is universally meaningful. Callers that need an admission verdict for a specific operation use the
read-only Decision Preview of the consumption-operations feature instead
(`cpt-cf-quota-enforcement-fr-evaluate-preview`).

**Scope**: `POST /v1/quota-enforcement/snapshot` for explicit caller-supplied tenant/subject/metric filters with cursor pagination
(operator-configured page size, default 100 entries per page); the per-Quota state contract shared by all three
cases; S2S consumer reads used by product backends to render self-service views; manager/operator reads with explicit
targets; lazy period-row materialization as the single read-path write
exception (the semantics are owned by the consumption-operations feature and invoked here); the
`QuotaEnforcementClientV1::snapshot` SDK method.

**Out of scope**: admission verdicts (served by `evaluate_preview` in the consumption-operations feature), the Quota
CRUD read endpoints (`GET /v1/quota-enforcement/quotas*`, owned by the quota-lifecycle feature), Policy attribution
reads (the Policy-read API of the resolution-policy-engine feature), end-user access to QE (products expose views
through their backend), subject attribution mapping (projection-contracts feature), and period-rollover semantics (`cpt-cf-quota-enforcement-algo-period-rollover`,
consumption-operations feature).

**Requirements**: `cpt-cf-quota-enforcement-fr-quota-snapshot-read`,
`cpt-cf-quota-enforcement-fr-bulk-quota-snapshot-read`,
`cpt-cf-quota-enforcement-fr-end-user-quota-snapshot-read`

**Principles**: none of its own (read-only composition over earlier features, per DECOMPOSITION §2.8); the feature
applies the foundation admission and tenant-isolation machinery unchanged.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-quota-enforcement-actor-quota-reader` | Reads Quota Snapshot data for one or many `(subject, metric)` pairs within the PDP-authorized scope |
| `cpt-cf-quota-enforcement-actor-quota-consumer` | Uses its authenticated service principal and explicit PDP-authorized attribution to render remaining-amount views through its own backend |
| `cpt-cf-quota-enforcement-actor-quota-manager` | Calls the same endpoint with an explicit target under its management PDP scope |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) (§5.10 Quota Snapshot Read API, §1.4 glossary "Quota Snapshot",
  §5.12 tenant isolation)
- **Design**: [DESIGN.md](../DESIGN.md) (REST endpoint inventory, `QuotaEnforcementClientV1` SDK trait, storage-plugin
  snapshot-read group, §3.3 error model, `cpt-cf-quota-enforcement-seq-end-user-snapshot`)
- **Decomposition**: [DECOMPOSITION.md](../DECOMPOSITION.md) (§2.8)
- **ADR**: [ADR-0007 Declarative GTS projection contracts](../ADR/0007-cpt-cf-quota-enforcement-adr-projection-contracts.md)
  (`cpt-cf-quota-enforcement-adr-projection-contracts`, status: accepted; the owner user/tenant projections the
  end-user restriction derives from follow that ADR)
- **Dependencies**: `cpt-cf-quota-enforcement-feature-quota-lifecycle` (Quota records, the validity-window
  computation), `cpt-cf-quota-enforcement-feature-consumption-operations` (counter rows, period materialization),
  plus transitively `cpt-cf-quota-enforcement-feature-projection-contracts` (subject resolution for the end-user
  case) and `cpt-cf-quota-enforcement-feature-foundation` (admission, `AccessScope` composition, tenant isolation)

## 2. Actor Flows (CDSL)

**Use cases**: `cpt-cf-quota-enforcement-usecase-end-user-quota-snapshot-read` (the operator-side single and bulk
reads are PRD requirement surfaces without a dedicated use case)

### Unified Snapshot Read

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-snapshot-read`

**Actor**: `cpt-cf-quota-enforcement-actor-quota-reader` (and
`cpt-cf-quota-enforcement-actor-quota-consumer` for the single-subject case)

**Success Scenarios**:
- A single `(subject, metric)` filter returns the engine-agnostic list of every applicable Quota with its per-Quota
  state
- N filter pairs return the combined result set, cursor-paginated when it exceeds the operator-configured page size
- A filter matching no Quota returns an empty page (not an error)

**Error Scenarios**:
- Malformed public filter shape: canonical `InvalidArgument` before PDP
- PDP denial or PDP unreachability at admission: canonical error from the foundation admission flow, fail-closed
- Rows outside the caller's tenant or `AccessScope` are unreachable by construction; they are absent, not errors
- Every error is a `Problem` envelope; a snapshot read never produces a Decision shape

**Steps**:
1. [ ] - `p1` - Caller sends `POST /v1/quota-enforcement/snapshot` with a `SnapshotRequest` carrying target
   `tenant_id`, `1..N` logical `{kind,id,metric}` filters, and page parameters; platform authentication has attached
   `SecurityContext` - `inst-snp-request`
2. [ ] - `p1` - Treat single and bulk as degenerate cases of the one request shape: `subjects.len() == 1` realises
   `cpt-cf-quota-enforcement-fr-quota-snapshot-read`, `subjects.len() >= 1` realises
   `cpt-cf-quota-enforcement-fr-bulk-quota-snapshot-read`; reject malformed public target/filter shape before PDP;
   there is no separate REST path - `inst-snp-shape`
3. [ ] - `p1` - PDP authorizes the complete structurally valid explicit target against the authenticated principal - `inst-snp-authz`
4. [ ] - `p1` - QE maps each authorized `(metric, kind)` through the catalogue, then calls
   `bulk_read_quota_snapshot(pairs, page)` under the returned `AccessScope` per
   `cpt-cf-quota-enforcement-algo-pdp-constraint-composition` (foundation), reading the `quotas` rows and their
   counter rows; the read is read-only (I3) - `inst-snp-read`
5. [ ] - `p1` - **IF** a consumption Quota's current-period counter row is missing or its boundary has passed
   (`now() >= period_end`) - `inst-snp-lazy-if`
   1. [ ] - `p1` - Materialize the new period row lazily per `cpt-cf-quota-enforcement-algo-period-rollover`
      (consumption-operations feature), the single permitted I3 write exception on this read path; the snapshot then
      reflects the fresh period - `inst-snp-lazy`
6. [ ] - `p1` - Assemble each returned Quota's state per `cpt-cf-quota-enforcement-algo-snapshot-state` - `inst-snp-assemble`
7. [ ] - `p1` - Apply `cpt-cf-quota-enforcement-algo-snapshot-pagination` to the result set - `inst-snp-page`
8. [ ] - `p1` - **RETURN** `200` with the `PageResult<QuotaSnapshot>` page; an empty applicable set returns an empty
   page; the SDK path is `QuotaEnforcementClientV1::snapshot(req)` returning `PageResult<QuotaSnapshot>` - `inst-snp-return`

### Consumer-Backed Self-Service Snapshot

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-flow-end-user-snapshot`

Realises `cpt-cf-quota-enforcement-seq-end-user-snapshot`.

**Actor**: `cpt-cf-quota-enforcement-actor-quota-consumer` (its backend serves the end-user UI; the end user never
calls Quota Enforcement directly)

**Success Scenarios**:
- The backend requests every applicable Quota under the explicit user and tenant attribution, in the
  same per-Quota state shape as the operator-side call
- An end user with no applicable Quotas receives an empty page, which Quota Manager renders as "no quotas apply"

**Error Scenarios**:
- Malformed public target shape: canonical `InvalidArgument` before PDP
- The backend supplies a tenant or user outside its service principal's PDP scope: canonical `PermissionDenied`

**Steps**:
1. [ ] - `p1` - The consuming backend calls `POST /v1/quota-enforcement/snapshot` with its authenticated service
   principal plus explicit target `tenant_id` and user `{kind,id}` - `inst-eus-request`
2. [ ] - `p1` - Reject malformed public target shape before PDP - `inst-eus-target-shape`
3. [ ] - `p1` - PDP authorizes the complete structurally valid target tuple; QE then maps the authorized tenant and user
   kinds through the catalogue - `inst-eus-fix`
4. [ ] - `p1` - **IF** any target lies outside the backend's authorized scope - `inst-eus-broaden-if`
   1. [ ] - `p1` - **RETURN** canonical `PermissionDenied` before storage - `inst-eus-broaden`
5. [ ] - `p1` - DB: the gateway and storage pipeline are otherwise identical to
   `cpt-cf-quota-enforcement-flow-snapshot-read`, including the lazy period materialization and the read-only
   guarantee - `inst-eus-read`
6. [ ] - `p1` - Return every applicable active Quota under that scope, and only Quotas applicable to that set:
   Quotas that govern a subject's consumption are transparent to that subject, and no per-Quota or per-key
   invisibility primitive exists - `inst-eus-all`
7. [ ] - `p1` - The per-Quota state shape is identical to the operator-side call and carries no Policy attribution;
   the applicable-Quotas filter is the only difference between the two cases
   (`cpt-cf-quota-enforcement-fr-end-user-quota-snapshot-read`) - `inst-eus-shape`
8. [ ] - `p1` - **RETURN** the `PageResult<QuotaSnapshot>` page for Quota Manager to render; the end-user
   authentication and rate-limit story is owned by Quota Manager, not by QE - `inst-eus-return`

## 3. Processes / Business Logic (CDSL)

### Per-Quota Snapshot State Assembly

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-snapshot-state`

**Input**: a matched `Quota` row and its counter row (`Counter` allocation or consumption shape), the server clock

**Output**: one `QuotaSnapshot`: the point-in-time per-Quota view defined by the PRD glossary

**Steps**:
1. [ ] - `p1` - Populate the identity fields: `quota_id`, the subject reference, the metric, and the quota type
   (`allocation` / `consumption`) (`cpt-cf-quota-enforcement-fr-quota-snapshot-read`) - `inst-sst-identity`
2. [ ] - `p1` - Populate `cap` (numeric, or `null` for unbounded Quotas per the quota-lifecycle cap semantics), the
   current consumed amount (or the in-flight amount for allocation type), and `remaining` (numeric, or `null` when
   `cap` is `null`) - `inst-sst-amounts`
3. [ ] - `p1` - Populate `enforcement_mode` (per `cpt-cf-quota-enforcement-fr-enforcement-mode`, owned by the
   quota-lifecycle feature) - `inst-sst-mode`
4. [ ] - `p1` - **IF** the quota type is `consumption` - `inst-sst-period-if`
   1. [ ] - `p1` - Populate the period boundary and the next reset timestamp from the persisted counter-row
      boundary - `inst-sst-period`
5. [ ] - `p1` - **ELSE** - `inst-sst-noperiod-else`
   1. [ ] - `p1` - Report "no period"; allocation Quotas have no period dimension - `inst-sst-noperiod`
6. [ ] - `p1` - Populate the Quota's `metadata` map in full, subject to PDP scoping
   (`cpt-cf-quota-enforcement-fr-quota-metadata` return rule, owned by the quota-lifecycle feature) - `inst-sst-metadata`
7. [ ] - `p1` - Populate `validity_window` (when set) plus the server-computed boolean `currently_within_window`
   per `cpt-cf-quota-enforcement-algo-validity-window` (quota-lifecycle feature), so callers render expiry state
   without recomputing the comparison - `inst-sst-window`
8. [ ] - `p1` - Exclude every Quota Resolution Policy attribution field: no `policy_id`, `policy_version`, `scope`,
   `engine_id`, `engine_config`, and no summary or content hash thereof; Policy-attribution callers route through
   `cpt-cf-quota-enforcement-fr-evaluate-preview` or the Policy-read API of
   `cpt-cf-quota-enforcement-fr-quota-resolution-policy-versioning` - `inst-sst-noattr`
9. [ ] - `p1` - Compute no aggregate "headline" cap or balance across Quotas, in the response body or in any
   per-page summary; the response is the engine-agnostic per-Quota list only - `inst-sst-noheadline`
10. [ ] - `p1` - **RETURN** the `QuotaSnapshot`; the view is point-in-time and carries no freshness promise beyond
    the storage consistency the foundation contract provides (I10): a snapshot can be invalidated by concurrent
    operations, and admission questions belong to `evaluate_preview` - `inst-sst-return`

### Snapshot Pagination

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-algo-snapshot-pagination`

**Input**: the PDP-scoped result set for the requested filter pairs, the operator-configured page size (default 100
entries per page), the optional continuation cursor from a prior page

**Output**: one `PageResult<QuotaSnapshot>` page with a continuation cursor while results remain

**Steps**:
1. [ ] - `p1` - **IF** the result set exceeds the operator-configured page size - `inst-spg-limit-if`
   1. [ ] - `p1` - Truncate the page at the page size and attach a continuation cursor
      (`cpt-cf-quota-enforcement-fr-bulk-quota-snapshot-read`) - `inst-spg-limit`
2. [ ] - `p1` - Resume from a supplied cursor so repeated calls walk the full result set to exhaustion
   (cursor-based continuation) - `inst-spg-resume`
3. [ ] - `p1` - Apply the same pagination contract to all three cases; single-subject responses that fit one page
   return no continuation cursor - `inst-spg-uniform`
4. [ ] - `p1` - **RETURN** the page; pagination state lives in the cursor, and the endpoint keeps no server-side
   session (the storage primitive `bulk_read_quota_snapshot(pairs, page)` takes the page argument per call) - `inst-spg-return`

## 4. States (CDSL)

This feature introduces no entity and no lifecycle state of its own: it is a read-only composition over state owned
elsewhere. The Quota lifecycle states it reads are defined by `cpt-cf-quota-enforcement-state-quota-lifecycle`
(quota-lifecycle feature), and the consumption period states its lazy materialization step touches are defined by
`cpt-cf-quota-enforcement-state-consumption-period` (consumption-operations feature). Neither is re-specified here.

## 5. Definitions of Done

### Unified Snapshot Endpoint

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-snapshot-endpoint`

The system **MUST** deliver the unified `POST /v1/quota-enforcement/snapshot` endpoint on `QuotaEnforcementService`
(`cpt-cf-quota-enforcement-component-quota-enforcement-service`, shared with the consumption-operations feature per
DECOMPOSITION §2.12) and the SDK method `QuotaEnforcementClientV1::snapshot(req: SnapshotRequest)` returning
`PageResult<QuotaSnapshot>`. The endpoint **MUST** accept `1..N` `(subject, metric)` filter pairs, treating single
(`subjects.len() == 1`) and bulk (`subjects.len() >= 1`) as degenerate cases of one request shape, **MUST** paginate
per `cpt-cf-quota-enforcement-algo-snapshot-pagination` (operator-configured page size, default 100 entries per
page, cursor continuation), and **MUST** apply the caller's `AccessScope` on every storage read. As a pure-CRUD
surface it **MUST** return a `Problem` envelope on every error and never a Decision shape.

**Implements**:
- `cpt-cf-quota-enforcement-flow-snapshot-read`
- `cpt-cf-quota-enforcement-algo-snapshot-pagination`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`,
`cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: `POST /v1/quota-enforcement/snapshot`; `QuotaEnforcementClientV1::snapshot`;
  storage `bulk_read_quota_snapshot(pairs, page)`
- DB: reads `quotas`, `quota_allocation_counters`, and `quota_consumption_counters`; no new tables
  (DECOMPOSITION §2.8)
- Entities: `SnapshotRequest`, `QuotaSnapshot`, `PageResult<QuotaSnapshot>`

### Per-Quota State Contract

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-snapshot-state-contract`

The system **MUST** return, for each applicable Quota, exactly the PRD §5.10 per-Quota state: `quota_id`, subject
reference, metric, quota type, `cap` (numeric or `null`), current consumed (or in-flight for allocation),
`remaining` (numeric, or `null` when `cap` is `null`), `enforcement_mode`, the period boundary and next reset
timestamp for consumption types (or "no period" for allocation types), the full `metadata` map subject to PDP
scoping, and `validity_window` plus the server-computed `currently_within_window` per
`cpt-cf-quota-enforcement-algo-validity-window`. The response **MUST NOT** carry Quota Resolution Policy attribution
in any form and **MUST NOT** compute an aggregate headline cap or balance. The state shape **MUST** be identical
across the operator-side and end-user cases.

**Implements**:
- `cpt-cf-quota-enforcement-algo-snapshot-state`

**Constraints**: `cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: the snapshot endpoint response body (no new route)
- DB: reads the `quotas` row fields and counter amounts listed above
- Entities: `QuotaSnapshot`

### S2S Self-Service Boundary

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-end-user-scope`

The system **MUST** keep snapshot access S2S. A consuming backend supplies explicit tenant/user attribution. QE **MUST**
reject malformed public target shape before PDP; PDP **MUST** authorize the complete structurally valid target against
its authenticated service principal before catalogue mapping or storage. End users never authenticate to QE directly.
For an authorized target, QE **MUST** return every applicable
Quota (no per-Quota invisibility filtering); cross-user or cross-tenant targets are rejected. End-user authentication,
presentation, and rate limiting stay with the consuming product.

**Implements**:
- `cpt-cf-quota-enforcement-flow-end-user-snapshot`

**Constraints**: `cpt-cf-quota-enforcement-constraint-security-context`

**Touches**:
- API: `POST /v1/quota-enforcement/snapshot` (no separate REST path)
- DB: the same read-only tables as the unified endpoint
- Entities: `QuotaSnapshot`, `SecurityContext` service principal, `AccessScope`

### Read-Only Guarantee

- [ ] `p1` - **ID**: `cpt-cf-quota-enforcement-dod-snapshot-read-only`

The snapshot read path **MUST** write no persistent state (I3): no counter mutation, no idempotency record, no
operation-log entry, and no outbox event of its own. The single permitted exception is the lazy materialization of a
fresh consumption period row when the read observes a crossed boundary or a missing row; that materialization
follows `cpt-cf-quota-enforcement-algo-period-rollover` (consumption-operations feature) unchanged, including its
`consumed = 0` and threshold-marker reset (I13) rules and its event emission, and is not re-specified here.

**Implements**:
- `cpt-cf-quota-enforcement-flow-snapshot-read`

**Constraints**: `cpt-cf-quota-enforcement-constraint-toolkit`

**Touches**:
- API: no new endpoint (a property of the snapshot read path)
- DB: `quota_consumption_counters` (lazy materialization only; all other access read-only)
- Entities: `Counter` (consumption)

## 6. Acceptance Criteria

- [ ] A single-pair request (`subjects.len() == 1`) returns the engine-agnostic list of every applicable Quota for
  that `(subject, metric)` pair with the full PRD §5.10 field set; no aggregate headline cap or balance appears
  anywhere in the response
- [ ] `remaining` is `null` exactly when `cap` is `null`; an allocation Quota reports its in-flight amount and
  "no period"; a consumption Quota reports its current-period consumed amount, the period boundary, and the next
  reset timestamp
- [ ] No snapshot response contains `policy_id`, `policy_version`, `scope`, `engine_id`, `engine_config`, or any
  summary or content hash of Policy state (schema-level response test on both the operator and end-user cases)
- [ ] A bulk request whose result set exceeds the operator-configured page size (default 100) returns
  cursor-paginated pages, and walking the cursors to exhaustion yields every matching row exactly once
- [ ] A product backend supplying `tenant_id=T` and user `{kind=user,id=U}` receives every active Quota applicable to
  the mapped owner user/tenant projections and nothing else; an unauthorized cross-tenant or cross-user target is denied
- [ ] The operator-side and end-user responses for the same Quota are byte-identical in per-Quota state shape; the
  applicable-Quotas filter is the only observable difference (contract test)
- [ ] A filter matching no Quota returns `200` with an empty page, not an error; every failure is a `Problem`
  envelope and never a Decision shape
- [ ] An operator-side bulk read never returns rows outside the caller's tenant or `AccessScope`; such rows are
  absent, not errors (PDP-scoped filtering through `SecureConn` scope compilation)
- [ ] After a snapshot read, storage holds no new idempotency record, operation-log entry, outbox event, or counter
  mutation, except the writes the consumption-operations rollover rules perform on a crossed boundary: the new
  consumption period row with `consumed = 0` and `highest_crossed_threshold_pct = NULL`, the closing-period
  settlement update, and the `period-rollover` outbox event when settlement completes
- [ ] A Quota whose `validity_end` has passed is still returned while active, with `currently_within_window = false`
  and its stored `validity_window` intact
- [ ] Metrics scrape shows no new gear-specific instrument from this feature and no high-cardinality label
  (`tenant_id`, `subject_id`, `quota_id`, metric, projection type, caller attribution) introduced by the snapshot
  path; the endpoint is covered by the framework HTTP baseline

## 7. Additional Context (optional)

- **Component sharing**: `cpt-cf-quota-enforcement-component-quota-enforcement-service` is established by the
  consumption-operations feature and extended here with the snapshot surface, per DECOMPOSITION §2.12. The DESIGN
  component description enumerates the eight enforcement operations and does not yet name `snapshot`; this document
  follows the DECOMPOSITION assignment, and the component-description alignment is a tracked upstream DESIGN item.
- **Single-read subject language**: single and bulk reads use the same explicit logical subject filters under PDP
  scope; QE maps scope kinds to concrete projections before storage lookup.
- **I3 exception naming**: DESIGN invariant I3 names `read_quota_snapshot` as the carrier of the lazy-materialization
  exception, while this endpoint is served by `bulk_read_quota_snapshot(pairs, page)`. DECOMPOSITION §2.8 grants
  "lazy period-row materialization as the single read-path write exception" to this feature's read path, so this
  document treats the exception as a property of the storage snapshot-read group; the invariant-text alignment is a
  tracked upstream DESIGN item.
- **`currently_within_window` hand-off**: the quota-lifecycle feature owns validity-window storage and the
  `cpt-cf-quota-enforcement-algo-validity-window` computation and surfaces the boolean on its Quota read endpoints;
  this feature surfaces the same two fields on Quota Snapshots per the PRD §5.10 grant, computing the boolean
  through the same algorithm.
- **ADR dependency**: the owner user/tenant projections behind the end-user restriction follow
  `cpt-cf-quota-enforcement-adr-projection-contracts` (ADR-0007, status accepted).
- **No caching or freshness promise**: the snapshot is a point-in-time view. PRD and DESIGN grant no read cache and
  no staleness bound for this surface, so none is promised; the read observes committed state per the storage
  contract's strong consistency within tenant scope (I10, foundation feature).
- **NFR ownership**: DECOMPOSITION §2.8 allocates no NFR to this feature; latency, throughput, and availability
  verification for the gear belongs to the consumption-operations feature, and no separate read-path budget is
  promised here.
- **Rust contract notes**: the read path is async over the Tokio-based storage plugin and holds no in-process lock;
  pagination state lives entirely in the caller-held cursor, so service instances stay freely replicable with no
  shared mutable state. `QuotaSnapshot` and `PageResult<QuotaSnapshot>` are `Send + Sync` compatible plain data
  crossing the handler boundary.
- **Rollout / rollback**: the feature is stateless above the storage plugin; rollout is a rolling update under the
  same schema major version, and disabling the route removes the read surface without touching stored state.
- **Test layering**: state assembly (field population, `remaining` derivation, window boolean) and pagination get
  unit tests; PDP scoping, the end-user restriction, the broadening rejection, and the read-only guarantee get
  integration tests against the storage plugin, including the boundary-crossing materialization case.
- **Non-applicable review domains**: UX/accessibility is not applicable; the end-user rendering surface is owned by
  Quota Manager. Data protection: responses carry Quota Metadata, which is Platform Operational Data per PRD §6.2
  and must not contain PII by the quota-lifecycle write-path rule; the read path adds no further handling.
